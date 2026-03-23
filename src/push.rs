// SPDX-FileCopyrightText: 2020 Serokell <https://serokell.io/>
//
// SPDX-License-Identifier: MPL-2.0

use log::{debug, info, warn};
use std::path::Path;
use std::process::{Command as StdCommand, Stdio};
use thiserror::Error;
use tokio::process::Command;

#[derive(Error, Debug)]
pub enum PushProfileError {
    #[error("Failed to run Nix show-derivation command: {0}")]
    ShowDerivation(std::io::Error),
    #[error("Nix show-derivation command resulted in a bad exit code: {0:?}")]
    ShowDerivationExit(Option<i32>),
    #[error("Nix show-derivation command output contained an invalid UTF-8 sequence: {0}")]
    ShowDerivationUtf8(std::str::Utf8Error),
    #[error("Failed to parse the output of nix show-derivation: {0}")]
    ShowDerivationParse(serde_json::Error),
    #[error("Nix show derivation output is not an object")]
    ShowDerivationInvalid,
    #[error("Nix show-derivation output is empty")]
    ShowDerivationEmpty,
    #[error("Failed to run Nix build command: {0}")]
    Build(std::io::Error),
    #[error("Nix build command resulted in a bad exit code: {0:?}")]
    BuildExit(Option<i32>),
    #[error(
        "Activation script deploy-rx-activate does not exist in profile.\n\
             Did you forget to use deploy-rx#lib.<...>.activate.<...> on your profile path?"
    )]
    DeployRsActivateDoesntExist,
    #[error("Activation script activate-rs does not exist in profile.\n\
             Is there a mismatch in deploy-rx used in the flake you're deploying and deploy-rx command you're running?")]
    ActivateRsDoesntExist,
    #[error("Failed to run Nix sign command: {0}")]
    Sign(std::io::Error),
    #[error("Nix sign command resulted in a bad exit code: {0:?}")]
    SignExit(Option<i32>),
    #[error("Failed to run Nix copy command: {0}")]
    Copy(std::io::Error),
    #[error("Nix copy command resulted in a bad exit code: {0:?}")]
    CopyExit(Option<i32>),

    #[error("Failed to run Nix path-info command: {0}")]
    PathInfo(std::io::Error),
}

pub struct PushProfileData<'a> {
    pub supports_flakes: bool,
    pub check_sigs: bool,
    pub repo: &'a str,
    pub deploy_data: &'a super::DeployData<'a>,
    pub deploy_defs: &'a super::DeployDefs,
    pub keep_result: bool,
    pub result_path: Option<&'a str>,
    pub extra_build_args: &'a [String],
    pub build_tree: bool,
}

async fn command_exists(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok()
}

async fn run_build_command(
    mut build_command: Command,
    build_tree: bool,
) -> Result<(), PushProfileError> {
    debug!("build command: {:?}", build_command);

    if build_tree {
        if !command_exists("nom").await {
            warn!(
                "Build tree visualization requested but `nom` is not available in PATH; falling back to regular build logs"
            );
        } else {
            info!("Streaming build tree with nix-output-monitor (`nom`)");

            build_command
                .arg("--log-format")
                .arg("internal-json")
                .arg("--verbose")
                .stdout(Stdio::null())
                .stderr(Stdio::piped());

            let (nix_status, nom_status) =
                tokio::task::spawn_blocking(move || -> Result<_, PushProfileError> {
                    let mut nix_child = build_command
                        .into_std()
                        .spawn()
                        .map_err(PushProfileError::Build)?;

                    let nix_stderr = nix_child.stderr.take().ok_or_else(|| {
                        PushProfileError::Build(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            "failed to capture nix build stderr for nom",
                        ))
                    })?;

                    let nom_status = StdCommand::new("nom")
                        .arg("--json")
                        .stdin(Stdio::from(nix_stderr))
                        .stdout(Stdio::inherit())
                        .stderr(Stdio::inherit())
                        .status()
                        .map_err(PushProfileError::Build)?;

                    let nix_status = nix_child.wait().map_err(PushProfileError::Build)?;

                    Ok((nix_status, nom_status))
                })
                .await
                .map_err(|err| {
                    PushProfileError::Build(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("failed waiting for build tree process: {}", err),
                    ))
                })??;

            if nom_status.code() != Some(0) {
                warn!(
                    "`nom` exited with status {:?}; continuing based on Nix build result",
                    nom_status.code()
                );
            }

            return match nix_status.code() {
                Some(0) => Ok(()),
                a => Err(PushProfileError::BuildExit(a)),
            };
        }
    }

    let build_exit_status = build_command
        // Logging should be in stderr, this just stops the store path from printing for no reason
        .stdout(Stdio::null())
        .status()
        .await
        .map_err(PushProfileError::Build)?;

    match build_exit_status.code() {
        Some(0) => Ok(()),
        a => Err(PushProfileError::BuildExit(a)),
    }
}

pub async fn build_profile_remotely(
    data: &PushProfileData<'_>,
    derivation_name: &str,
) -> Result<(), PushProfileError> {
    info!(
        "Building profile `{}` for node `{}` on remote host",
        data.deploy_data.profile_name, data.deploy_data.node_name
    );

    // TODO: this should probably be handled more nicely during 'data' construction
    let hostname = match data.deploy_data.cmd_overrides.hostname {
        Some(ref x) => x,
        None => &data.deploy_data.node.node_settings.hostname,
    };
    let store_address = format!("ssh-ng://{}@{}", data.deploy_defs.ssh_user, hostname);

    let ssh_opts_str = data.deploy_data.merged_settings.ssh_opts.join(" ");

    // copy the derivation to remote host so it can be built there
    let copy_command_status = Command::new("nix")
        .arg("--experimental-features")
        .arg("nix-command")
        .arg("copy")
        .arg("-s") // fetch dependencies from substitures, not localhost
        .arg("--to")
        .arg(&store_address)
        .arg("--derivation")
        .arg(derivation_name)
        .env("NIX_SSHOPTS", ssh_opts_str.clone())
        .stdout(Stdio::null())
        .status()
        .await
        .map_err(PushProfileError::Copy)?;

    match copy_command_status.code() {
        Some(0) => (),
        a => return Err(PushProfileError::CopyExit(a)),
    };

    let mut build_command = Command::new("nix");
    build_command
        .arg("--experimental-features")
        .arg("nix-command")
        .arg("build")
        .arg(derivation_name)
        .arg("--eval-store")
        .arg("auto")
        .arg("--store")
        .arg(&store_address)
        .args(data.extra_build_args)
        .env("NIX_SSHOPTS", ssh_opts_str.clone());

    run_build_command(build_command, data.build_tree && data.supports_flakes).await?;

    Ok(())
}

/// Resolve the derivation path for a profile, returning the derivation name suitable for building.
pub async fn resolve_derivation(data: &PushProfileData<'_>) -> Result<String, PushProfileError> {
    debug!(
        "Finding the deriver of store path for {}",
        &data.deploy_data.profile.profile_settings.path
    );

    // `nix-store --query --deriver` doesn't work on invalid paths, so we parse output of show-derivation :(
    let show_derivation_output = Command::new("nix")
        .arg("--experimental-features")
        .arg("nix-command")
        .arg("show-derivation")
        .arg(&data.deploy_data.profile.profile_settings.path)
        .output()
        .await
        .map_err(PushProfileError::ShowDerivation)?;

    match show_derivation_output.status.code() {
        Some(0) => (),
        a => return Err(PushProfileError::ShowDerivationExit(a)),
    };

    let show_derivation_json: serde_json::value::Value = serde_json::from_str(
        std::str::from_utf8(&show_derivation_output.stdout)
            .map_err(PushProfileError::ShowDerivationUtf8)?,
    )
    .map_err(PushProfileError::ShowDerivationParse)?;

    // Nix 2.33+ nests derivations under a "derivations" key, so try to get that first
    let derivation_info = show_derivation_json
        .get("derivations")
        .unwrap_or(&show_derivation_json)
        .as_object()
        .ok_or(PushProfileError::ShowDerivationInvalid)?;

    let deriver_key = derivation_info
        .keys()
        .next()
        .ok_or(PushProfileError::ShowDerivationEmpty)?;

    // Nix 2.32+ returns relative paths (without /nix/store/ prefix) in show-derivation output
    // Normalize to always use full store paths
    let deriver = if deriver_key.starts_with("/nix/store/") {
        deriver_key.to_string()
    } else {
        format!("/nix/store/{}", deriver_key)
    };

    let new_deriver = if data.supports_flakes
        || data
            .deploy_data
            .merged_settings
            .remote_build
            .unwrap_or(false)
    {
        // Since nix 2.15.0 'nix build <path>.drv' will build only the .drv file itself, not the
        // derivation outputs, '^out' is used to refer to outputs explicitly
        deriver.clone() + "^out"
    } else {
        deriver.clone()
    };

    let path_info_output = Command::new("nix")
        .arg("--experimental-features")
        .arg("nix-command")
        .arg("path-info")
        .arg(&deriver)
        .output()
        .await
        .map_err(PushProfileError::PathInfo)?;

    let deriver = if std::str::from_utf8(&path_info_output.stdout).map(|s| s.trim())
        == Ok(deriver.as_str())
    {
        new_deriver
    } else {
        deriver
    };

    Ok(deriver)
}

/// Check that the built profile contains the expected activation scripts, and sign if needed.
pub async fn check_and_sign_profile(data: &PushProfileData<'_>) -> Result<(), PushProfileError> {
    if !Path::new(
        format!(
            "{}/deploy-rx-activate",
            data.deploy_data.profile.profile_settings.path
        )
        .as_str(),
    )
    .exists()
    {
        return Err(PushProfileError::DeployRsActivateDoesntExist);
    }

    if !Path::new(
        format!(
            "{}/activate-rs",
            data.deploy_data.profile.profile_settings.path
        )
        .as_str(),
    )
    .exists()
    {
        return Err(PushProfileError::ActivateRsDoesntExist);
    }

    if let Ok(local_key) = std::env::var("LOCAL_KEY") {
        info!(
            "Signing key present! Signing profile `{}` for node `{}`",
            data.deploy_data.profile_name, data.deploy_data.node_name
        );

        let sign_exit_status = Command::new("nix")
            .arg("sign-paths")
            .arg("-r")
            .arg("-k")
            .arg(local_key)
            .arg(&data.deploy_data.profile.profile_settings.path)
            .status()
            .await
            .map_err(PushProfileError::Sign)?;

        match sign_exit_status.code() {
            Some(0) => (),
            a => return Err(PushProfileError::SignExit(a)),
        };
    }

    Ok(())
}

struct BuildCommandInfo<'a> {
    node_name: &'a str,
    profile_name: &'a str,
}

fn make_build_command(
    supports_flakes: bool,
    keep_result: bool,
    result_path: Option<&str>,
    extra_build_args: &[String],
    derivations: &[&str],
    profiles: &[BuildCommandInfo],
) -> Command {
    let mut build_command = if supports_flakes {
        Command::new("nix")
    } else {
        Command::new("nix-build")
    };

    if supports_flakes {
        build_command.arg("build");
    }

    for derivation in derivations {
        build_command.arg(*derivation);
    }

    if !keep_result {
        if supports_flakes {
            build_command.arg("--no-link");
        } else {
            build_command.arg("--no-out-link");
        }
    } else {
        let result_path = result_path.unwrap_or("./.deploy-gc");
        for info in profiles {
            build_command.arg("--out-link").arg(format!(
                "{}/{}/{}",
                result_path, info.node_name, info.profile_name
            ));
        }
    }

    build_command.args(extra_build_args);

    build_command
}

/// Build multiple profiles locally in a single nix build invocation.
pub async fn build_profiles_locally(
    items: &[(&PushProfileData<'_>, &str)],
) -> Result<(), PushProfileError> {
    if items.is_empty() {
        return Ok(());
    }

    let data = items[0].0;

    // Validate that global build options are consistent across all items
    for (d, _) in &items[1..] {
        debug_assert_eq!(
            d.supports_flakes, data.supports_flakes,
            "All items must share the same supports_flakes value"
        );
        debug_assert_eq!(
            d.keep_result, data.keep_result,
            "All items must share the same keep_result value"
        );
        debug_assert_eq!(
            d.result_path, data.result_path,
            "All items must share the same result_path value"
        );
        debug_assert_eq!(
            d.extra_build_args, data.extra_build_args,
            "All items must share the same extra_build_args value"
        );
    }

    for (d, _) in items {
        info!(
            "Building profile `{}` for node `{}`",
            d.deploy_data.profile_name, d.deploy_data.node_name
        );
    }

    let derivations: Vec<&str> = items.iter().map(|&(_, d)| d).collect();
    let profiles: Vec<BuildCommandInfo> = items
        .iter()
        .map(|&(d, _)| BuildCommandInfo {
            node_name: d.deploy_data.node_name,
            profile_name: d.deploy_data.profile_name,
        })
        .collect();

    let build_command = make_build_command(
        data.supports_flakes,
        data.keep_result,
        data.result_path,
        data.extra_build_args,
        &derivations,
        &profiles,
    );

    if data.build_tree && !data.supports_flakes {
        warn!(
            "Build tree visualization currently requires flake-capable nix builds; continuing without tree output"
        );
    }

    run_build_command(build_command, data.build_tree && data.supports_flakes).await?;

    for &(d, _) in items {
        check_and_sign_profile(d).await?;
    }

    Ok(())
}

/// Resolve derivations, then build all profiles (dispatching remote vs local).
///
/// Remote profiles are built individually; local profiles are batched into a
/// single `nix build` invocation for efficiency.
pub async fn build_profiles(datas: &[PushProfileData<'_>]) -> Result<(), PushProfileError> {
    // Resolve derivations for every profile
    let mut derivations: Vec<String> = Vec::with_capacity(datas.len());
    for data in datas {
        let deriver = resolve_derivation(data).await?;
        derivations.push(deriver);
    }

    // Separate remote vs local, building remote ones immediately
    let mut local_builds: Vec<(&PushProfileData<'_>, &str)> = Vec::new();
    for (data, deriver) in datas.iter().zip(derivations.iter()) {
        if data
            .deploy_data
            .merged_settings
            .remote_build
            .unwrap_or(false)
        {
            if !data.supports_flakes {
                warn!("remote builds using non-flake nix are experimental");
            }
            build_profile_remotely(data, deriver).await?;
        } else {
            local_builds.push((data, deriver.as_str()));
        }
    }

    // Build all local profiles in a single nix build invocation
    if !local_builds.is_empty() {
        build_profiles_locally(&local_builds).await?;
    }

    Ok(())
}

pub async fn build_profile(data: PushProfileData<'_>) -> Result<(), PushProfileError> {
    build_profiles(&[data]).await
}

pub async fn push_profile(data: PushProfileData<'_>) -> Result<(), PushProfileError> {
    let ssh_opts_str = data
        .deploy_data
        .merged_settings
        .ssh_opts
        // This should provide some extra safety, but it also breaks for some reason, oh well
        // .iter()
        // .map(|x| format!("'{}'", x))
        // .collect::<Vec<String>>()
        .join(" ");

    // remote building guarantees that the resulting derivation is stored on the target system
    // no need to copy after building
    if !data
        .deploy_data
        .merged_settings
        .remote_build
        .unwrap_or(false)
    {
        info!(
            "Copying profile `{}` to node `{}`",
            data.deploy_data.profile_name, data.deploy_data.node_name
        );

        let mut copy_command = Command::new("nix");
        copy_command.arg("copy");

        if data.deploy_data.merged_settings.fast_connection != Some(true) {
            copy_command.arg("--substitute-on-destination");
        }

        if !data.check_sigs {
            copy_command.arg("--no-check-sigs");
        }

        let hostname = match data.deploy_data.cmd_overrides.hostname {
            Some(ref x) => x,
            None => &data.deploy_data.node.node_settings.hostname,
        };

        let copy_exit_status = copy_command
            .arg("--to")
            .arg(format!("ssh://{}@{}", data.deploy_defs.ssh_user, hostname))
            .arg(&data.deploy_data.profile.profile_settings.path)
            .env("NIX_SSHOPTS", ssh_opts_str)
            .status()
            .await
            .map_err(PushProfileError::Copy)?;

        match copy_exit_status.code() {
            Some(0) => (),
            a => return Err(PushProfileError::CopyExit(a)),
        };
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn get_args(cmd: &Command) -> Vec<String> {
        let std_cmd = cmd.as_std();
        std::iter::once(std_cmd.get_program())
            .chain(std_cmd.get_args())
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn test_make_build_command_flakes_single_derivation() {
        let cmd = make_build_command(
            true,
            false,
            None,
            &[],
            &["/nix/store/abc.drv^out"],
            &[],
        );
        assert_eq!(
            get_args(&cmd),
            vec!["nix", "build", "/nix/store/abc.drv^out", "--no-link"]
        );
    }

    #[test]
    fn test_make_build_command_flakes_multiple_derivations() {
        let cmd = make_build_command(
            true,
            false,
            None,
            &[],
            &["/nix/store/abc.drv^out", "/nix/store/def.drv^out"],
            &[],
        );
        assert_eq!(
            get_args(&cmd),
            vec![
                "nix",
                "build",
                "/nix/store/abc.drv^out",
                "/nix/store/def.drv^out",
                "--no-link"
            ]
        );
    }

    #[test]
    fn test_make_build_command_no_flakes_multiple_derivations() {
        let cmd = make_build_command(
            false,
            false,
            None,
            &[],
            &["/nix/store/abc.drv", "/nix/store/def.drv"],
            &[],
        );
        assert_eq!(
            get_args(&cmd),
            vec![
                "nix-build",
                "/nix/store/abc.drv",
                "/nix/store/def.drv",
                "--no-out-link"
            ]
        );
    }

    #[test]
    fn test_make_build_command_keep_result() {
        let profiles = vec![
            BuildCommandInfo {
                node_name: "node1",
                profile_name: "system",
            },
            BuildCommandInfo {
                node_name: "node2",
                profile_name: "system",
            },
        ];
        let cmd = make_build_command(
            true,
            true,
            Some("./results"),
            &[],
            &["/nix/store/abc.drv^out", "/nix/store/def.drv^out"],
            &profiles,
        );
        assert_eq!(
            get_args(&cmd),
            vec![
                "nix",
                "build",
                "/nix/store/abc.drv^out",
                "/nix/store/def.drv^out",
                "--out-link",
                "./results/node1/system",
                "--out-link",
                "./results/node2/system",
            ]
        );
    }

    #[test]
    fn test_make_build_command_keep_result_default_path() {
        let profiles = vec![BuildCommandInfo {
            node_name: "mynode",
            profile_name: "web",
        }];
        let cmd = make_build_command(
            true,
            true,
            None,
            &[],
            &["/nix/store/abc.drv^out"],
            &profiles,
        );
        assert_eq!(
            get_args(&cmd),
            vec![
                "nix",
                "build",
                "/nix/store/abc.drv^out",
                "--out-link",
                "./.deploy-gc/mynode/web",
            ]
        );
    }

    #[test]
    fn test_make_build_command_extra_args() {
        let extra = vec!["--option".to_string(), "foo".to_string(), "bar".to_string()];
        let cmd = make_build_command(
            true,
            false,
            None,
            &extra,
            &["/nix/store/abc.drv^out"],
            &[],
        );
        assert_eq!(
            get_args(&cmd),
            vec![
                "nix",
                "build",
                "/nix/store/abc.drv^out",
                "--no-link",
                "--option",
                "foo",
                "bar"
            ]
        );
    }

    fn empty_settings() -> crate::data::GenericSettings {
        crate::data::GenericSettings {
            ssh_user: None,
            user: None,
            ssh_opts: vec![],
            fast_connection: None,
            auto_rollback: None,
            confirm_timeout: None,
            activation_timeout: None,
            temp_path: None,
            magic_rollback: None,
            sudo: None,
            remote_build: None,
            interactive_sudo: None,
        }
    }

    fn empty_cmd_overrides() -> crate::CmdOverrides {
        crate::CmdOverrides {
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
        }
    }

    fn test_node() -> crate::data::Node {
        crate::data::Node {
            generic_settings: empty_settings(),
            node_settings: crate::data::NodeSettings {
                hostname: "example.com".to_string(),
                profiles: HashMap::new(),
                profiles_order: vec![],
            },
        }
    }

    fn test_deploy_defs() -> crate::DeployDefs {
        crate::DeployDefs {
            ssh_user: "root".to_string(),
            profile_user: "root".to_string(),
            sudo: None,
            sudo_password: None,
        }
    }

    #[test]
    fn test_check_and_sign_profile_missing_deploy_rx_activate() {
        let settings = empty_settings();
        let node = test_node();
        let profile = crate::data::Profile {
            profile_settings: crate::data::ProfileSettings {
                path: "/nonexistent/path".to_string(),
                profile_path: None,
            },
            generic_settings: empty_settings(),
        };
        let cmd_overrides = empty_cmd_overrides();
        let deploy_data = crate::make_deploy_data(
            &settings, &node, "testnode", &profile, "system", &cmd_overrides, false, None,
        );
        let deploy_defs = test_deploy_defs();
        let data = PushProfileData {
            supports_flakes: true,
            check_sigs: false,
            repo: ".",
            deploy_data: &deploy_data,
            deploy_defs: &deploy_defs,
            keep_result: false,
            result_path: None,
            extra_build_args: &[],
            build_tree: false,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(check_and_sign_profile(&data));
        assert!(matches!(
            result,
            Err(PushProfileError::DeployRsActivateDoesntExist)
        ));
    }

    #[test]
    fn test_check_and_sign_profile_missing_activate_rs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("deploy-rx-activate"), "").unwrap();

        let settings = empty_settings();
        let node = test_node();
        let profile = crate::data::Profile {
            profile_settings: crate::data::ProfileSettings {
                path: dir.path().to_string_lossy().into_owned(),
                profile_path: None,
            },
            generic_settings: empty_settings(),
        };
        let cmd_overrides = empty_cmd_overrides();
        let deploy_data = crate::make_deploy_data(
            &settings, &node, "testnode", &profile, "system", &cmd_overrides, false, None,
        );
        let deploy_defs = test_deploy_defs();
        let data = PushProfileData {
            supports_flakes: true,
            check_sigs: false,
            repo: ".",
            deploy_data: &deploy_data,
            deploy_defs: &deploy_defs,
            keep_result: false,
            result_path: None,
            extra_build_args: &[],
            build_tree: false,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(check_and_sign_profile(&data));
        assert!(matches!(
            result,
            Err(PushProfileError::ActivateRsDoesntExist)
        ));
    }

    #[test]
    fn test_check_and_sign_profile_success() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("deploy-rx-activate"), "").unwrap();
        std::fs::write(dir.path().join("activate-rs"), "").unwrap();

        let settings = empty_settings();
        let node = test_node();
        let profile = crate::data::Profile {
            profile_settings: crate::data::ProfileSettings {
                path: dir.path().to_string_lossy().into_owned(),
                profile_path: None,
            },
            generic_settings: empty_settings(),
        };
        let cmd_overrides = empty_cmd_overrides();
        let deploy_data = crate::make_deploy_data(
            &settings, &node, "testnode", &profile, "system", &cmd_overrides, false, None,
        );
        let deploy_defs = test_deploy_defs();
        let data = PushProfileData {
            supports_flakes: true,
            check_sigs: false,
            repo: ".",
            deploy_data: &deploy_data,
            deploy_defs: &deploy_defs,
            keep_result: false,
            result_path: None,
            extra_build_args: &[],
            build_tree: false,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(check_and_sign_profile(&data));
        assert!(result.is_ok());
    }
}
