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
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

fn executable(path: &Path, source: &str) {
    std::fs::write(path, source).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

const NIX: &str = r#"#!/bin/sh
set -eu
cd "$FIXTURE"
event() { printf '%s\n' "$1" >> events; : > "$1"; }
wait_for() {
  tries=0
  while [ ! -f "$1" ]; do
    tries=$((tries + 1))
    if [ "$tries" -gt 250 ]; then echo "barrier timed out: $1" >&2; exit 90; fi
    sleep 0.02
  done
}
case " $* " in
  *' eval '*)
    case " $* " in
      *' --json '*) cat deployment.json ;;
      *) printf 'true\n' ;;
    esac
    exit 0 ;;
  *' show-derivation '*)
    case " $* " in
      *resolve-fail*) wait_for resolve.slow.start; event resolve.fail; exit 45 ;;
    esac
    event resolve.slow.start
    echo "$$" > pid.resolve.slow
    sleep 0.2
    event resolve.slow.end
    printf '{"/nix/store/resolved.drv":{}}\n'
    exit 0 ;;
  *' copy '*' --derivation '*) exit 0 ;;
  *' store ls '*)
    case " $* " in
      *remote-a1*) event check.a1 ;;
      *remote-a2*) event check.a2 ;;
    esac
    if [ "$FAIL" = check ]; then exit 43; fi
    printf '{"entries":{"deploy-rx-activate":{},"activate-rs":{}}}\n'
    exit 0 ;;
  *' build '*)
    case " $* " in
      *remote-a1.drv*) name=a1 ;;
      *remote-a2.drv*) name=a2 ;;
      *remote-b.drv*) name=b ;;
      *) name=local ;;
    esac
    event "build.$name.start"
    echo "streamed build $name" >&2
    echo "$$" > "pid.build.$name"
    printf '%s\n' "$@" > "build.$name.args"
    if [ "$FAIL" = none ]; then
      case "$name" in
        a1) wait_for build.b.start; wait_for build.local.start ;;
        b) wait_for copy.local.start ;;
      esac
    fi
    if [ "$name" = b ]; then sleep 0.3; else sleep 0.1; fi
    event "build.$name.end"
    case "$FAIL:$name" in
      remote:a1|local:local) exit 42 ;;
    esac
    cat "result.$name.json"
    exit 0 ;;
  *' copy '*)
    case " $* " in
      *'ssh://root@local '*) name=local ;;
      *'ssh://root@other '*) name=other ;;
      *'ssh://root@remote-a '*) name=a ;;
      *) echo "unexpected copy: $*" >&2; exit 91 ;;
    esac
    event "copy.$name.start"
    echo "$$" > "pid.copy.$name"
    printf '%s\n' "$@" > "copy.$name.args"
    printf '%s\n' "$NIX_SSHOPTS" > "copy.$name.ssh"
    if [ "$FAIL" = none ] && [ "$name" = local ]; then wait_for copy.other.start; fi
    sleep 0.1
    event "copy.$name.end"
    if [ "$FAIL:$name" = copy:local ]; then exit 44; fi
    exit 0 ;;
esac
echo "unexpected nix: $*" >&2
exit 92
"#;

const SSH: &str = r#"#!/bin/sh
set -eu
cd "$FIXTURE"
printf '%s\n' "activate:$*" >> events
read -r request
printf '%s\n' "$request" >> activations
printf '%s\n' '{"Finished":{"ok":true,"rolled_back":false,"message":"done","rollback":null}}'
"#;

fn before(events: &str, first: &str, second: &str) {
    let lines: Vec<_> = events.lines().collect();
    let a = lines.iter().position(|&line| line == first).unwrap();
    let b = lines.iter().position(|&line| line == second).unwrap();
    assert!(a < b, "expected {} before {}:\n{}", first, second, events);
}

#[tokio::test]
async fn concurrent_preparation_preserves_mapping_and_drains_failures() {
    // One test owns this process's PATH; child CLI runs get isolated environments.
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    executable(&bin.join("nix"), NIX);
    executable(&bin.join("ssh"), SSH);
    let original_path = std::env::var_os("PATH").unwrap();
    let path = std::env::join_paths(
        std::iter::once(bin.clone()).chain(std::env::split_paths(&original_path)),
    )
    .unwrap();
    std::env::set_var("PATH", &path);
    std::env::remove_var("LOCAL_KEY");

    for failure in ["none", "resolve", "remote", "local", "check", "copy"] {
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
                build_tree: false,
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
        if failure == "none" {
            let closures = build_and_push_profiles(&datas).await.unwrap();
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
            let output = Command::new(env!("CARGO_BIN_EXE_deploy"))
                // Nix builders set RUST_LOG="", which disables the logs asserted below.
                .env("RUST_LOG", "info")
                .args([
                    "--skip-checks",
                    "--no-build-tree",
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
        }
        // Every recorded Nix process must have been reaped before returning.
        for entry in std::fs::read_dir(&fixture).unwrap() {
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
    std::env::set_var("PATH", original_path);
}
