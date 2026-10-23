// SPDX-FileCopyrightText: 2026 deploy-rx contributors
//
// SPDX-License-Identifier: MPL-2.0

//! Exercise the public preparation API and real CLI with controlled subprocesses.
//! Barriers, rather than elapsed-time assertions, require remote/local builds
//! and copies to overlap. Reordered JSON, repeated installables and selected
//! dynamic outputs must still map to the original deployment order. Failures
//! must drain all started children and prevent every activation.
#![cfg(unix)]

use deploy::push::{build_and_push_profiles, PushProfileData};
use serde_json::json;
use std::io::{self, Read, Write};
use std::num::NonZeroU16;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn main() {
    // Cargo builds just this test target. Its temporary nix/ssh symlinks run
    // these fixtures directly, without a shell or a separately installed binary.
    let program = std::env::args_os().next().unwrap();
    let fixture = match Path::new(&program).file_name().and_then(|s| s.to_str()) {
        Some("nix") => Some(nix_fixture as fn() -> io::Result<i32>),
        Some("ssh") => Some(ssh_fixture as fn() -> io::Result<i32>),
        Some("nom") => Some(nom_fixture as fn() -> io::Result<i32>),
        _ => None,
    };
    if let Some(fixture) = fixture {
        std::env::set_current_dir(std::env::var_os("FIXTURE").unwrap()).unwrap();
        let code = fixture().unwrap_or_else(|error| {
            eprintln!("fixture failed: {error}");
            92
        });
        std::process::exit(code);
    }

    // Match the ignored-test invocation used by the repository's checks.
    if std::env::args().any(|arg| arg == "--ignored") {
        return;
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    if std::env::var_os("DEPLOY_RX_TEST_NOM").is_some()
        && std::env::var_os("DEPLOY_RX_TEST_DRV").is_none()
    {
        let output = Command::new("nix-instantiate")
            .args([
                "--expr",
                r#"builtins.derivation {
            name = "display-shared-workload"; system = builtins.currentSystem;
            builder = "/does-not-exist";
        }"#,
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        std::env::set_var(
            "DEPLOY_RX_TEST_DRV",
            String::from_utf8(output.stdout).unwrap().trim(),
        );
    }
    if std::env::var_os("DISPLAY_DEMO").is_some() {
        deploy::init_logger(false, None, &deploy::LoggerType::Deploy).unwrap();
        runtime.block_on(concurrent_preparation_preserves_mapping_and_drains_failures(true));
        return;
    }
    cli_checks_monitor_lifecycle();
    cli_many_monitored_profiles();
    runtime.block_on(concurrent_preparation_preserves_mapping_and_drains_failures(false));
    cli_activation_and_connection_identity(false);
    println!("concurrent_preparation: ok (8 preparation scenarios, signed CLI activation and connection isolation)");
    if std::env::var_os("DEPLOY_RX_TEST_NOM").is_some() {
        runtime.block_on(concurrent_preparation_preserves_mapping_and_drains_failures(true));
        cli_activation_and_connection_identity(true);
        println!("real_nom_preparation: ok (8 preparation scenarios, signed CLI activation and connection isolation)");
        for scenario in ["none", "remote", "resize"] {
            real_nom_in_pty(scenario);
        }
        for mode in ["early", "hang", "blocked"] {
            std::env::set_var("NOM_FAILURE", mode);
            runtime.block_on(concurrent_preparation_preserves_mapping_and_drains_failures(true));
            println!("nom_{mode}: ok (Nix result preserved, all children reaped)");
        }
        std::env::remove_var("NOM_FAILURE");
    }
}

fn real_nom_in_pty(scenario: &str) {
    let (mut master, mut slave) = (-1, -1);
    let size = libc::winsize {
        ws_row: 50,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        },
        0
    );
    let mut master = unsafe { std::fs::File::from_raw_fd(master) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave) };
    assert_ne!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
        -1
    );
    assert_ne!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
        -1
    );
    let sync = tempfile::tempdir().unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    if scenario == "resize" {
        command.env("DISPLAY_RESIZE", sync.path());
    }
    let mut child = command
        .env(
            "DISPLAY_DEMO",
            if scenario == "resize" {
                "none"
            } else {
                scenario
            },
        )
        .env("RUST_LOG", "info")
        .stdin(Stdio::null())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave)
        .spawn()
        .unwrap();
    drop(command);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut terminal = vt100::Parser::new(
        NonZeroU16::new(50).unwrap(),
        NonZeroU16::new(120).unwrap(),
        0,
    );
    let mut transcript = Vec::new();
    let mut saw_three_trees = false;
    let mut resized = false;
    let mut restored = false;
    let mut pages = std::collections::HashSet::new();
    let mut buffer = [0; 8192];
    loop {
        match master.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                transcript.extend_from_slice(&buffer[..count]);
                terminal.process(&buffer[..count]);
                let screen = terminal.screen().contents();
                let three_trees = screen.contains("[running] Build local batch")
                    && screen.lines().any(|line| {
                        line.starts_with("[running] Build connection") && line.contains("remote-a")
                    })
                    && screen.lines().any(|line| {
                        line.starts_with("[running] Build connection") && line.contains("remote-b")
                    })
                    && screen.matches("Dependency Graph").count() >= 3;
                saw_three_trees |= three_trees;
                if scenario == "resize" {
                    if !resized && three_trees {
                        resize_terminal(&master, &mut terminal, 18, 64);
                        resized = true;
                    } else if resized && !restored {
                        for page in 1..=3 {
                            if screen.contains(&format!("page {page}/3"))
                                && screen.matches("Dependency Graph").count() == 1
                            {
                                pages.insert(page);
                            }
                        }
                        if pages.len() == 3 {
                            resize_terminal(&master, &mut terminal, 50, 120);
                            restored = true;
                        }
                    } else if restored && three_trees {
                        std::fs::write(sync.path().join("resized"), []).unwrap();
                    }
                }
            }
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(error) => panic!("PTY read failed: {}", error),
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("PTY scenario timed out");
        }
    }
    assert!(child.wait().unwrap().success());
    let transcript = String::from_utf8_lossy(&transcript);
    assert!(
        saw_three_trees,
        "three independent live nom trees were not visible"
    );
    assert!(!transcript.contains("do-not-change-title"));
    assert!(transcript.contains("[completed] Build"));
    if scenario == "remote" {
        assert!(transcript.contains("[FAILED] Build"));
    }
    if scenario == "resize" {
        assert_eq!(pages.len(), 3, "not all compact pages were rendered");
        assert!(
            sync.path().join("resized").exists(),
            "three trees did not return after resizing"
        );
    }
    println!("real_nom_pty_{scenario}: ok (three isolated trees, completion and failure states)");
}

fn resize_terminal(master: &std::fs::File, parser: &mut vt100::Parser, rows: u16, cols: u16) {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) },
        0
    );
    parser.screen_mut().set_size(
        NonZeroU16::new(rows).unwrap(),
        NonZeroU16::new(cols).unwrap(),
    );
}

fn nom_fixture() -> io::Result<i32> {
    if std::env::args().any(|arg| arg == "--version")
        && std::env::var("NOM_FAILURE").as_deref() == Ok("spawn")
    {
        // Availability probing succeeds, but the subsequent spawn must fail.
        // argv[0] is just "nom"; remove the fixture from its actual PATH entry.
        let bin = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .next()
            .unwrap();
        std::fs::remove_file(bin.join("nom"))?;
        event("nom.probe.succeeded")?;
        return Ok(0);
    }
    if std::env::args().any(|arg| arg == "--version")
        && (std::env::var_os("RESOURCE_CASE").is_some()
            || std::env::var_os("NOM_FAILURE").is_some())
    {
        return Ok(0);
    }
    if std::env::args().any(|arg| arg == "--json") {
        record_pid(&format!("nom.{}", std::process::id()))?;
        if std::env::var_os("RESOURCE_CASE").is_some()
            || std::env::var("NOM_FAILURE").as_deref() == Ok("consume")
        {
            io::stdin().read_to_end(&mut Vec::new())?;
            return Ok(0);
        }
        match std::env::var("NOM_FAILURE").as_deref() {
            Ok("early") => return Ok(73),
            Ok("blocked") => {
                event("nom.blocked")?;
                std::thread::sleep(Duration::from_secs(60));
                return Ok(75);
            }
            Ok("hang") => {
                io::stdin().read_to_end(&mut Vec::new())?;
                std::thread::sleep(Duration::from_secs(60));
                return Ok(74);
            }
            _ => (),
        }
    }
    Err(
        Command::new(std::env::var_os("DEPLOY_RX_TEST_NOM").unwrap())
            .args(std::env::args_os().skip(1))
            .exec(),
    )
}

fn append_line(path: &str, line: &str) -> io::Result<()> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(format!("{line}\n").as_bytes())
}

fn event(name: &str) -> io::Result<()> {
    append_line("events", name)?;
    std::fs::write(name, [])
}

fn wait_for(name: &str) -> io::Result<()> {
    // The injected hung monitor deliberately consumes the production EOF
    // timeout before local copies start. Keep that test's barrier longer.
    let deadline = Instant::now()
        + Duration::from_secs(
            if std::env::var_os("NOM_FAILURE").is_some()
                || std::env::var_os("DISPLAY_RESIZE").is_some()
            {
                20
            } else {
                5
            },
        );
    while !Path::new(name).exists() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("barrier timed out: {name}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn record_pid(name: &str) -> io::Result<()> {
    std::fs::write(format!("pid.{name}"), std::process::id().to_string())
}

fn print_file(path: &str) -> io::Result<()> {
    io::stdout().write_all(&std::fs::read(path)?)
}

fn nix_fixture() -> io::Result<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |arg: &str| args.iter().any(|item| item == arg);
    let failure = std::env::var("FAIL").unwrap();
    if has("flake") && has("check") {
        record_pid("check")?;
        event("check.start")?;
        // More than a pipe's capacity: a monitor that never reads must trigger
        // the write timeout, not just the post-EOF wait timeout.
        flood_monitor()?;
        println!("discarded check stdout");
        event("check.end")?;
        return Ok(if failure == "flake-check" { 42 } else { 0 });
    } else if has("eval") {
        if has("--json") {
            print_file("deployment.json")?;
        } else {
            println!("true");
        }
    } else if has("show-derivation") {
        if has("/nix/store/resolve-fail") {
            wait_for("resolve.slow.start")?;
            event("resolve.fail")?;
            return Ok(45);
        }
        record_pid("resolve.slow")?;
        event("resolve.slow.start")?;
        std::thread::sleep(Duration::from_millis(200));
        event("resolve.slow.end")?;
        println!("{}", json!({"/nix/store/resolved.drv": {}}));
    } else if has("copy") && has("--derivation") {
        // Remote derivation transfer succeeds before that target's build.
    } else if has("store") && has("ls") {
        if has("/nix/store/remote-a1-profile") {
            event("check.a1")?;
        } else if has("/nix/store/remote-a2-profile") {
            event("check.a2")?;
        }
        if failure == "check" {
            return Ok(43);
        }
        println!(
            "{}",
            json!({"entries": {"deploy-rx-activate": {}, "activate-rs": {}}})
        );
    } else if has("sign-paths") || (has("store") && has("sign")) {
        let closure = args.last().unwrap();
        let name = Path::new(closure).file_name().unwrap().to_str().unwrap();
        let key_flag = if has("sign-paths") {
            "-k"
        } else {
            "--key-file"
        };
        let key = args.windows(2).find(|pair| pair[0] == key_flag).unwrap();
        assert_eq!(std::fs::read_to_string(&key[1])?, "fixture signing key");
        record_pid(&format!("sign.{name}"))?;
        event(&format!("sign.{name}.start"))?;
        if (failure == "sign-local" && has("sign-paths"))
            || (failure == "sign-remote" && name == "remote-a1-profile")
        {
            event(&format!("sign.{name}.failed"))?;
            return Ok(46);
        }
        event(&format!("sign.{name}.end"))?;
    } else if has("build") {
        let name = if has("/nix/store/remote-a1.drv^out") {
            "a1"
        } else if has("/nix/store/remote-a2.drv^out") {
            "a2"
        } else if has("/nix/store/remote-b.drv^out") {
            "b"
        } else if has("/nix/store/remote-c.drv^out") {
            "c"
        } else {
            "local"
        };
        let address = args.windows(2).find(|pair| pair[0] == "--store");
        if let Some(address) = address {
            let options = shlex::split(&std::env::var("NIX_SSHOPTS").unwrap()).unwrap();
            check_master(&options)?;
            std::fs::write(
                format!("connection.{name}.json"),
                json!({"address": address[1], "options": options}).to_string(),
            )?;
        }
        record_pid(&format!("build.{name}"))?;
        std::fs::write(format!("build.{name}.args"), args.join("\n"))?;
        event(&format!("build.{name}.start"))?;
        eprintln!("streamed build {name}");
        if std::env::var_os("RESOURCE_CASE").is_some() {
            event(&format!("build.{name}.end"))?;
            return print_file("result.a1.json").map(|()| 0);
        }
        let monitored = has("internal-json");
        let activity = u64::from(std::process::id()) << 32;
        if monitored {
            flood_monitor()?;
            // Identical drv path across independent hosts is deliberate: a
            // merged nom session would conflate these builds.
            eprintln!(
                "@nix {}",
                json!({"action":"start", "id":activity, "level":3,
                "type":105, "text":"building shared-workload", "fields":[
                    std::env::var("DEPLOY_RX_TEST_DRV").unwrap(), "", 1, 1
                ], "parent":0})
            );
            for index in 0..200 {
                eprintln!("ordinary {name} diagnostic {index}: 中文 λ \u{1b}]0;do-not-change-title\u{7}ok");
            }
        }
        if failure == "none" {
            if std::env::var_os("CONNECTION_CASE").is_some() {
                // A hostname-only, user-only, or options-only connection key
                // serializes one of these independent chains and times out.
                if name != "a2" {
                    for peer in ["a1", "b", "c"] {
                        wait_for(&format!("build.{peer}.start"))?;
                    }
                }
            } else {
                match name {
                    "a1" => {
                        wait_for("build.b.start")?;
                        wait_for("build.local.start")?;
                    }
                    "b" => wait_for("copy.local.start")?,
                    _ => (),
                }
            }
        }
        if let Some(sync) = std::env::var_os("DISPLAY_RESIZE") {
            if matches!(name, "a1" | "local") {
                wait_for(Path::new(&sync).join("resized").to_str().unwrap())?;
            }
        }
        let scale = if std::env::var_os("DISPLAY_DEMO").is_some() {
            15
        } else if monitored {
            4
        } else {
            1
        };
        std::thread::sleep(Duration::from_millis(
            scale * if name == "b" { 300 } else { 100 },
        ));
        event(&format!("build.{name}.end"))?;
        if matches!(
            (failure.as_str(), name),
            ("remote", "a1") | ("local", "local")
        ) {
            if monitored {
                eprintln!(
                    "@nix {}",
                    json!({"action":"msg", "level":0, "msg":format!("error: {name} build failed")})
                );
            }
            return Ok(42);
        }
        if monitored {
            eprintln!("@nix {}", json!({"action":"stop", "id":activity}));
        }
        print_file(&format!("result.{name}.json"))?;
    } else if has("copy") {
        let name = if has("ssh://root@local") {
            "local"
        } else if has("ssh://root@other") {
            "other"
        } else if has("ssh://root@remote-a") {
            "a"
        } else {
            return Err(io::Error::other(format!("unexpected copy: {args:?}")));
        };
        record_pid(&format!("copy.{name}"))?;
        std::fs::write(format!("copy.{name}.args"), args.join("\n"))?;
        std::fs::write(
            format!("copy.{name}.ssh"),
            std::env::var("NIX_SSHOPTS").unwrap(),
        )?;
        event(&format!("copy.{name}.start"))?;
        eprintln!("copy {name}: transferring closures");
        if failure == "none" && name == "local" {
            wait_for("copy.other.start")?;
        }
        std::thread::sleep(Duration::from_millis(
            if std::env::var_os("DISPLAY_DEMO").is_some() {
                1500
            } else {
                100
            },
        ));
        event(&format!("copy.{name}.end"))?;
        if failure == "copy" && name == "local" {
            return Ok(44);
        }
    } else {
        return Err(io::Error::other(format!("unexpected nix: {args:?}")));
    }
    Ok(0)
}

fn ssh_fixture() -> io::Result<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "ControlMaster=yes") {
        let path = &args.windows(2).find(|pair| pair[0] == "-S").unwrap()[1];
        let name = Path::new(path).file_name().unwrap().to_str().unwrap();
        record_pid(&format!("master.start.{name}"))?;
        std::fs::write(path, "active")?;
        append_line("masters", &json!({"path":path, "args":args}).to_string())?;
        event(&format!("master.start.{name}"))?;
        return Ok(0);
    }
    if args.iter().any(|arg| arg == "-O") {
        let path = args
            .iter()
            .find_map(|arg| arg.strip_prefix("ControlPath="))
            .unwrap();
        let name = Path::new(path).file_name().unwrap().to_str().unwrap();
        record_pid(&format!("master.stop.{name}"))?;
        std::fs::remove_file(path)?;
        event(&format!("master.stop.{name}"))?;
        return Ok(0);
    }
    check_master(&args)?;
    let mut request = String::new();
    io::stdin().read_line(&mut request)?;
    let parsed: deploy::remote_protocol::BootstrapRequest = serde_json::from_str(&request)?;
    let (closure, operation) = match &parsed.operation {
        deploy::remote_protocol::RemoteOperation::Deploy(request) => (&request.closure, "activate"),
        deploy::remote_protocol::RemoteOperation::Revoke(request) => {
            let name = Path::new(&request.closure)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap();
            assert_eq!(request.rollback.session_id, format!("receipt-{name}"));
            (&request.closure, "revoke")
        }
    };
    let name = Path::new(closure).file_name().unwrap().to_str().unwrap();
    append_line("events", &format!("{operation}:{}", args.join(" ")))?;
    record_pid(&format!("{operation}.{name}"))?;
    event(&format!("{operation}.{name}.start"))?;
    append_line("activations", request.trim_end())?;
    std::thread::sleep(Duration::from_millis(20));
    event(&format!("{operation}.{name}.end"))?;
    let mode = std::env::var("ACTIVATION_FAILURE").unwrap_or_else(|_| "none".into());
    let failed = mode != "none" && operation == "activate" && name == "remote-c-profile"
        || mode == "revoke-fail" && operation == "revoke" && name == "remote-a1-profile";
    let receipt = (mode != "none" && operation == "activate" && !failed).then(|| {
        json!({
            "session_id": format!("receipt-{name}"), "previous_session": null,
            "previous_link": null, "previous_target": null, "previous_running_target": null,
            "expected_link": null, "created_generation": null, "boot": false, "test": false
        })
    });
    println!(
        "{}",
        json!({"Finished": {
            "ok": !failed, "rolled_back": false,
            "message": if failed { format!("injected {operation} failure") } else { "done".into() },
            "rollback": receipt
        }})
    );
    Ok(0)
}

fn check_master(options: &[String]) -> io::Result<()> {
    if let Some(pair) = options.windows(2).find(|pair| pair[0] == "-S") {
        assert_eq!(std::fs::read_to_string(&pair[1])?, "active");
    }
    Ok(())
}

fn assert_masters_closed(fixture: &Path, expected: usize) {
    let masters = std::fs::read_to_string(fixture.join("masters")).unwrap();
    assert_eq!(masters.lines().count(), expected);
    let events = std::fs::read_to_string(fixture.join("events")).unwrap();
    for line in masters.lines() {
        let master: serde_json::Value = serde_json::from_str(line).unwrap();
        let path = Path::new(master["path"].as_str().unwrap());
        assert!(!path.exists(), "master {} was not closed", path.display());
        assert!(
            !path.parent().unwrap().exists(),
            "control directory was not removed"
        );
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            events
                .lines()
                .filter(|event| *event == format!("master.stop.{name}"))
                .count(),
            1
        );
    }
}

fn before(events: &str, first: &str, second: &str) {
    let lines: Vec<_> = events.lines().collect();
    let a = lines.iter().position(|&line| line == first).unwrap();
    let b = lines.iter().position(|&line| line == second).unwrap();
    assert!(a < b, "expected {} before {}:\n{}", first, second, events);
}

fn flood_monitor() -> io::Result<()> {
    if std::env::var("NOM_FAILURE").as_deref() == Ok("blocked") {
        let line = format!(
            "@nix {}\n",
            json!({
                "action": "result", "id": 1, "type": 100, "fields": ["x".repeat(4096)]
            })
        );
        for _ in 0..512 {
            io::stderr().write_all(line.as_bytes())?;
        }
        event("nix.flood.drained")?;
    }
    Ok(())
}

fn fixture_bin(root: &Path, names: &[&str]) -> PathBuf {
    let bin = root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let executable = std::env::current_exe().unwrap();
    for name in names {
        symlink(&executable, bin.join(name)).unwrap();
    }
    bin
}

fn cli_checks_monitor_lifecycle() {
    for (mode, fail_check) in [
        ("consume", false),
        ("consume", true),
        ("early", false),
        ("early", true),
        ("hang", false),
        ("blocked", false),
        ("spawn", false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let bin = fixture_bin(root.path(), &["nix", "nom"]);
        std::fs::write(root.path().join("deployment.json"), r#"{"nodes":{}}"#).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_deploy"));
        command
            .env("PATH", &bin)
            .env("FIXTURE", root.path())
            .env("NOM_FAILURE", mode)
            .env("FAIL", if fail_check { "flake-check" } else { "none" })
            .env("RUST_LOG", "info")
            .env_remove("LOCAL_KEY")
            .arg("--no-review-changes")
            .arg(".")
            .stdin(Stdio::null());
        let started = Instant::now();
        let output = command.output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.success(),
            !fail_check && mode != "spawn",
            "{stderr}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "{} did not settle promptly",
            mode
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("discarded check stdout"));
        assert!(!output.stderr.contains(&0x1b));
        let events = std::fs::read_to_string(root.path().join("events")).unwrap_or_default();
        if mode == "spawn" {
            assert!(events.contains("nom.probe.succeeded"));
        } else {
            assert!(events.contains("check.end"), "{}", stderr);
            if mode == "blocked" {
                assert!(events.contains("nom.blocked"));
                assert!(events.contains("nix.flood.drained"));
            }
        }
        assert_reaped(root.path());
    }
    println!("cli_checks_monitor: ok (Nix outcomes, spawn failure, early exit, EOF hang and input backpressure)");
}

fn cli_many_monitored_profiles() {
    let root = tempfile::tempdir().unwrap();
    let bin = fixture_bin(root.path(), &["nix", "nom", "ssh"]);
    let profiles: serde_json::Map<String, serde_json::Value> = (0..96)
        .map(|index| {
            (
                format!("p{index}"),
                json!({
                    "path": "/nix/store/placeholder", "drvPath": "/nix/store/remote-a1.drv"
                }),
            )
        })
        .collect();
    std::fs::write(
        root.path().join("deployment.json"),
        json!({
            "user":"root", "sshUser":"root", "nodes":{"stress":{
                "hostname":"remote-a", "remoteBuild":true, "profiles":profiles
            }}
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        root.path().join("result.a1.json"),
        json!([{
            "drvPath":"/nix/store/remote-a1.drv", "outputs":{"out":"/nix/store/remote-a1-profile"}
        }])
        .to_string(),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_deploy"));
    command
        .env("PATH", &bin)
        .env("FIXTURE", root.path())
        .env("FAIL", "none")
        .env("RESOURCE_CASE", "1")
        .env_remove("NOM_FAILURE")
        .env_remove("LOCAL_KEY")
        .args(["--skip-checks", "--no-review-changes", ".#stress"])
        .stdin(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 64,
                rlim_max: 64,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = std::fs::read_to_string(root.path().join("activations")).unwrap();
    assert_eq!(requests.lines().count(), 96);
    assert_masters_closed(root.path(), 1);
    assert_reaped(root.path());
    println!("monitor_descriptors: ok (96 sequential builds with RLIMIT_NOFILE=64)");
}

fn cli_activation_and_connection_identity(build_tree: bool) {
    // Remote-only profiles permit successful CLI activation without writing
    // /nix/store, including inside Nix's read-only build sandbox.
    for (multiplex, spawn_failure, rollback) in [
        (false, false, "none"),
        (true, false, "none"),
        (true, true, "none"),
        (true, false, "rollback"),
        (true, false, "revoke-fail"),
    ] {
        if spawn_failure && !build_tree {
            continue;
        }
        let root = tempfile::tempdir().unwrap();
        let bin = fixture_bin(root.path(), &["nix", "ssh", "nom"]);
        let path = std::env::join_paths(
            std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let options_a = ["-o", "ProxyCommand=ssh jump host -W %h:%p"];
        let options_b = ["-o", "ProxyCommand=ssh other jump -W %h:%p"];
        // Each independent connection differs from root/options_a in one field.
        // Explicit target order deliberately differs from build completion order.
        let profiles = [
            ("r2", "b", "deploy", options_a),
            ("r1", "a1", "root", options_a),
            ("r4", "c", "root", options_b),
            ("r3", "a2", "root", options_a),
        ];
        let mut nodes = serde_json::Map::new();
        for (node, name, user, options) in profiles {
            nodes.insert(
                node.into(),
                json!({
                    "hostname": "remote-a", "sshUser": user, "sshOpts": options,
                    "tags": ["edge"], "remoteBuild": true,
                    "profiles": {"system": {
                        "path": "/nix/store/placeholder", "tags": ["service"],
                        "drvPath": format!("/nix/store/remote-{name}.drv")
                    }}
                }),
            );
            std::fs::write(
                root.path().join(format!("result.{name}.json")),
                json!([{
                    "drvPath": format!("/nix/store/remote-{name}.drv"),
                    "outputs": {"out": format!("/nix/store/remote-{name}-profile")}
                }])
                .to_string(),
            )
            .unwrap();
        }
        std::fs::write(
            root.path().join("deployment.json"),
            json!({
                "tags": ["fleet"], "user": "root", "nodes": nodes
            })
            .to_string(),
        )
        .unwrap();
        let key = root.path().join("test.key");
        std::fs::write(&key, "fixture signing key").unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_deploy"));
        command
            .env("PATH", &path)
            .env("FIXTURE", root.path())
            .env("FAIL", if spawn_failure { "nom-spawn" } else { "none" })
            .env("LOCAL_KEY", &key)
            .env("RUST_LOG", "info")
            .env("CONNECTION_CASE", "1")
            .env("ACTIVATION_FAILURE", rollback)
            .env_remove("NOM_FAILURE")
            .args([
                "--skip-checks",
                "--no-review-changes",
                "--tag",
                "fleet",
                "--tag",
                "edge",
                "--tag",
                "service",
            ]);
        if !build_tree {
            command.arg("--no-build-tree");
        }
        if !multiplex {
            command.arg("--no-ssh-multiplexing");
        }
        command.arg("--targets");
        if spawn_failure {
            // A single build avoids racing the destructive availability probe.
            // Isolate PATH so unlinking the fixture cannot find another nom.
            command
                .env("NOM_FAILURE", "spawn")
                .env("PATH", root.path().join("bin"))
                .arg(".#r1.system");
        } else {
            command.args(profiles.iter().map(|(node, ..)| format!(".#{node}.system")));
        }
        let output = command.stdin(Stdio::null()).output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.success(),
            !spawn_failure && rollback == "none",
            "{stderr}"
        );
        assert!(!stderr.contains("barrier timed out"), "{}", stderr);
        let events = std::fs::read_to_string(root.path().join("events")).unwrap();
        if spawn_failure {
            assert!(events.contains("nom.probe.succeeded"));
            assert!(stderr.contains("No such file or directory"), "{}", stderr);
            assert!(!events.contains("activate:"), "{}", events);
        } else {
            let requests = std::fs::read_to_string(root.path().join("activations")).unwrap();
            let requests: Vec<deploy::remote_protocol::BootstrapRequest> = requests
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let closures: Vec<_> = requests
                .iter()
                .filter_map(|request| match &request.operation {
                    deploy::remote_protocol::RemoteOperation::Deploy(request) => {
                        Some(request.closure.as_str())
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                closures,
                profiles
                    .iter()
                    .take(if rollback == "none" { 4 } else { 3 })
                    .map(|(_, name, ..)| format!("/nix/store/remote-{name}-profile"))
                    .collect::<Vec<_>>()
            );
            for (index, (_, name, user, options)) in profiles.iter().enumerate() {
                before(
                    &events,
                    &format!("build.{name}.end"),
                    &format!("sign.remote-{name}-profile.start"),
                );
                // Every chain must finish signing before the first activation.
                before(
                    &events,
                    &format!("sign.remote-{name}-profile.end"),
                    "activate.remote-b-profile.start",
                );
                if index > 0 && (rollback == "none" || index < 3) {
                    before(
                        &events,
                        &format!("activate.remote-{}-profile.end", profiles[index - 1].1),
                        &format!("activate.remote-{name}-profile.start"),
                    );
                }
                let connection: serde_json::Value = serde_json::from_slice(
                    &std::fs::read(root.path().join(format!("connection.{name}.json"))).unwrap(),
                )
                .unwrap();
                assert_eq!(connection["address"], format!("ssh-ng://{user}@remote-a"));
                let actual: Vec<String> =
                    serde_json::from_value(connection["options"].clone()).unwrap();
                assert_eq!(&actual[..2], options);
                assert_eq!(actual.len(), if multiplex { 4 } else { 2 });
            }
            before(&events, "sign.remote-a1-profile.end", "build.a2.start");
            if rollback != "none" {
                let revoked: Vec<_> = requests
                    .iter()
                    .filter_map(|request| match &request.operation {
                        deploy::remote_protocol::RemoteOperation::Revoke(request) => {
                            Some(request.closure.as_str())
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    revoked,
                    [
                        "/nix/store/remote-a1-profile",
                        "/nix/store/remote-b-profile"
                    ]
                );
                before(
                    &events,
                    "activate.remote-c-profile.end",
                    "revoke.remote-a1-profile.start",
                );
                before(
                    &events,
                    "revoke.remote-a1-profile.end",
                    "revoke.remote-b-profile.start",
                );
                assert!(stderr.contains("injected activate failure"), "{}", stderr);
                if rollback == "revoke-fail" {
                    assert!(stderr.contains("injected revoke failure"), "{}", stderr);
                    assert!(
                        stderr.contains("revoked 1 previous profile(s)"),
                        "{}",
                        stderr
                    );
                }
            }
        }
        if multiplex {
            assert_masters_closed(root.path(), if spawn_failure { 1 } else { 3 });
        }
        assert_reaped(root.path());
    }
}

async fn concurrent_preparation_preserves_mapping_and_drains_failures(build_tree: bool) {
    // One test owns this process's PATH; child CLI runs get isolated environments.
    let root = tempfile::tempdir().unwrap();
    let bin = fixture_bin(
        root.path(),
        if build_tree {
            &["nix", "ssh", "nom"]
        } else {
            &["nix", "ssh"]
        },
    );
    let original_path = std::env::var_os("PATH").unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin.clone()).chain(std::env::split_paths(&original_path)),
    )
    .unwrap();
    std::env::set_var("PATH", &path);
    for scenario in [
        "none",
        "cli-success",
        "resolve",
        "remote",
        "local",
        "check",
        "copy",
        "sign-local",
        "sign-remote",
    ] {
        if let Ok(demo) = std::env::var("DISPLAY_DEMO") {
            if scenario != demo {
                continue;
            }
        }
        if std::env::var_os("NOM_FAILURE").is_some() && scenario != "none" {
            continue;
        }
        let cli_success = scenario == "cli-success";
        let store_profiles = std::env::var_os("DEPLOY_RX_TEST_PROFILE");
        if cli_success && store_profiles.is_none() {
            continue;
        }
        let failure = if cli_success { "none" } else { scenario };
        let fixture = root.path().join(scenario);
        std::fs::create_dir(&fixture).unwrap();
        std::env::set_var("FIXTURE", &fixture);
        std::env::set_var("FAIL", failure);
        let key = fixture.join("test.key");
        std::fs::write(&key, "fixture signing key").unwrap();
        std::env::set_var("LOCAL_KEY", &key);
        let profile_root = if cli_success {
            Path::new(store_profiles.as_ref().unwrap())
        } else {
            fixture.as_path()
        };
        let local = profile_root.join("local-profile");
        let dev = profile_root.join("dev-profile");
        let shared = profile_root.join("shared-profile");
        if !cli_success {
            for closure in [&local, &dev, &shared] {
                std::fs::create_dir(closure).unwrap();
                std::fs::write(closure.join("deploy-rx-activate"), "").unwrap();
                std::fs::write(closure.join("activate-rs"), "").unwrap();
            }
        }
        let targets = [
            ("r1", "remote-a", "/nix/store/remote-a1.drv", "out", true),
            ("l1", "local", "/nix/store/local.drv", "out", false),
            ("r2", "remote-b", "/nix/store/remote-b.drv", "out", true),
            // Another node on the same connection must follow r1.
            ("r3", "remote-a", "/nix/store/remote-a2.drv", "out", true),
            ("l2", "local", "/nix/store/outer.drv^out", "dev", false),
            // Same output, different node and destination.
            ("l3", "other", "/nix/store/local.drv", "out", false),
            ("l4", "remote-a", "/nix/store/shared.drv", "out", false),
        ];
        let mut nodes = serde_json::Map::new();
        for (node, hostname, drv, output, remote) in targets {
            nodes.insert(
                node.into(),
                json!({
                    "hostname": hostname,
                    "tags": ["edge", "fleet"],
                    "sshOpts": ["-o", "ProxyCommand=ssh jump host -W %h:%p"],
                    "profiles": {"system": {
                        "path": "/nix/store/placeholder", "drvPath": drv,
                        "outputName": output, "remoteBuild": remote,
                        "tags": ["service"]
                    }}
                }),
            );
        }
        nodes.insert(
            "excluded".into(),
            json!({
                "hostname": "must-not-be-contacted",
                "profiles": {"system": {
                    "path": "/nix/store/must-not-be-built",
                    "tags": ["service"]
                }}
            }),
        );
        let mut config = json!({"sshUser":"root", "user":"root", "tags":["fleet"], "nodes":nodes});
        if failure == "resolve" {
            for (node, path) in [("r1", "resolve-fail"), ("l1", "resolve-slow")] {
                let profile = &mut config["nodes"][node]["profiles"]["system"];
                profile.as_object_mut().unwrap().remove("drvPath");
                profile["path"] = json!(format!("/nix/store/{path}"));
            }
        }
        std::fs::write(fixture.join("deployment.json"), config.to_string()).unwrap();
        for (name, output) in [
            ("a1", "/nix/store/remote-a1-profile"),
            ("a2", "/nix/store/remote-a2-profile"),
            ("b", "/nix/store/remote-b-profile"),
        ] {
            let drv = format!("/nix/store/remote-{name}.drv");
            std::fs::write(
                fixture.join(format!("result.{name}.json")),
                json!([{"drvPath":drv,"outputs":{"out":output}}]).to_string(),
            )
            .unwrap();
        }
        // Reverse JSON order and give the nested derivation an unrequested out.
        std::fs::write(
            fixture.join("result.local.json"),
            json!([
                {"drvPath":"/nix/store/shared.drv","outputs":{"out":shared}},
                {"drvPath":{"drvPath":"/nix/store/outer.drv","output":"out"},
                 "outputs":{"out":"/wrong-output","dev":dev}},
                {"drvPath":"/nix/store/local.drv","outputs":{"out":local}}
            ])
            .to_string(),
        )
        .unwrap();

        let data: deploy::data::Data = serde_json::from_value(config).unwrap();
        let overrides = deploy::CmdOverrides {
            ssh_user: None,
            profile_user: None,
            ssh_opts: None,
            fast_connection: None,
            auto_rollback: None,
            hostname: None,
            magic_rollback: None,
            temp_path: None,
            confirm_timeout: None,
            activation_timeout: None,
            sudo: None,
            interactive_sudo: None,
            dry_activate: false,
            remote_build: false,
        };
        let parts: Vec<_> = targets
            .iter()
            .map(|(name, ..)| {
                let node = &data.nodes[*name];
                deploy::make_deploy_data(
                    &data.generic_settings,
                    node,
                    name,
                    &node.node_settings.profiles["system"],
                    "system",
                    &overrides,
                    false,
                    None,
                )
            })
            .collect();
        let defs: Vec<_> = parts.iter().map(|data| data.defs().unwrap()).collect();
        let datas: Vec<_> = parts
            .iter()
            .zip(&defs)
            .map(|(data, defs)| PushProfileData {
                supports_flakes: true,
                check_sigs: false,
                repo: ".",
                deploy_data: data,
                deploy_defs: defs,
                keep_result: false,
                result_path: None,
                extra_build_args: &[],
                build_tree,
            })
            .collect();
        let expected_closures = vec![
            "/nix/store/remote-a1-profile".to_string(),
            local.display().to_string(),
            "/nix/store/remote-b-profile".to_string(),
            "/nix/store/remote-a2-profile".to_string(),
            dev.display().to_string(),
            local.display().to_string(),
            shared.display().to_string(),
        ];
        if (!cli_success && failure == "none") || std::env::var_os("DISPLAY_DEMO").is_some() {
            let result = build_and_push_profiles(&datas).await;
            if failure != "none" {
                assert!(result.is_err());
                log::error!("Preparation failed: {}", result.unwrap_err());
                assert_reaped(&fixture);
                continue;
            }
            let closures = result.unwrap();
            assert_eq!(closures, expected_closures);
            let events = std::fs::read_to_string(fixture.join("events")).unwrap();
            before(&events, "build.b.start", "build.a1.end");
            before(&events, "build.local.start", "build.a1.end");
            before(&events, "copy.local.start", "build.b.end");
            before(&events, "copy.other.start", "copy.local.end");
            before(&events, "check.a1", "sign.remote-a1-profile.start");
            before(&events, "sign.remote-a1-profile.end", "build.a2.start");
            before(&events, "check.a2", "sign.remote-a2-profile.start");
            before(&events, "sign.remote-a2-profile.end", "copy.a.start");
            for name in ["local-profile", "dev-profile", "shared-profile"] {
                before(&events, "build.local.end", &format!("sign.{name}.start"));
                before(&events, &format!("sign.{name}.end"), "copy.local.start");
            }
            assert_eq!(
                events.lines().filter(|&s| s == "build.local.start").count(),
                1
            );
            assert_eq!(
                events.lines().filter(|&s| s == "copy.local.start").count(),
                1
            );
            let build_args = std::fs::read_to_string(fixture.join("build.local.args")).unwrap();
            assert_eq!(
                build_args
                    .lines()
                    .filter(|&s| s == "/nix/store/local.drv^out")
                    .count(),
                1
            );
            assert!(build_args
                .lines()
                .any(|s| s == "/nix/store/outer.drv^out^dev"));
            let args = std::fs::read_to_string(fixture.join("copy.local.args")).unwrap();
            assert!(args.contains(local.to_str().unwrap()));
            assert!(args.contains(dev.to_str().unwrap()));
            assert!(!args.contains("/wrong-output"));
            let ssh = std::fs::read_to_string(fixture.join("copy.local.ssh")).unwrap();
            assert_eq!(
                shlex::split(ssh.trim()).unwrap(),
                ["-o", "ProxyCommand=ssh jump host -W %h:%p"]
            );
        } else {
            let mut command = Command::new(env!("CARGO_BIN_EXE_deploy"));
            if !build_tree {
                command.arg("--no-build-tree");
            }
            let output = command
                // Nix builders set RUST_LOG="", which disables the logs asserted below.
                .env("RUST_LOG", "info")
                .args([
                    "--skip-checks",
                    "--no-review-changes",
                    "--tag",
                    "fleet",
                    "--tag",
                    "edge",
                    "--tag",
                    "service",
                    "--targets",
                ])
                .args(targets.iter().map(|(name, ..)| format!(".#{name}.system")))
                .arg(".#excluded.system")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!stderr.contains("barrier timed out"), "{}", stderr);
            let events = std::fs::read_to_string(fixture.join("events")).unwrap();
            if cli_success {
                assert!(output.status.success(), "{}", stderr);
                let requests = std::fs::read_to_string(fixture.join("activations")).unwrap();
                let closures: Vec<String> = requests
                    .lines()
                    .map(|line| {
                        let request: deploy::remote_protocol::BootstrapRequest =
                            serde_json::from_str(line).unwrap();
                        match request.operation {
                            deploy::remote_protocol::RemoteOperation::Deploy(request) => {
                                request.closure
                            }
                            _ => panic!("unexpected revoke"),
                        }
                    })
                    .collect();
                assert_eq!(closures, expected_closures);
                let first = "activate.remote-a1-profile.start";
                for event in events
                    .lines()
                    .filter(|event| event.starts_with("sign.") || event.starts_with("copy."))
                {
                    before(&events, event, first);
                }
                for name in ["local", "other", "a"] {
                    before(&events, &format!("copy.{name}.end"), first);
                }
                // Include the SSH destination because l1/l3 intentionally share
                // the same closure. This catches swaps hidden by equal paths.
                let destinations: Vec<_> = events
                    .lines()
                    .filter_map(|event| {
                        event
                            .strip_prefix("activate:")
                            .and_then(|args| args.split_whitespace().next())
                    })
                    .collect();
                assert_eq!(
                    destinations,
                    targets.map(|(_, host, ..)| format!("root@{host}"))
                );
                assert_masters_closed(&fixture, 4);
                assert_reaped(&fixture);
                println!("mixed_cli_activation: ok (7 ordered activations after all signatures and copies)");
                continue;
            }
            assert!(!output.status.success());
            assert!(
                stderr.contains("bad exit status"),
                "scenario={}, status={}, stdout={:?}, stderr={}",
                failure,
                output.status,
                String::from_utf8_lossy(&output.stdout),
                stderr
            );
            assert!(!events.contains("activate:"), "{}", events);
            if failure == "resolve" {
                assert!(events.contains("resolve.slow.end"), "{}", events);
                assert!(!events.contains("build."), "{}", events);
            } else {
                assert!(stderr.contains("streamed build"), "{}", stderr);
                assert!(events.contains("build.b.end"), "{}", events);
            }
            if matches!(failure, "remote" | "check" | "sign-remote") {
                assert!(!events.contains("build.a2.start"), "{}", events);
                assert!(!events.contains("copy.a.start"), "{}", events);
            }
            if failure == "sign-local" {
                assert!(events.contains("sign.local-profile.failed"), "{}", events);
                assert!(!events.contains("copy.local.start"), "{}", events);
            }
            if failure == "sign-remote" {
                assert!(
                    events.contains("sign.remote-a1-profile.failed"),
                    "{}",
                    events
                );
            }
            assert_masters_closed(&fixture, 4);
            if build_tree {
                assert!(
                    !output.stderr.contains(&0x1b),
                    "non-TTY output contains terminal controls"
                );
                if failure != "resolve" {
                    assert!(stderr.contains("Build local batch"), "{}", stderr);
                    assert!(stderr.contains("ssh-ng://root@remote-b"), "{}", stderr);
                    assert!(stderr.contains("[completed] Build"), "{}", stderr);
                    assert!(!stderr.contains("monitor failed"), "{}", stderr);
                }
            }
        }
        assert_reaped(&fixture);
    }
    std::env::set_var("PATH", original_path);
    std::env::remove_var("LOCAL_KEY");
}

fn assert_reaped(fixture: &Path) {
    // Every recorded Nix and nom process must be reaped before returning.
    for entry in std::fs::read_dir(fixture).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name().to_string_lossy().starts_with("pid.") {
            let pid: i32 = std::fs::read_to_string(entry.path())
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "child {pid} still alive");
        }
    }
}
