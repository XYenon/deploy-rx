// SPDX-FileCopyrightText: 2026 deploy-rx contributors
//
// SPDX-License-Identifier: MPL-2.0

//! Run with `nix develop --command cargo test --test remote_sessions -- --ignored`.
//! These tests use real Nix generations and isolated profiles, without a VM.
#![cfg(unix)]

use deploy::remote_protocol::{
    BootstrapRequest, ProfileTarget, RemoteDeployRequest, RemoteEvent, RemoteOperation,
    RemoteRevokeRequest, RollbackReceipt, REMOTE_PROTOCOL_VERSION,
};
use std::io::Write;
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn flake_probe_preserves_extra_nix_arguments() {
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("nix-args");
    let wrapper = root.path().join("nix");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n\
             printf '%s\\n' BEGIN \"$@\" END >> {}\n\
             for arg do\n\
               case \"$arg\" in\n\
                 *builtins.compareVersions*|builtins.getFlake) exit 0 ;;\n\
               esac\n\
             done\n\
             exit 1\n",
            shlex::try_quote(log.to_str().unwrap()).unwrap(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let extra_args = [
        "--extra-experimental-features",
        "flakes ca-derivations",
        "--option",
        "substituters",
        "https://cache.example.org https://other.example.org",
        "--offline",
    ];
    let output = Command::new(env!("CARGO_BIN_EXE_deploy"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                root.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .args(["--skip-checks", "--no-build-tree", "--no-review-changes"])
        .arg(root.path().join("missing-deployment"))
        .arg("--")
        .args(extra_args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    // Stop at configuration evaluation; this test never builds or deploys.
    assert!(!output.status.success());
    let log = std::fs::read_to_string(log).unwrap();
    let commands: Vec<Vec<&str>> = log
        .split("BEGIN\n")
        .skip(1)
        .map(|command| command.strip_suffix("END\n").unwrap().lines().collect())
        .collect();
    assert_eq!(commands.len(), 3, "{log}");
    let mut expected = vec![
        "--extra-experimental-features",
        "nix-command",
        "--extra-experimental-features",
        "flakes",
        "eval",
        "--expr",
        "builtins.getFlake",
    ];
    expected.extend(extra_args);
    assert_eq!(commands[1], expected);
    assert!(commands[2].ends_with(&extra_args), "{}", log);
    assert!(commands
        .iter()
        .all(|args| !args.contains(&"--experimental-features")));
}

#[test]
#[ignore = "requires Nix to evaluate the CLI's version check"]
fn minimum_nix_version_is_checked_before_deployment_evaluation() {
    let nix = Command::new("sh")
        .args(["-c", "command -v nix"])
        .output()
        .unwrap();
    assert!(nix.status.success());
    let nix = String::from_utf8(nix.stdout).unwrap();
    for (version, supported) in [
        ("2.12.1", false),
        ("2.13pre20230101", false),
        ("2.13", true),
        ("2.35.2", true),
        ("3.0", true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let probe = root.path().join("probe");
        let wrapper = root.path().join("nix");
        std::fs::write(&wrapper, format!(
            "#!/bin/sh\ncase \"$5\" in\n\
             *builtins.compareVersions*) exec {} --extra-experimental-features nix-command eval --expr \"let native = builtins; in let builtins = native // {{ nixVersion = \\\"{version}\\\"; }}; in ($5)\" ;;\n\
             *) : > {}; exit 1 ;;\nesac\n",
            shlex::try_quote(nix.trim()).unwrap(),
            shlex::try_quote(probe.to_str().unwrap()).unwrap(),
        )).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_deploy"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    root.path().display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .args(["--skip-checks", "--no-build-tree", "--no-review-changes"])
            .arg(root.path().join("missing-deployment"))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(probe.exists(), supported, "version {version}: {stderr}");
        if !supported {
            assert!(!output.status.success());
            assert!(
                stderr.contains("Nix 2.13 or newer is required"),
                "{}",
                stderr
            );
            assert!(stderr.contains(&format!("found {version}")), "{}", stderr);
        }
    }
}

struct Fixture {
    root: tempfile::TempDir,
    profile: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        Self { root, profile }
    }

    fn closure(&self, name: &str, script: &str) -> String {
        let path = self.root.path().join(name);
        std::fs::create_dir(&path).unwrap();
        let activate = path.join("deploy-rx-activate");
        std::fs::write(&activate, format!("#!/bin/sh\nset -eu\n{}\n", script)).unwrap();
        std::fs::set_permissions(activate, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = Command::new("nix-store")
            .arg("--add")
            .arg(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    fn request(&self, closure: &str) -> RemoteDeployRequest {
        RemoteDeployRequest {
            closure: closure.into(),
            profile: ProfileTarget::ProfilePath {
                profile_path: self.profile.display().to_string(),
            },
            profile_user: "unused-without-sudo".into(),
            review_changes: false,
            dry_activate: false,
            boot: false,
            test: false,
            auto_rollback: true,
            magic_rollback: false,
            confirm_timeout: 1,
            activation_timeout: Some(10),
            temp_path: self.root.path().display().to_string(),
            debug_logs: false,
            log_dir: None,
        }
    }

    fn set(&self, closure: &str) {
        let status = Command::new("nix-env")
            .arg("-p")
            .arg(&self.profile)
            .arg("--set")
            .arg(closure)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn run(&self, operation: RemoteOperation, broken_stderr: bool) -> RemoteEvent {
        run(operation, broken_stderr)
    }

    fn deploy(&self, closure: &str) -> RollbackReceipt {
        receipt(self.run(RemoteOperation::Deploy(self.request(closure)), false))
    }

    fn revoke(&self, closure: &str, rollback: RollbackReceipt) -> RemoteEvent {
        self.run(
            RemoteOperation::Revoke(RemoteRevokeRequest {
                closure: closure.into(),
                profile: self.request(closure).profile,
                rollback,
                profile_user: "unused-without-sudo".into(),
                temp_path: self.root.path().display().to_string(),
                debug_logs: false,
                log_dir: None,
            }),
            false,
        )
    }
}

fn run(operation: RemoteOperation, broken_stderr: bool) -> RemoteEvent {
    let request = BootstrapRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        sudo: None,
        sudo_password: None,
        interactive_sudo: false,
        operation,
    };
    let mut command = Command::new(env!("CARGO_BIN_EXE_activate"));
    command
        .arg("bootstrap-session")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(if broken_stderr {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });
    let mut child = command.spawn().unwrap();
    if broken_stderr {
        drop(child.stderr.take());
    }
    let mut stdin = child.stdin.take().unwrap();
    serde_json::to_writer(&mut stdin, &request).unwrap();
    writeln!(stdin).unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "remote process failed: {:?}",
        output.status
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<RemoteEvent>(line).ok())
        .find(|event| matches!(event, RemoteEvent::Finished { .. }))
        .expect("remote session did not report Finished")
}

fn receipt(event: RemoteEvent) -> RollbackReceipt {
    match event {
        RemoteEvent::Finished {
            ok: true,
            rollback: Some(receipt),
            ..
        } => receipt,
        other => panic!("expected successful deployment receipt, got {:?}", other),
    }
}

fn success(event: RemoteEvent) {
    assert!(
        matches!(event, RemoteEvent::Finished { ok: true, .. }),
        "{:?}",
        event
    );
}

#[test]
#[ignore = "requires a writable Nix store"]
fn unchanged_deployment_does_not_revoke_an_existing_generation() {
    let fixture = Fixture::new();
    let old = fixture.closure("old", "exit 0");
    let current = fixture.closure("current", "echo current");
    fixture.set(&old);
    fixture.set(&current);
    let link = std::fs::read_link(&fixture.profile).unwrap();
    let rollback = fixture.deploy(&current);
    assert!(rollback.created_generation.is_none());
    success(fixture.revoke(&current, rollback));
    assert_eq!(std::fs::read_link(&fixture.profile).unwrap(), link);
    assert_eq!(fixture.profile.canonicalize().unwrap(), Path::new(&current));
}

#[test]
#[ignore = "requires a writable Nix store"]
fn failed_profile_set_preserves_the_previous_revoke_identity() {
    let fixture = Fixture::new();
    let old = fixture.closure("old", "exit 0");
    let new = fixture.closure("new", "exit 0");
    fixture.set(&old);
    let rollback = fixture.deploy(&new);
    let invalid = fixture.root.path().join("missing-closure");
    let result = fixture.run(
        RemoteOperation::Deploy(fixture.request(invalid.to_str().unwrap())),
        false,
    );
    assert!(matches!(result, RemoteEvent::Finished { ok: false, .. }));
    success(fixture.revoke(&new, rollback));
    assert_eq!(fixture.profile.canonicalize().unwrap(), Path::new(&old));
}

#[test]
#[ignore = "requires a writable Nix store"]
fn revoke_restores_the_selected_generation_instead_of_the_next_older_one() {
    let fixture = Fixture::new();
    let first = fixture.closure("first", "exit 0");
    let second = fixture.closure("second", "echo second");
    let third = fixture.closure("third", "echo third");
    fixture.set(&first);
    fixture.set(&second);
    assert!(Command::new("nix-env")
        .arg("-p")
        .arg(&fixture.profile)
        .args(["--switch-generation", "1"])
        .status()
        .unwrap()
        .success());
    let old_link = std::fs::read_link(&fixture.profile).unwrap();
    let rollback = fixture.deploy(&third);
    success(fixture.revoke(&third, rollback));
    assert_eq!(std::fs::read_link(&fixture.profile).unwrap(), old_link);
    assert!(fixture.root.path().join("profile-2-link").exists());
    assert!(!fixture.root.path().join("profile-3-link").exists());
}

#[test]
#[ignore = "requires a writable Nix store"]
fn newer_same_closure_deployment_invalidates_an_old_receipt() {
    let fixture = Fixture::new();
    let old = fixture.closure("old", "exit 0");
    let new = fixture.closure("new", "echo new");
    fixture.set(&old);
    let first = fixture.deploy(&new);
    let second = fixture.deploy(&new);
    assert!(
        matches!(fixture.revoke(&new, first.clone()), RemoteEvent::Finished { ok: false, message, .. }
        if message.contains("refusing to revoke a newer deployment"))
    );
    assert_eq!(fixture.profile.canonicalize().unwrap(), Path::new(&new));
    success(fixture.revoke(&new, second));
    success(fixture.revoke(&new, first));
    assert_eq!(fixture.profile.canonicalize().unwrap(), Path::new(&old));
}

#[test]
#[ignore = "requires a writable Nix store"]
fn rollback_preserves_boot_and_test_modes() {
    for (boot, test, expected) in [(true, false, "1:0"), (false, true, "0:1")] {
        let fixture = Fixture::new();
        let marker = fixture.root.path().join("mode");
        let old = fixture.closure(
            "old",
            &format!(
                "printf '%s:%s' \"$BOOT\" \"$TEST\" > '{}'",
                marker.display()
            ),
        );
        let new = fixture.closure("new", "echo new");
        fixture.set(&old);
        let mut request = fixture.request(&new);
        request.boot = boot;
        request.test = test;
        let rollback = receipt(fixture.run(RemoteOperation::Deploy(request), false));
        success(fixture.revoke(&new, rollback));
        assert_eq!(std::fs::read_to_string(marker).unwrap(), expected);
    }
}

#[test]
#[ignore = "requires a writable Nix store"]
fn activation_timeout_stops_descendants_before_rollback() {
    let fixture = Fixture::new();
    let marker = fixture.root.path().join("marker");
    let old = fixture.closure("old", &format!("echo old > '{}'", marker.display()));
    let new = fixture.closure(
        "new",
        &format!("(sleep 2; echo late > '{}') &\nwait", marker.display()),
    );
    fixture.set(&old);
    let mut request = fixture.request(&new);
    request.activation_timeout = Some(1);
    assert!(
        matches!(fixture.run(RemoteOperation::Deploy(request), false),
        RemoteEvent::Finished { ok: false, rolled_back: true, message, .. } if message.contains("timed out"))
    );
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "old\n");
    assert_eq!(fixture.profile.canonicalize().unwrap(), Path::new(&old));
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires a writable Nix store and root mount namespaces (sudo when non-root)"]
fn system_test_then_failed_switch_preserves_boot_and_running_targets() {
    let mut fixture = Fixture::new();
    let state = fixture.root.path().join("state");
    std::fs::create_dir_all(state.join("profiles")).unwrap();
    fixture.profile = state.join("profiles/system");
    let boot = fixture.root.path().join("boot");
    let script = |name: &str, fail: bool| {
        format!(
        "if [ \"$BOOT\" = 1 ] || [ \"$TEST\" = 0 ]; then echo {name} > '{}'; fi\n\
         if [ \"$BOOT\" = 0 ]; then ln -sfn \"$(readlink -f \"$PROFILE\")\" /run/current-system; fi\n\
         exit {}",
        boot.display(), if fail { 7 } else { 0 },
    )
    };
    let a = fixture.closure("system-a", &script("A", false));
    let b = fixture.closure("system-b", &script("B", false));
    let c = fixture.closure("system-c", &script("C", true));
    fixture.set(&a);
    let original_link = std::fs::read_link(&fixture.profile).unwrap();
    let mut test = fixture.request(&b);
    test.test = true;
    let test_request = fixture.root.path().join("test.json");
    let switch_request = fixture.root.path().join("switch.json");
    for (path, request) in [
        (&test_request, test),
        (&switch_request, fixture.request(&c)),
    ] {
        std::fs::write(
            path,
            serde_json::to_vec(&RemoteOperation::Deploy(request)).unwrap(),
        )
        .unwrap();
    }
    let nix_state = std::env::var("NIX_STATE_DIR").unwrap_or_else(|_| "/nix/var/nix".into());
    let socket = Path::new(&nix_state).join("daemon-socket/socket");
    let store = if socket.exists() {
        format!("unix://{}", socket.display())
    } else {
        format!("local?state={nix_state}")
    };
    let mut command = if unsafe { libc::geteuid() } == 0 {
        Command::new("env")
    } else {
        let mut sudo = Command::new("sudo");
        sudo.args(["-n", "env"]);
        sudo
    };
    let output = command
        .arg(format!("PATH={}", std::env::var("PATH").unwrap()))
        .arg(format!("NIX_STATE_DIR={}", state.display()))
        .arg(format!("NIX_REMOTE={store}"))
        .args([
            "unshare",
            "--mount",
            "--propagation",
            "private",
            "sh",
            "-eu",
            "-c",
        ])
        .arg(format!(
            "trap 'chmod -R a+rwX \"$NIX_STATE_DIR\"' EXIT\n\
             mount -t tmpfs tmpfs /run\n\
             ln -s '{a}' /run/current-system\n\
             echo A > '{}'\n\
             '{}' privileged-session --request-path '{}'\n\
             test \"$(readlink -f '{}')\" = '{a}'\n\
             test \"$(readlink -f /run/current-system)\" = '{b}'\n\
             test \"$(cat '{}')\" = A\n\
             '{}' privileged-session --request-path '{}'\n\
             test \"$(readlink -f /run/current-system)\" = '{b}'\n\
             test \"$(cat '{}')\" = A",
            boot.display(),
            env!("CARGO_BIN_EXE_activate"),
            test_request.display(),
            fixture.profile.display(),
            boot.display(),
            env!("CARGO_BIN_EXE_activate"),
            switch_request.display(),
            boot.display(),
        ))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<_> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<RemoteEvent>(line).ok())
        .filter(|event| matches!(event, RemoteEvent::Finished { .. }))
        .collect();
    assert_eq!(events.len(), 2);
    let test_receipt = receipt(events[0].clone());
    assert_eq!(test_receipt.previous_link, test_receipt.expected_link);
    assert!(test_receipt.created_generation.is_none());
    assert!(matches!(
        events[1],
        RemoteEvent::Finished {
            ok: false,
            rolled_back: true,
            ..
        }
    ));
    assert_eq!(std::fs::read_link(&fixture.profile).unwrap(), original_link);
    assert_eq!(fixture.profile.canonicalize().unwrap(), Path::new(&a));
}

#[test]
#[ignore = "requires a writable Nix store"]
fn closed_log_pipe_does_not_prevent_reactivation() {
    let fixture = Fixture::new();
    let marker = fixture.root.path().join("marker");
    let old = fixture.closure(
        "old",
        &format!("echo old > '{}'; echo restored", marker.display()),
    );
    let new = fixture.closure(
        "new",
        &format!(
            "echo new > '{}'; echo failure >&2; exit 7",
            marker.display()
        ),
    );
    fixture.set(&old);
    for log_dir in [
        None,
        Some(fixture.root.path().join("logs").display().to_string()),
    ] {
        let mut request = fixture.request(&new);
        request.log_dir = log_dir;
        assert!(matches!(
            fixture.run(RemoteOperation::Deploy(request), true),
            RemoteEvent::Finished {
                ok: false,
                rolled_back: true,
                ..
            }
        ));
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "old\n");
    }
}

#[test]
#[ignore = "requires a writable Nix store"]
fn stalled_stderr_does_not_delay_timeout_or_rollback() {
    for file_logs in [false, true] {
        let fixture = Fixture::new();
        let marker = fixture.root.path().join("restored");
        let pid_file = fixture.root.path().join("activation-pid");
        let old = fixture.closure(
            "old",
            &format!("echo old > '{}'; echo restored", marker.display()),
        );
        let new = fixture.closure(
            "new",
            &format!(
                "echo $$ > '{}'; head -c 2097152 /dev/zero; sleep 30",
                pid_file.display()
            ),
        );
        fixture.set(&old);
        let mut request = fixture.request(&new);
        request.activation_timeout = Some(1);
        let request_path = fixture.root.path().join("request.json");
        std::fs::write(
            &request_path,
            serde_json::to_vec(&RemoteOperation::Deploy(request)).unwrap(),
        )
        .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_activate"));
        if file_logs {
            command
                .arg("--log-dir")
                .arg(fixture.root.path().join("logs"));
        }
        let mut child = command
            .args(["privileged-session", "--request-path"])
            .arg(request_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stalled_reader = child.stderr.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                if let Ok(pid) = std::fs::read_to_string(&pid_file) {
                    unsafe {
                        libc::kill(-pid.trim().parse::<i32>().unwrap(), libc::SIGKILL);
                    }
                }
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("stalled stderr blocked the activation deadline or rollback");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let output = child.wait_with_output().unwrap();
        drop(stalled_reader);
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .any(|line| matches!(
                serde_json::from_str::<RemoteEvent>(line),
                Ok(RemoteEvent::Finished {
                    ok: false,
                    rolled_back: true,
                    ..
                })
            )));
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "old\n");
        assert_eq!(fixture.profile.canonicalize().unwrap(), Path::new(&old));
    }
}

#[test]
#[ignore = "requires a writable Nix store"]
fn running_system_restore_is_attempted_even_when_boot_restore_fails() {
    let fixture = Fixture::new();
    let marker = fixture.root.path().join("running-restored");
    let previous = fixture.closure("previous-boot", "test \"$BOOT\" = 1; exit 8");
    let running = fixture.closure(
        "previous-running",
        &format!(
            "test \"$TEST\" = 1; test \"$BOOT\" = 0; echo running > '{}'",
            marker.display()
        ),
    );
    let new = fixture.closure("new", "exit 0");
    fixture.set(&previous);
    let mut rollback = fixture.deploy(&new);
    rollback.previous_running_target = Some(running.into());
    let result = fixture.revoke(&new, rollback);
    assert!(
        matches!(result, RemoteEvent::Finished { ok: false, message, .. } if message.contains("bad exit code: Some(8)"))
    );
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "running\n");
    assert_eq!(
        fixture.profile.canonicalize().unwrap(),
        Path::new(&previous)
    );
}

#[test]
#[ignore = "requires a writable Nix store"]
fn concurrent_sessions_serialize_activation_and_snapshot() {
    let fixture = Fixture::new();
    let log = fixture.root.path().join("order");
    let old = fixture.closure("old", "exit 0");
    let first = fixture.closure(
        "first",
        &format!(
            "echo first-start >> '{}'; sleep 1; echo first-end >> '{}'",
            log.display(),
            log.display()
        ),
    );
    let second = fixture.closure("second", &format!("echo second >> '{}'", log.display()));
    fixture.set(&old);
    let request = fixture.request(&first);
    let thread = std::thread::spawn(move || receipt(run(RemoteOperation::Deploy(request), false)));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !log.exists() {
        assert!(Instant::now() < deadline, "first activation never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    let rollback = fixture.deploy(&second);
    let _ = thread.join().unwrap();
    assert_eq!(rollback.previous_target.as_deref(), Some(Path::new(&first)));
    assert_eq!(
        std::fs::read_to_string(log).unwrap(),
        "first-start\nfirst-end\nsecond\n"
    );
}

#[test]
#[cfg(target_os = "linux")]
#[ignore = "requires Nix and a POSIX shell"]
fn restrictive_umask_keeps_handoff_readable_across_users() {
    let fixture = Fixture::new();
    let closure = fixture.closure("dry", "exit 0");
    let sudo = fixture.root.path().join("check-handoff");
    // Inspect the handoff before exec, just as another profile user must read it.
    std::fs::write(&sudo, "#!/bin/sh\nshift\nexe=$1; shift\n[ \"$1\" = privileged-session ]; shift\nshift\n[ \"$(stat -c %a \"$1\")\" = 644 ] || exit 31\n[ \"$(stat -c %a \"$(dirname \"$1\")\")\" = 711 ] || exit 32\nexec \"$exe\" privileged-session --request-path \"$1\"\n").unwrap();
    std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut operation = fixture.request(&closure);
    operation.dry_activate = true;
    let request = BootstrapRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        sudo: Some(deploy::sudo::SudoCommand::new(vec![sudo.display().to_string()]).unwrap()),
        sudo_password: None,
        interactive_sudo: false,
        operation: RemoteOperation::Deploy(operation),
    };
    let mut command = Command::new(env!("CARGO_BIN_EXE_activate"));
    command
        .arg("bootstrap-session")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    serde_json::to_writer(&mut stdin, &request).unwrap();
    writeln!(stdin).unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("\"ok\":true"));
}

#[tokio::test]
#[ignore = "requires a writable Nix store and dynamic derivations"]
async fn nested_dynamic_derivation_survives_evaluation_and_build() {
    build_dynamic_fixture(false, None, false, false).await;
}

#[tokio::test]
#[ignore = "requires a writable Nix store and dynamic derivations"]
async fn dynamic_build_preserves_encoded_flake_subdirectory() {
    build_dynamic_fixture(true, None, false, false).await;
}

#[tokio::test]
#[ignore = "requires a writable Nix store and dynamic derivations"]
async fn dynamic_build_preserves_nonpersisted_input_overrides() {
    build_dynamic_fixture(true, Some("message"), false, false).await;
}

#[tokio::test]
#[ignore = "requires a writable Nix store and dynamic derivations"]
async fn dynamic_build_preserves_nested_input_overrides() {
    build_dynamic_fixture(true, Some("parent/message"), false, false).await;
}

#[tokio::test]
#[ignore = "requires a writable Nix store and dynamic derivations"]
async fn dynamic_build_preserves_dirty_git_metadata() {
    build_dynamic_fixture(true, None, true, false).await;
}

#[tokio::test]
#[ignore = "requires a writable Nix store and dynamic derivations"]
async fn split_dynamic_build_keeps_each_shared_closure_rooted() {
    build_dynamic_fixture(false, None, false, true).await;
}

async fn build_dynamic_fixture(
    subdir: bool,
    override_input: Option<&str>,
    dirty_git: bool,
    shared_groups: bool,
) {
    let root = tempfile::tempdir().unwrap();
    let repo_path = root.path().join("repo");
    let flake_dir = if subdir {
        repo_path.join("deploy config")
    } else {
        repo_path.clone()
    };
    std::fs::create_dir_all(&flake_dir).unwrap();
    if subdir {
        std::fs::write(
            repo_path.join("flake.nix"),
            "{ outputs = { self }: throw \"wrong repository-root flake\"; }",
        )
        .unwrap();
    }
    let message = repo_path.join("message");
    let overridden = root.path().join("override");
    for (path, value) in [
        (
            &message,
            if override_input.is_some() {
                "wrong-default"
            } else {
                "selected"
            },
        ),
        (&overridden, "selected"),
    ] {
        std::fs::create_dir(path).unwrap();
        std::fs::write(
            path.join("flake.nix"),
            format!("{{ outputs = {{ self }}: {{ text = \"{value}\"; }}; }}"),
        )
        .unwrap();
    }
    let parent = repo_path.join("parent");
    std::fs::create_dir_all(parent.join("asset")).unwrap();
    std::fs::write(parent.join("asset/result"), "auxiliary").unwrap();
    std::fs::write(
        parent.join("flake.nix"),
        r#"{
          inputs.message.url = "path:../message";
          inputs.alias.follows = "message";
          inputs.asset = { url = "path:./asset"; flake = false; };
          outputs = { self, message, alias, asset }: {
            text = assert alias.text == message.text;
              assert builtins.readFile "${asset}/result" == "auxiliary";
              message.text;
          };
        }"#,
    )
    .unwrap();
    std::fs::write(
        flake_dir.join("flake.nix"),
        r#"{
      inputs.message.url = "path:@MESSAGE@";
      inputs.alias.follows = "message";
      inputs.parent.url = "path:@PARENT@";
      outputs = { self, message, alias, parent }: let
        text = @TEXT@;
        inner = assert self.lastModified > 1; assert alias.text == message.text; assert parent.text != ""; builtins.derivation {
          name = "dynamic-result"; system = "x86_64-linux"; builder = "/bin/sh";
          __contentAddressed = true; outputHashMode = "recursive"; outputHashAlgo = "sha256";
          outputs = [ "out" "dev" ];
          args = [ "-c" "/bin/mkdir $out $dev; echo default > $out/result; echo ${text} > $dev/result; /bin/touch $dev/deploy-rx-activate $dev/activate-rs" ];
        };
        copyDrv = name: drv: builtins.derivation {
          inherit name; system = "x86_64-linux"; builder = "/bin/sh";
          __contentAddressed = true; outputHashMode = "text"; outputHashAlgo = "sha256";
          args = [ "-c" "/bin/cp ${drv.drvPath} $out" ];
        };
        middle = copyDrv "dynamic-middle.drv" inner;
        outer = copyDrv "dynamic-outer.drv" middle;
        generated = builtins.outputOf
          (builtins.outputOf (builtins.unsafeDiscardOutputDependency outer.drvPath) "out") "out";
        regular = name: builtins.derivation {
          inherit name; system = "x86_64-linux"; builder = "/bin/sh";
          args = [ "-c" "/bin/mkdir $out; echo ${name} > $out/result; /bin/touch $out/deploy-rx-activate $out/activate-rs" ];
        };
        one = regular "one";
        two = regular "two";
      in {
        deploy.nodes."node.with-dot" = {
          hostname = "unused"; user = "root";
          profiles = { "app-with-dash".path = {
            type = "derivation"; outputs = [ "out" "dev" ];
            drvPath = generated; outputName = "dev";
            outPath = throw "generated outPath must not be forced";
          }; } // (if @SHARED@ then {
            app-with-dash-dev.path = one;
            one.path = one; one-again.path = one;
            two.path = two; two-again.path = two;
          } else {});
        };
        expected = builtins.unsafeDiscardStringContext outer.drvPath;
      };
    }"#
        .replace("@MESSAGE@", if subdir { "../message" } else { "./message" })
        .replace("@PARENT@", if subdir { "../parent" } else { "./parent" })
        .replace("@TEXT@", if dirty_git {
            "assert self.dirtyRev == self.sourceInfo.dirtyRev; assert self.dirtyShortRev == self.sourceInfo.dirtyShortRev; self.dirtyShortRev + \"_\" + self.dirtyRev"
        } else if override_input == Some("parent/message") { "parent.text" } else { "message.text" })
        .replace("@SHARED@", if shared_groups { "true" } else { "false" }),
    )
    .unwrap();
    let mut expected_text = "selected\n".to_string();
    if dirty_git {
        for args in [
            vec!["init", "--quiet"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        ] {
            assert!(Command::new("git")
                .current_dir(&repo_path)
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        let revision = Command::new("git")
            .current_dir(&repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(revision.status.success());
        let revision = String::from_utf8(revision.stdout).unwrap();
        let revision = revision.trim();
        expected_text = format!("{}-dirty_{revision}-dirty\n", &revision[..7]);
        let flake = flake_dir.join("flake.nix");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(flake)
            .unwrap();
        writeln!(file, "# dirty working tree").unwrap();
    }
    let repo = format!(
        "{}{}{}",
        if dirty_git { "git+file://" } else { "path:" },
        repo_path.display(),
        if subdir { "?dir=deploy%20config" } else { "" }
    );
    let store_root = root.path().display().to_string();
    let mut args = vec![
        "--store".into(),
        format!("local?store={store_root}/store&state={store_root}/state&log={store_root}/log"),
        "--option".into(),
        "build-users-group".into(),
        "".into(),
        "--extra-experimental-features".into(),
        "dynamic-derivations ca-derivations".into(),
        "--option".into(),
        "sandbox".into(),
        "false".into(),
    ];
    let mut original_lock = None;
    if let Some(input) = override_input {
        let output = Command::new("nix")
            .args([
                "flake",
                "lock",
                "--extra-experimental-features",
                "nix-command flakes",
            ])
            .arg(&repo)
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        original_lock = Some(std::fs::read(flake_dir.join("flake.lock")).unwrap());
        args.extend([
            "--override-input".into(),
            input.into(),
            format!("path:{}", overridden.display()),
        ]);
    }
    args.push("--no-write-lock-file".into());
    let output = Command::new("nix")
        .args([
            "eval",
            "--json",
            "--extra-experimental-features",
            "nix-command flakes",
        ])
        .args(&args)
        .arg(format!("{}#deploy", repo))
        .arg("--apply")
        .arg(include_str!("../nix/transform-deploy.nix"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let data: deploy::data::Data = serde_json::from_slice(&output.stdout).unwrap();
    let expected = Command::new("nix")
        .args([
            "eval",
            "--raw",
            "--extra-experimental-features",
            "nix-command flakes",
        ])
        .args(&args)
        .arg(format!("{}#expected", repo))
        .output()
        .unwrap();
    assert!(expected.status.success());
    let outer = String::from_utf8(expected.stdout).unwrap();
    let node = &data.nodes["node.with-dot"];
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
    let mut profile_names = vec!["app-with-dash"];
    if shared_groups {
        profile_names.extend(["app-with-dash-dev", "one", "one-again", "two", "two-again"]);
    }
    let deployments: Vec<_> = profile_names
        .iter()
        .map(|name| {
            deploy::make_deploy_data(
                &data.generic_settings,
                node,
                "node.with-dot",
                &node.node_settings.profiles[*name],
                name,
                &overrides,
                false,
                None,
            )
        })
        .collect();
    let defs: Vec<_> = deployments
        .iter()
        .map(|deployment| deployment.defs().unwrap())
        .collect();
    let result_path = root.path().join("results").display().to_string();
    let pushes: Vec<_> = deployments
        .iter()
        .zip(&defs)
        .map(|(deployment, defs)| deploy::push::PushProfileData {
            supports_flakes: true,
            check_sigs: false,
            repo: &repo,
            deploy_data: deployment,
            deploy_defs: defs,
            keep_result: shared_groups,
            result_path: Some(&result_path),
            extra_build_args: &args,
            build_tree: false,
        })
        .collect();
    let mut installables = Vec::new();
    for push in &pushes {
        installables.push(deploy::push::resolve_derivation(push).await.unwrap());
    }
    assert!(installables[0].starts_with("(builtins.outputOf "));
    assert!(std::path::Path::new(&outer).exists());
    assert!(!std::fs::read_dir(root.path().join("store"))
        .unwrap()
        .any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with("-dynamic-middle.drv")));
    let items: Vec<_> = pushes
        .iter()
        .zip(&installables)
        .map(|(push, drv)| (push, drv.as_str()))
        .collect();
    let outputs = deploy::push::build_profiles_locally(&items).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(std::path::Path::new(&outputs[0]).join("result")).unwrap(),
        expected_text
    );
    if shared_groups {
        assert_eq!(outputs[1], outputs[2]);
        assert_eq!(outputs[2], outputs[3]);
        assert_eq!(outputs[4], outputs[5]);
        for (link, output) in [
            ("groups/0/profiles-dev", &outputs[0]),
            ("groups/1/profiles", &outputs[1]),
            ("groups/2/profiles", &outputs[4]),
        ] {
            assert_eq!(
                Path::new(&result_path).join(link).canonicalize().unwrap(),
                Path::new(output)
            );
        }
        assert_eq!(
            std::fs::read_to_string(Path::new(&outputs[1]).join("result")).unwrap(),
            "one\n"
        );
        assert_eq!(
            std::fs::read_to_string(Path::new(&outputs[4]).join("result")).unwrap(),
            "two\n"
        );
    }
    if let Some(original) = original_lock {
        assert_eq!(
            std::fs::read(flake_dir.join("flake.lock")).unwrap(),
            original
        );
    } else {
        assert!(!flake_dir.join("flake.lock").exists());
    }
}

#[test]
#[ignore = "requires Nix packages from the flake"]
fn profile_helper_replaces_the_actual_manifest_element() {
    let root = tempfile::tempdir().unwrap();
    let expr = format!("let f = builtins.getFlake {}; pkgs = import f.inputs.nixpkgs {{ system = \"x86_64-linux\"; }}; p = f.lib.x86_64-linux.activate.profile pkgs.hello; in (builtins.elemAt p.paths 1).text",
        serde_json::to_string(env!("CARGO_MANIFEST_DIR")).unwrap());
    let output = Command::new("nix")
        .args(["eval", "--raw", "--impure", "--expr"])
        .arg(expr)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let script = root.path().join("activate");
    std::fs::write(&script, output.stdout).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    for _ in 0..2 {
        assert!(Command::new(&script)
            .env("HOME", root.path())
            .env("XDG_STATE_HOME", root.path().join("state"))
            .env(
                "NIX_CONFIG",
                "experimental-features = nix-command flakes\nuse-xdg-base-directories = true"
            )
            .status()
            .unwrap()
            .success());
    }
    let list = Command::new("nix")
        .args(["profile", "list", "--json"])
        .env("HOME", root.path())
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env(
            "NIX_CONFIG",
            "experimental-features = nix-command flakes\nuse-xdg-base-directories = true",
        )
        .output()
        .unwrap();
    assert!(list.status.success());
    let manifest: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    let elements = manifest["elements"].as_object().unwrap();
    assert_eq!(elements.len(), 1);
    assert!(elements.contains_key("hello"));
}
