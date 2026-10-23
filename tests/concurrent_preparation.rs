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
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::path::Path;
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
    runtime.block_on(concurrent_preparation_preserves_mapping_and_drains_failures(false));
    println!("concurrent_preparation_preserves_mapping_and_drains_failures: ok (6 scenarios)");
    if std::env::var_os("DEPLOY_RX_TEST_NOM").is_some() {
        runtime.block_on(concurrent_preparation_preserves_mapping_and_drains_failures(true));
        println!("real_nom_preparation: ok (6 scenarios)");
        for scenario in ["none", "remote"] {
            real_nom_in_pty(scenario);
        }
        for mode in ["early", "hang"] {
            std::env::set_var("NOM_FAILURE", mode);
            runtime.block_on(concurrent_preparation_preserves_mapping_and_drains_failures(true));
            println!("nom_{mode}: ok (Nix result preserved, all children reaped)");
        }
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
    let mut child = Command::new(std::env::current_exe().unwrap())
        .env("DISPLAY_DEMO", scenario)
        .env("RUST_LOG", "info")
        .stdin(Stdio::null())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut terminal = vt100::Parser::new(50, 120, 0);
    let mut transcript = Vec::new();
    let mut saw_three_trees = false;
    let mut buffer = [0; 8192];
    loop {
        match master.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                transcript.extend_from_slice(&buffer[..count]);
                terminal.process(&buffer[..count]);
                let screen = terminal.screen().contents();
                saw_three_trees |= screen.contains("[running] Build local batch")
                    && screen.lines().any(|line| {
                        line.starts_with("[running] Build connection") && line.contains("remote-a")
                    })
                    && screen.lines().any(|line| {
                        line.starts_with("[running] Build connection") && line.contains("remote-b")
                    })
                    && screen.matches("Dependency Graph").count() >= 3;
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
    println!("real_nom_pty_{scenario}: ok (three isolated trees, completion and failure states)");
}

fn nom_fixture() -> io::Result<i32> {
    if std::env::args().any(|arg| arg == "--json") {
        record_pid(&format!("nom.{}", std::process::id()))?;
        match std::env::var("NOM_FAILURE").as_deref() {
            Ok("early") => return Ok(73),
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
        + Duration::from_secs(if std::env::var_os("NOM_FAILURE").is_some() {
            20
        } else {
            5
        });
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
    if has("eval") {
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
    } else if has("build") {
        let name = if has("/nix/store/remote-a1.drv^out") {
            "a1"
        } else if has("/nix/store/remote-a2.drv^out") {
            "a2"
        } else if has("/nix/store/remote-b.drv^out") {
            "b"
        } else {
            "local"
        };
        record_pid(&format!("build.{name}"))?;
        std::fs::write(format!("build.{name}.args"), args.join("\n"))?;
        event(&format!("build.{name}.start"))?;
        eprintln!("streamed build {name}");
        let monitored = has("internal-json");
        let activity = u64::from(std::process::id()) << 32;
        if monitored {
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
            match name {
                "a1" => {
                    wait_for("build.b.start")?;
                    wait_for("build.local.start")?;
                }
                "b" => wait_for("copy.local.start")?,
                _ => (),
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
    let args: Vec<_> = std::env::args().skip(1).collect();
    append_line("events", &format!("activate:{}", args.join(" ")))?;
    let mut request = String::new();
    io::stdin().read_line(&mut request)?;
    append_line("activations", request.trim_end())?;
    println!(
        "{}",
        json!({"Finished": {
            "ok": true, "rolled_back": false, "message": "done", "rollback": null
        }})
    );
    Ok(0)
}

fn before(events: &str, first: &str, second: &str) {
    let lines: Vec<_> = events.lines().collect();
    let a = lines.iter().position(|&line| line == first).unwrap();
    let b = lines.iter().position(|&line| line == second).unwrap();
    assert!(a < b, "expected {} before {}:\n{}", first, second, events);
}

async fn concurrent_preparation_preserves_mapping_and_drains_failures(build_tree: bool) {
    // One test owns this process's PATH; child CLI runs get isolated environments.
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let executable = std::env::current_exe().unwrap();
    symlink(&executable, bin.join("nix")).unwrap();
    symlink(&executable, bin.join("ssh")).unwrap();
    if build_tree {
        symlink(&executable, bin.join("nom")).unwrap();
    }
    let original_path = std::env::var_os("PATH").unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin.clone()).chain(std::env::split_paths(&original_path)),
    )
    .unwrap();
    std::env::set_var("PATH", &path);
    std::env::remove_var("LOCAL_KEY");

    for failure in ["none", "resolve", "remote", "local", "check", "copy"] {
        if let Ok(scenario) = std::env::var("DISPLAY_DEMO") {
            if failure != scenario {
                continue;
            }
        }
        if std::env::var_os("NOM_FAILURE").is_some() && failure != "none" {
            continue;
        }
        let fixture = root.path().join(failure);
        std::fs::create_dir(&fixture).unwrap();
        std::env::set_var("FIXTURE", &fixture);
        std::env::set_var("FAIL", failure);
        let local = fixture.join("local-profile");
        let dev = fixture.join("dev-profile");
        let shared = fixture.join("shared-profile");
        for closure in [&local, &dev, &shared] {
            std::fs::create_dir(closure).unwrap();
            std::fs::write(closure.join("deploy-rx-activate"), "").unwrap();
            std::fs::write(closure.join("activate-rs"), "").unwrap();
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
        if failure == "none" || std::env::var_os("DISPLAY_DEMO").is_some() {
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
            before(&events, "check.a1", "build.a2.start");
            before(&events, "check.a2", "copy.a.start");
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
                    "--no-ssh-multiplexing",
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
            if failure == "remote" || failure == "check" {
                assert!(!events.contains("build.a2.start"), "{}", events);
            }
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
