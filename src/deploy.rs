// SPDX-FileCopyrightText: 2020 Serokell <https://serokell.io/>
// SPDX-FileCopyrightText: 2020 Andreas Fuchs <asf@boinkor.net>
// SPDX-FileCopyrightText: 2021 Yannik Sander <contact@ysndr.de>
//
// SPDX-License-Identifier: MPL-2.0

use log::{info, warn};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::remote_protocol::{
    BootstrapRequest, ConfirmRequest, ProfileTarget, RemoteDeployRequest, RemoteEvent,
    RemoteOperation, RemoteRevokeRequest, REMOTE_PROTOCOL_VERSION,
};
use crate::{DeployDataDefsError, ProfileInfo};

fn profile_target(profile_info: ProfileInfo) -> ProfileTarget {
    match profile_info {
        ProfileInfo::ProfilePath { profile_path } => ProfileTarget::ProfilePath { profile_path },
        ProfileInfo::ProfileUserAndName {
            profile_user,
            profile_name,
        } => ProfileTarget::ProfileUserAndName {
            profile_user,
            profile_name,
        },
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[derive(Error, Debug)]
pub enum RemoteCommandError {
    #[error("remote closure path must be an absolute /nix/store path: {0}")]
    NotStorePath(String),
    #[error("remote closure path contains an unsupported NUL byte")]
    NulByte,
}

fn remote_activate_rs_command(
    closure: &str,
    subcommand: &str,
) -> Result<String, RemoteCommandError> {
    if !closure.starts_with("/nix/store/") {
        return Err(RemoteCommandError::NotStorePath(closure.to_string()));
    }

    if closure.contains('\0') {
        return Err(RemoteCommandError::NulByte);
    }

    Ok(format!(
        "{} {}",
        shell_quote(&format!("{}/activate-rs", closure)),
        subcommand
    ))
}

fn ssh_opts_without_control_master(ssh_opts: &[String]) -> Vec<String> {
    let mut filtered = Vec::new();
    let mut i = 0;

    while i < ssh_opts.len() {
        let ssh_opt = &ssh_opts[i];
        if ssh_opt == "-o" && i + 1 < ssh_opts.len() {
            let next = &ssh_opts[i + 1];
            if next.contains("ControlPath") || next.contains("ControlMaster") {
                i += 2;
                continue;
            }
        }

        if ssh_opt.contains("ControlPath") || ssh_opt.contains("ControlMaster") {
            i += 1;
            continue;
        }

        filtered.push(ssh_opt.clone());
        i += 1;
    }

    filtered
}

#[derive(Error, Debug)]
pub enum RemoteSessionError {
    #[error("failed to build remote command: {0}")]
    RemoteCommand(#[from] RemoteCommandError),
    #[error("failed to serialize remote session request: {0}")]
    Serialize(serde_json::Error),
    #[error("failed to spawn remote session over SSH: {0}")]
    Spawn(std::io::Error),
    #[error("failed to open stdin for remote session")]
    MissingStdin,
    #[error("failed to open stdout for remote session")]
    MissingStdout,
    #[error("failed to write remote session request: {0}")]
    WriteRequest(std::io::Error),
    #[error("failed to read remote session event: {0}")]
    ReadEvent(std::io::Error),
    #[error("failed to decode remote session event `{line}`: {source}")]
    DecodeEvent {
        line: String,
        source: serde_json::Error,
    },
    #[error("remote protocol version mismatch: local={local}, remote={remote}")]
    ProtocolVersion { local: u16, remote: u16 },
    #[error("failed to confirm activation over SSH: {0}")]
    Confirm(#[from] RemoteConfirmError),
    #[error("remote session failed: {0}")]
    RemoteFailed(String),
    #[error("remote session exited with a bad exit code: {0:?}")]
    RemoteExit(Option<i32>),
    #[error("remote session ended without a final status")]
    MissingFinished,
    #[error("failed to wait for remote session: {0}")]
    Wait(std::io::Error),
}

fn interpret_remote_session_completion(
    finished: Option<(bool, String)>,
    exit_success: bool,
    exit_code: Option<i32>,
) -> Result<(), RemoteSessionError> {
    match finished {
        Some((true, _)) => {
            if !exit_success {
                return Err(RemoteSessionError::RemoteExit(exit_code));
            }
            Ok(())
        }
        Some((false, message)) => Err(RemoteSessionError::RemoteFailed(message)),
        None => {
            if !exit_success {
                return Err(RemoteSessionError::RemoteExit(exit_code));
            }
            Err(RemoteSessionError::MissingFinished)
        }
    }
}

#[derive(Error, Debug)]
pub enum RemoteConfirmError {
    #[error("failed to build remote confirm command: {0}")]
    RemoteCommand(#[from] RemoteCommandError),
    #[error("failed to serialize remote confirm request: {0}")]
    Serialize(serde_json::Error),
    #[error("failed to spawn remote confirm over SSH: {0}")]
    Spawn(std::io::Error),
    #[error("failed to open stdin for remote confirm")]
    MissingStdin,
    #[error("failed to write remote confirm request: {0}")]
    WriteRequest(std::io::Error),
    #[error("remote confirm exited with a bad exit code: {0:?}")]
    Exit(Option<i32>),
    #[error("failed to wait for remote confirm: {0}")]
    Wait(std::io::Error),
}

struct RemoteConfirmData<'a> {
    deploy_data: &'a super::DeployData<'a>,
    deploy_defs: &'a super::DeployDefs,
    hostname: &'a str,
    closure: &'a str,
    temp_path: &'a Path,
    session_id: String,
    nonce: String,
    rollback_fresh_connection: bool,
}

async fn confirm_remote_session(data: RemoteConfirmData<'_>) -> Result<(), RemoteConfirmError> {
    let ssh_addr = format!("{}@{}", data.deploy_defs.ssh_user, data.hostname);
    let remote_command = remote_activate_rs_command(data.closure, "confirm-session")?;
    let confirm_request = ConfirmRequest {
        temp_path: data.temp_path.display().to_string(),
        session_id: data.session_id,
        nonce: data.nonce,
        sudo: data.deploy_defs.sudo.clone(),
        sudo_password: data.deploy_defs.sudo_password.clone(),
        interactive_sudo: data
            .deploy_data
            .merged_settings
            .interactive_sudo
            .unwrap_or(false),
        profile_user: data.deploy_defs.profile_user.clone(),
    };
    let confirm_request =
        serde_json::to_vec(&confirm_request).map_err(RemoteConfirmError::Serialize)?;

    let mut command = Command::new("ssh");
    command.arg(&ssh_addr).stdin(Stdio::piped());

    if data.rollback_fresh_connection {
        for ssh_opt in ssh_opts_without_control_master(&data.deploy_data.merged_settings.ssh_opts) {
            command.arg(ssh_opt);
        }
        command.arg("-o").arg("ControlPath=none");
    } else {
        for ssh_opt in &data.deploy_data.merged_settings.ssh_opts {
            command.arg(ssh_opt);
        }
    }

    let mut child = command
        .arg(remote_command)
        .spawn()
        .map_err(RemoteConfirmError::Spawn)?;

    let mut stdin = child.stdin.take().ok_or(RemoteConfirmError::MissingStdin)?;
    stdin
        .write_all(&confirm_request)
        .await
        .map_err(RemoteConfirmError::WriteRequest)?;
    stdin
        .write_all(b"\n")
        .await
        .map_err(RemoteConfirmError::WriteRequest)?;
    stdin
        .shutdown()
        .await
        .map_err(RemoteConfirmError::WriteRequest)?;

    let status = child.wait().await.map_err(RemoteConfirmError::Wait)?;

    if !status.success() {
        return Err(RemoteConfirmError::Exit(status.code()));
    }

    Ok(())
}

async fn run_remote_operation(
    deploy_data: &super::DeployData<'_>,
    deploy_defs: &super::DeployDefs,
    operation: RemoteOperation,
    rollback_fresh_connection: bool,
) -> Result<(), RemoteSessionError> {
    let hostname = match deploy_data.cmd_overrides.hostname {
        Some(ref x) => x,
        None => &deploy_data.node.node_settings.hostname,
    };
    let ssh_addr = format!("{}@{}", deploy_defs.ssh_user, hostname);
    let closure = match &operation {
        RemoteOperation::Deploy(request) => request.closure.clone(),
        RemoteOperation::Revoke(request) => request.closure.clone(),
    };
    let temp_path = PathBuf::from(operation.temp_path());
    let remote_command = remote_activate_rs_command(&closure, "bootstrap-session")?;

    let bootstrap_request = BootstrapRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        sudo: deploy_defs.sudo.clone(),
        sudo_password: deploy_defs.sudo_password.clone(),
        interactive_sudo: deploy_data
            .merged_settings
            .interactive_sudo
            .unwrap_or(false),
        operation,
    };
    let bootstrap_request =
        serde_json::to_vec(&bootstrap_request).map_err(RemoteSessionError::Serialize)?;

    let mut command = Command::new("ssh");
    command
        .arg(&ssh_addr)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    for ssh_opt in &deploy_data.merged_settings.ssh_opts {
        command.arg(ssh_opt);
    }

    let mut child = command
        .arg(remote_command)
        .spawn()
        .map_err(RemoteSessionError::Spawn)?;

    let mut stdin = child.stdin.take().ok_or(RemoteSessionError::MissingStdin)?;
    stdin
        .write_all(&bootstrap_request)
        .await
        .map_err(RemoteSessionError::WriteRequest)?;
    stdin
        .write_all(b"\n")
        .await
        .map_err(RemoteSessionError::WriteRequest)?;
    stdin
        .shutdown()
        .await
        .map_err(RemoteSessionError::WriteRequest)?;

    let stdout = child
        .stdout
        .take()
        .ok_or(RemoteSessionError::MissingStdout)?;
    let mut lines = BufReader::new(stdout).lines();
    let mut finished: Option<(bool, String)> = None;

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(RemoteSessionError::ReadEvent)?
    {
        if line.trim().is_empty() {
            continue;
        }

        let event: RemoteEvent =
            serde_json::from_str(&line).map_err(|source| RemoteSessionError::DecodeEvent {
                line: line.clone(),
                source,
            })?;

        match event {
            RemoteEvent::Hello { protocol_version } => {
                if protocol_version != REMOTE_PROTOCOL_VERSION {
                    return Err(RemoteSessionError::ProtocolVersion {
                        local: REMOTE_PROTOCOL_VERSION,
                        remote: protocol_version,
                    });
                }
            }
            RemoteEvent::AwaitingConfirm { session_id, nonce } => {
                info!("Activation is waiting for fresh SSH confirmation");
                if let Err(err) = confirm_remote_session(RemoteConfirmData {
                    deploy_data,
                    deploy_defs,
                    hostname,
                    closure: &closure,
                    temp_path: &temp_path,
                    session_id,
                    nonce,
                    rollback_fresh_connection,
                })
                .await
                {
                    warn!("Fresh SSH confirmation failed: {}", err);
                }
            }
            RemoteEvent::Finished {
                ok,
                rolled_back: _,
                message,
            } => {
                finished = Some((ok, message));
            }
        }
    }

    let status = child.wait().await.map_err(RemoteSessionError::Wait)?;

    // Prefer the descriptive error message from the `Finished` event when available.
    interpret_remote_session_completion(finished, status.success(), status.code())
}

#[derive(Error, Debug)]
pub enum DeployProfileError {
    #[error("Error running remote deployment session: {0}")]
    RemoteSession(#[from] RemoteSessionError),
    #[error("Deployment data invalid: {0}")]
    InvalidDeployDataDefs(#[from] DeployDataDefsError),
}

pub async fn deploy_profile(
    deploy_data: &super::DeployData<'_>,
    deploy_defs: &super::DeployDefs,
    dry_activate: bool,
    boot: bool,
    rollback_fresh_connection: bool,
    review_changes: bool,
) -> Result<(), DeployProfileError> {
    if !dry_activate {
        info!(
            "Activating profile `{}` for node `{}`",
            deploy_data.profile_name, deploy_data.node_name
        );
    }

    let temp_path: &Path = match &deploy_data.merged_settings.temp_path {
        Some(x) => x,
        None => Path::new("/tmp"),
    };

    let request = RemoteDeployRequest {
        closure: deploy_data.profile.profile_settings.path.clone(),
        profile: profile_target(deploy_data.get_profile_info()?),
        profile_user: deploy_defs.profile_user.clone(),
        review_changes,
        dry_activate,
        boot,
        auto_rollback: deploy_data.merged_settings.auto_rollback.unwrap_or(true),
        magic_rollback: deploy_data.merged_settings.magic_rollback.unwrap_or(true),
        confirm_timeout: deploy_data.merged_settings.confirm_timeout.unwrap_or(30),
        activation_timeout: deploy_data.merged_settings.activation_timeout,
        temp_path: temp_path.display().to_string(),
        debug_logs: deploy_data.debug_logs,
        log_dir: deploy_data.log_dir.map(|log_dir| log_dir.to_string()),
    };

    run_remote_operation(
        deploy_data,
        deploy_defs,
        RemoteOperation::Deploy(request),
        rollback_fresh_connection,
    )
    .await?;

    if dry_activate {
        info!("Completed dry-activate!");
    } else if boot {
        info!("Success activating for next boot, done!");
    } else {
        info!("Success activating, done!");
    }

    Ok(())
}

#[derive(Error, Debug)]
pub enum RevokeProfileError {
    #[error("Error running remote revoke session: {0}")]
    RemoteSession(#[from] RemoteSessionError),
    #[error("Deployment data invalid: {0}")]
    InvalidDeployDataDefs(#[from] DeployDataDefsError),
}

pub async fn revoke(
    deploy_data: &crate::DeployData<'_>,
    deploy_defs: &crate::DeployDefs,
) -> Result<(), RevokeProfileError> {
    let temp_path: &Path = match &deploy_data.merged_settings.temp_path {
        Some(x) => x,
        None => Path::new("/tmp"),
    };

    let request = RemoteRevokeRequest {
        closure: deploy_data.profile.profile_settings.path.clone(),
        profile: profile_target(deploy_data.get_profile_info()?),
        profile_user: deploy_defs.profile_user.clone(),
        temp_path: temp_path.display().to_string(),
        debug_logs: deploy_data.debug_logs,
        log_dir: deploy_data.log_dir.map(|log_dir| log_dir.to_string()),
    };

    run_remote_operation(
        deploy_data,
        deploy_defs,
        RemoteOperation::Revoke(request),
        true,
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_remote_activate_rs_path() {
        assert_eq!(
            remote_activate_rs_command("/nix/store/abc-profile", "bootstrap-session").unwrap(),
            "'/nix/store/abc-profile/activate-rs' bootstrap-session"
        );
    }

    #[test]
    fn rejects_non_store_remote_path() {
        assert!(matches!(
            remote_activate_rs_command("/tmp/profile", "bootstrap-session"),
            Err(RemoteCommandError::NotStorePath(_))
        ));
    }

    #[test]
    fn remote_session_prefers_finished_error_over_exit_code() {
        let err =
            interpret_remote_session_completion(Some((false, "boom".to_string())), false, Some(1))
                .unwrap_err();

        assert!(matches!(
            err,
            RemoteSessionError::RemoteFailed(message) if message == "boom"
        ));
    }
}
