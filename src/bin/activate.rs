// SPDX-FileCopyrightText: 2020 Serokell <https://serokell.io/>
// SPDX-FileCopyrightText: 2020 Andreas Fuchs <asf@boinkor.net>
// SPDX-FileCopyrightText: 2021 Yannik Sander <contact@ysndr.de>
//
// SPDX-License-Identifier: MPL-2.0

use signal_hook::{consts::signal::SIGHUP, iterator::Signals};

use clap::Parser;
use serde::de::DeserializeOwned;

use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::timeout;

use std::time::Duration;

use std::env;
use std::fmt::Write as FmtWrite;
use std::io::{Read as IoRead, Write as IoWrite};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use notify::{recommended_watcher, RecommendedWatcher, RecursiveMode, Watcher};

use thiserror::Error;

use log::{debug, error, info, warn};

use deploy::remote_protocol::{
    BootstrapRequest, ConfirmRequest, ProfileTarget, RemoteEvent, RemoteOperation,
    REMOTE_PROTOCOL_VERSION,
};

/// Remote activation utility for deploy-rx
#[derive(Parser, Debug)]
#[command(version = "1.0", author = "Serokell <https://serokell.io/>")]
struct Opts {
    /// Print debug logs to output
    #[arg(short, long)]
    debug_logs: bool,
    /// Directory to print logs to
    #[arg(long)]
    log_dir: Option<String>,

    #[command(subcommand)]
    subcmd: SubCommand,
}

#[derive(Parser, Debug)]
enum SubCommand {
    Activate(ActivateOpts),
    Wait(WaitOpts),
    Revoke(RevokeOpts),
    DryDiff(DryDiffOpts),
    BootstrapSession,
    PrivilegedSession(PrivilegedSessionOpts),
    ConfirmSession,
    WriteConfirmation(WriteConfirmationOpts),
}

#[derive(Parser, Debug)]
struct PrivilegedSessionOpts {
    #[arg(long)]
    request_path: PathBuf,
}

#[derive(Parser, Debug)]
struct WriteConfirmationOpts {
    #[arg(long)]
    path: PathBuf,
    #[arg(long)]
    nonce: String,
}

/// Activate a profile
#[derive(Parser, Debug)]
#[command(group(
    clap::ArgGroup::new("profile")
        .required(true)
        .multiple(false)
        .args(&["profile_path","profile_user"])
))]
struct ActivateOpts {
    /// The closure to activate
    closure: String,
    /// The profile path to install into
    #[arg(long)]
    profile_path: Option<String>,
    /// The profile user if explicit profile path is not specified
    #[arg(long, requires = "profile_name")]
    profile_user: Option<String>,
    /// The profile name
    #[arg(long, requires = "profile_user")]
    profile_name: Option<String>,

    /// Maximum time to wait for confirmation after activation
    #[arg(long)]
    confirm_timeout: u16,

    /// Wait for confirmation after deployment and rollback if not confirmed
    #[arg(long)]
    magic_rollback: bool,

    /// Auto rollback if failure
    #[arg(long)]
    auto_rollback: bool,

    /// Show what will be activated on the machines
    #[arg(long)]
    dry_activate: bool,

    /// Don't activate, but update the boot loader to boot into the new profile
    #[arg(long)]
    boot: bool,

    /// Path for any temporary files that may be needed during activation
    #[arg(long)]
    temp_path: PathBuf,
}

/// Wait for profile activation
#[derive(Parser, Debug)]
struct WaitOpts {
    /// The closure to wait for
    closure: String,

    /// Path for any temporary files that may be needed during activation
    #[arg(long)]
    temp_path: PathBuf,

    /// Timeout to wait for activation
    #[arg(long)]
    activation_timeout: Option<u16>,
}

/// Revoke profile activation
#[derive(Parser, Debug)]
struct RevokeOpts {
    /// The profile path to install into
    #[arg(long)]
    profile_path: Option<String>,
    /// The profile user if explicit profile path is not specified
    #[arg(long, requires = "profile_name")]
    profile_user: Option<String>,
    /// The profile name
    #[arg(long, requires = "profile_user")]
    profile_name: Option<String>,
}

/// Show derivation changes before activation
#[derive(Parser, Debug)]
pub struct DryDiffOpts {
    /// The new closure to compare against
    new_closure: String,
    /// The profile path to install into
    #[arg(long)]
    profile_path: Option<String>,
    /// The profile user if explicit profile path is not specified
    #[arg(long, requires = "profile_name")]
    profile_user: Option<String>,
    /// The profile name
    #[arg(long, requires = "profile_user")]
    profile_name: Option<String>,
}

#[derive(Error, Debug)]
pub enum DeactivateError {
    #[error("Failed to execute the rollback command: {0}")]
    Rollback(std::io::Error),
    #[error("The rollback resulted in a bad exit code: {0:?}")]
    RollbackExit(Option<i32>),
    #[error("Failed to run command for listing generations: {0}")]
    ListGen(std::io::Error),
    #[error("Command for listing generations resulted in a bad exit code: {0:?}")]
    ListGenExit(Option<i32>),
    #[error("Error converting generation list output to utf8: {0}")]
    DecodeListGenUtf8(std::string::FromUtf8Error),
    #[error("Failed to run command for deleting generation: {0}")]
    DeleteGen(std::io::Error),
    #[error("Command for deleting generations resulted in a bad exit code: {0:?}")]
    DeleteGenExit(Option<i32>),
    #[error("Failed to run command for re-activating the last generation: {0}")]
    Reactivate(std::io::Error),
    #[error("Command for re-activating the last generation resulted in a bad exit code: {0:?}")]
    ReactivateExit(Option<i32>),
}

pub async fn deactivate(profile_path: &str) -> Result<(), DeactivateError> {
    warn!("De-activating due to error");

    let nix_env_rollback_exit_status = Command::new("nix-env")
        .arg("-p")
        .arg(&profile_path)
        .arg("--rollback")
        .status()
        .await
        .map_err(DeactivateError::Rollback)?;

    match nix_env_rollback_exit_status.code() {
        Some(0) => (),
        a => return Err(DeactivateError::RollbackExit(a)),
    };

    debug!("Listing generations");

    let nix_env_list_generations_out = Command::new("nix-env")
        .arg("-p")
        .arg(&profile_path)
        .arg("--list-generations")
        .output()
        .await
        .map_err(DeactivateError::ListGen)?;

    match nix_env_list_generations_out.status.code() {
        Some(0) => (),
        a => return Err(DeactivateError::ListGenExit(a)),
    };

    let generations_list = String::from_utf8(nix_env_list_generations_out.stdout)
        .map_err(DeactivateError::DecodeListGenUtf8)?;

    let last_generation_line = generations_list
        .lines()
        .last()
        .expect("Expected to find a generation in list");

    let last_generation_id = last_generation_line
        .split_whitespace()
        .next()
        .expect("Expected to get ID from generation entry");

    debug!("Removing generation entry {}", last_generation_line);
    warn!("Removing generation by ID {}", last_generation_id);

    let nix_env_delete_generation_exit_status = Command::new("nix-env")
        .arg("-p")
        .arg(&profile_path)
        .arg("--delete-generations")
        .arg(last_generation_id)
        .status()
        .await
        .map_err(DeactivateError::DeleteGen)?;

    match nix_env_delete_generation_exit_status.code() {
        Some(0) => (),
        a => return Err(DeactivateError::DeleteGenExit(a)),
    };

    info!("Attempting to re-activate the last generation");

    let re_activate_exit_status = Command::new(format!("{}/deploy-rx-activate", profile_path))
        .env("PROFILE", &profile_path)
        .current_dir(&profile_path)
        .status()
        .await
        .map_err(DeactivateError::Reactivate)?;

    match re_activate_exit_status.code() {
        Some(0) => (),
        a => return Err(DeactivateError::ReactivateExit(a)),
    };

    Ok(())
}

#[derive(Error, Debug)]
pub enum ActivationConfirmationError {
    #[error("Failed to create activation confirmation directory: {0}")]
    CreateConfirmDir(std::io::Error),
    #[error("Failed to create activation confirmation file: {0}")]
    CreateConfirmFile(std::io::Error),
    #[error("Could not watch for activation sentinel: {0}")]
    Watcher(#[from] notify::Error),
    #[error("Error waiting for confirmation event: {0}")]
    WaitingError(#[from] DangerZoneError),
}

#[derive(Error, Debug)]
pub enum DangerZoneError {
    #[error("Timeout elapsed for confirmation")]
    TimesUp,
    #[error("inotify stream ended without activation confirmation")]
    NoConfirmation,
    #[error("inotify encountered an error: {0}")]
    Watch(notify::Error),
}

async fn danger_zone(
    mut events: mpsc::Receiver<Result<(), notify::Error>>,
    confirm_timeout: u16,
) -> Result<(), DangerZoneError> {
    info!("Waiting for confirmation event...");

    match timeout(Duration::from_secs(confirm_timeout as u64), events.recv()).await {
        Ok(Some(Ok(()))) => Ok(()),
        Ok(Some(Err(e))) => Err(DangerZoneError::Watch(e)),
        Ok(None) => Err(DangerZoneError::NoConfirmation),
        Err(_) => Err(DangerZoneError::TimesUp),
    }
}

pub async fn activation_confirmation(
    temp_path: PathBuf,
    confirm_timeout: u16,
    closure: String,
) -> Result<(), ActivationConfirmationError> {
    let lock_path = deploy::make_lock_path(&temp_path, &closure);

    debug!("Ensuring parent directory exists for canary file");

    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(ActivationConfirmationError::CreateConfirmDir)?;
    }

    debug!("Creating canary file");

    fs::File::create(&lock_path)
        .await
        .map_err(ActivationConfirmationError::CreateConfirmFile)?;

    debug!("Creating notify watcher");

    let (deleted, done) = mpsc::channel(1);

    let mut watcher: RecommendedWatcher =
        recommended_watcher(move |res: Result<notify::event::Event, notify::Error>| {
            let send_result = match res {
                Ok(e) if e.kind == notify::EventKind::Remove(notify::event::RemoveKind::File) => {
                    debug!("Got worthy removal event, sending on channel");
                    deleted.try_send(Ok(()))
                }
                Err(e) => {
                    debug!("Got error waiting for removal event, sending on channel");
                    deleted.try_send(Err(e))
                }
                Ok(_) => Ok(()), // ignore non-removal events
            };

            if let Err(e) = send_result {
                error!("Could not send file system event to watcher: {}", e);
            }
        })?;

    watcher.watch(&lock_path, RecursiveMode::NonRecursive)?;

    danger_zone(done, confirm_timeout)
        .await
        .map_err(|err| ActivationConfirmationError::WaitingError(err))
}

#[derive(Error, Debug)]
pub enum WaitError {
    #[error("Error creating watcher for activation: {0}")]
    Watcher(#[from] notify::Error),
    #[error("Error waiting for activation: {0}")]
    Waiting(#[from] DangerZoneError),
}
pub async fn wait(
    temp_path: PathBuf,
    closure: String,
    activation_timeout: Option<u16>,
) -> Result<(), WaitError> {
    let lock_path = deploy::make_lock_path(&temp_path, &closure);

    let (created, done) = mpsc::channel(1);

    let mut watcher: RecommendedWatcher = {
        // TODO: fix wasteful clone
        let lock_path = lock_path.clone();

        recommended_watcher(move |res: Result<notify::event::Event, notify::Error>| {
            let send_result = match res {
                Ok(e) if e.kind == notify::EventKind::Create(notify::event::CreateKind::File) => {
                    match &e.paths[..] {
                        [x] => match lock_path.canonicalize() {
                            // 'lock_path' may not exist yet when some other files are created in 'temp_path'
                            // x is already supposed to be canonical path
                            Ok(lock_path) if x == &lock_path => created.try_send(Ok(())),
                            _ => Ok(()),
                        },
                        _ => Ok(()),
                    }
                }
                Err(e) => created.try_send(Err(e)),
                Ok(_) => Ok(()), // ignore non-removal events
            };

            if let Err(e) = send_result {
                error!("Could not send file system event to watcher: {}", e);
            }
        })?
    };

    watcher.watch(&temp_path, RecursiveMode::NonRecursive)?;

    // Avoid a potential race condition by checking for existence after watcher creation
    if fs::metadata(&lock_path).await.is_ok() {
        watcher.unwatch(&temp_path)?;
        return Ok(());
    }

    danger_zone(done, activation_timeout.unwrap_or(240)).await?;

    info!("Found canary file, done waiting!");

    Ok(())
}

#[derive(Error, Debug)]
pub enum ActivateError {
    #[error("Failed to execute the command for setting profile: {0}")]
    SetProfile(std::io::Error),
    #[error("The command for setting profile resulted in a bad exit code: {0:?}")]
    SetProfileExit(Option<i32>),

    #[error("Failed to execute the activation script: {0}")]
    RunActivate(std::io::Error),
    #[error("The activation script resulted in a bad exit code: {0:?}")]
    RunActivateExit(Option<i32>),

    #[error("There was an error de-activating after an error was encountered: {0}")]
    Deactivate(#[from] DeactivateError),

    #[error("Failed to get activation confirmation: {0}")]
    ActivationConfirmation(#[from] ActivationConfirmationError),
}

pub async fn activate(
    profile_path: String,
    closure: String,
    auto_rollback: bool,
    temp_path: PathBuf,
    confirm_timeout: u16,
    magic_rollback: bool,
    dry_activate: bool,
    boot: bool,
) -> Result<(), ActivateError> {
    if !dry_activate {
        info!("Activating profile");
        // Only attempt a destructive rollback if `nix-env --set` actually advanced the profile to a
        // new generation. If `--set` fails without creating a new generation, rolling back here
        // would revert a previously-good deployment.
        let profile_link_before_set = std::fs::read_link(&profile_path).ok();
        let nix_env_set_exit_status = Command::new("nix-env")
            .arg("-p")
            .arg(&profile_path)
            .arg("--set")
            .arg(&closure)
            .status()
            .await
            .map_err(ActivateError::SetProfile)?;
        match nix_env_set_exit_status.code() {
            Some(0) => (),
            a => {
                let profile_link_after_set = std::fs::read_link(&profile_path).ok();
                let should_rollback = auto_rollback
                    && profile_link_before_set.is_some()
                    && profile_link_after_set.is_some()
                    && profile_link_before_set != profile_link_after_set;
                if should_rollback {
                    deactivate(&profile_path).await?;
                }
                return Err(ActivateError::SetProfileExit(a));
            }
        };
    }

    debug!("Running activation script");

    let activation_location = if dry_activate {
        &closure
    } else {
        &profile_path
    };

    let activate_status = match Command::new(format!("{}/deploy-rx-activate", activation_location))
        .env("PROFILE", activation_location)
        .env("DRY_ACTIVATE", if dry_activate { "1" } else { "0" })
        .env("BOOT", if boot { "1" } else { "0" })
        .current_dir(activation_location)
        .status()
        .await
        .map_err(ActivateError::RunActivate)
    {
        Ok(x) => x,
        Err(e) => {
            if auto_rollback && !dry_activate {
                deactivate(&profile_path).await?;
            }
            return Err(e);
        }
    };

    if !dry_activate {
        match activate_status.code() {
            Some(0) => (),
            a => {
                if auto_rollback {
                    deactivate(&profile_path).await?;
                }
                return Err(ActivateError::RunActivateExit(a));
            }
        };

        if !dry_activate {
            info!("Activation succeeded!");
        }

        if magic_rollback && !boot {
            info!("Magic rollback is enabled, setting up confirmation hook...");
            if let Err(err) = activation_confirmation(temp_path, confirm_timeout, closure).await {
                deactivate(&profile_path).await?;
                return Err(ActivateError::ActivationConfirmation(err));
            }
        }
    }

    Ok(())
}

async fn revoke(profile_path: String) -> Result<(), DeactivateError> {
    deactivate(profile_path.as_str()).await?;
    Ok(())
}

#[derive(Error, Debug)]
pub enum GetProfilePathError {
    #[error("Failed to deduce HOME directory for user {0}")]
    NoUserHome(String),
}

fn get_profile_path(
    profile_path: Option<String>,
    profile_user: Option<String>,
    profile_name: Option<String>,
) -> Result<String, GetProfilePathError> {
    match (profile_path, profile_user, profile_name) {
        (Some(profile_path), None, None) => Ok(profile_path),
        (None, Some(profile_user), Some(profile_name)) => {
            let nix_state_dir = env::var("NIX_STATE_DIR").unwrap_or("/nix/var/nix".to_string());
            // As per https://nixos.org/manual/nix/stable/command-ref/files/profiles#profiles
            match &profile_user[..] {
                "root" => {
                    match &profile_name[..] {
                        // NixOS system profile belongs to the root user, but isn't stored in the 'per-user/root'
                        "system" => Ok(format!("{}/profiles/system", nix_state_dir)),
                        // system-manager stores generations in a dedicated global profile path.
                        "system-manager" => Ok(format!(
                            "{}/profiles/system-manager-profiles/system-manager",
                            nix_state_dir
                        )),
                        _ => Ok(format!(
                            "{}/profiles/per-user/root/{}",
                            nix_state_dir, profile_name
                        )),
                    }
                }
                _ => {
                    let old_user_profiles_dir =
                        format!("{}/profiles/per-user/{}", nix_state_dir, profile_user);
                    // To stay backward compatible
                    if Path::new(&old_user_profiles_dir).exists() {
                        Ok(format!("{}/{}", old_user_profiles_dir, profile_name))
                    } else {
                        // https://github.com/NixOS/nix/blob/2.17.0/src/libstore/profiles.cc#L308
                        // This is basically the equivalent of calling 'dirs::state_dir()'.
                        // However, this function returns 'None' on macOS, while nix will actually
                        // check env variables, so we imitate nix implementation below instead of
                        // using 'dirs::state_dir()' directly.
                        let state_dir = env::var("XDG_STATE_HOME").or_else(|_| {
                            dirs::home_dir()
                                .map(|h| {
                                    format!("{}/.local/state", h.as_path().display().to_string())
                                })
                                .ok_or(GetProfilePathError::NoUserHome(profile_user))
                        })?;
                        Ok(format!("{}/nix/profiles/{}", state_dir, profile_name))
                    }
                }
            }
        }
        _ => panic!("impossible"),
    }
}

#[cfg(test)]
mod tests {
    use super::{get_profile_path, resolve_interactive_sudo_password, write_confirmation_file};
    use std::env;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn test_get_profile_path_for_root_system_profile() {
        let nix_state_dir = env::var("NIX_STATE_DIR").unwrap_or("/nix/var/nix".to_string());
        assert_eq!(
            get_profile_path(None, Some("root".to_string()), Some("system".to_string())).unwrap(),
            format!("{}/profiles/system", nix_state_dir)
        );
    }

    #[test]
    fn test_get_profile_path_for_root_system_manager_profile() {
        let nix_state_dir = env::var("NIX_STATE_DIR").unwrap_or("/nix/var/nix".to_string());
        assert_eq!(
            get_profile_path(
                None,
                Some("root".to_string()),
                Some("system-manager".to_string())
            )
            .unwrap(),
            format!(
                "{}/profiles/system-manager-profiles/system-manager",
                nix_state_dir
            )
        );
    }

    #[test]
    fn test_get_profile_path_for_other_root_profile() {
        let nix_state_dir = env::var("NIX_STATE_DIR").unwrap_or("/nix/var/nix".to_string());
        assert_eq!(
            get_profile_path(
                None,
                Some("root".to_string()),
                Some("custom-profile".to_string())
            )
            .unwrap(),
            format!("{}/profiles/per-user/root/custom-profile", nix_state_dir)
        );
    }

    #[test]
    fn test_write_confirmation_file_allows_repeated_matching_nonce() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("confirm");

        write_confirmation_file(&path, "nonce").unwrap();
        write_confirmation_file(&path, "nonce").unwrap();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "nonce\n");
    }

    #[test]
    fn test_write_confirmation_file_rejects_nonce_mismatch() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("confirm");

        write_confirmation_file(&path, "nonce").unwrap();

        assert_eq!(
            write_confirmation_file(&path, "other").unwrap_err(),
            "existing confirmation file has a different nonce"
        );
    }

    #[test]
    fn test_resolve_interactive_sudo_password_requires_password() {
        assert_eq!(
            resolve_interactive_sudo_password(
                true,
                true,
                None,
                "starting the privileged activation session"
            )
            .unwrap_err(),
            "interactive sudo requires sudo_password when starting the privileged activation session"
        );
    }

    #[test]
    fn test_resolve_interactive_sudo_password_returns_password_only_when_needed() {
        assert_eq!(
            resolve_interactive_sudo_password(true, true, Some("secret"), "confirming activation")
                .unwrap(),
            Some("secret")
        );
        assert_eq!(
            resolve_interactive_sudo_password(false, true, None, "confirming activation").unwrap(),
            None
        );
        assert_eq!(
            resolve_interactive_sudo_password(true, false, None, "confirming activation").unwrap(),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_write_confirmation_file_uses_private_permissions() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("confirm");

        write_confirmation_file(&path, "nonce").unwrap();

        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[derive(Error, Debug)]
pub enum DryDiffError {
    #[error("Failed to resolve profile path: {0}")]
    ProfilePath(#[from] GetProfilePathError),
    #[error("Failed to read current profile: {0}")]
    ReadProfile(std::io::Error),
    #[error("Failed to compute closure size: {0}")]
    SizeDiff(anyhow::Error),
    #[error("Failed to write package diff: {0}")]
    PackageDiff(anyhow::Error),
}

fn render_dry_diff(profile_path: &str, new_closure: &str) -> Result<String, DryDiffError> {
    if !Path::new(&profile_path).exists() {
        return Ok(format!(
            "No existing generation found at {}, skipping derivation diff.\n",
            profile_path
        ));
    }

    let old_generation = Path::new(&profile_path)
        .canonicalize()
        .map_err(DryDiffError::ReadProfile)?;
    let new_generation = PathBuf::from(new_closure)
        .canonicalize()
        .map_err(DryDiffError::ReadProfile)?;

    let mut output = String::new();
    writeln!(&mut output, "Derivation changes for {}:", profile_path).unwrap();

    // Use dix for the diff
    let size_handle = dix::spawn_size_diff(old_generation.clone(), new_generation.clone(), true);

    let wrote = dix::write_package_diff(&mut output, &old_generation, &new_generation, true)
        .map_err(DryDiffError::PackageDiff)?;

    if let Ok(Ok((size_old, size_new))) = size_handle.join() {
        if size_old == size_new {
            if wrote == 0 {
                output.push_str("No version or size changes.\n");
            }
        } else {
            if wrote > 0 {
                output.push('\n');
            }
            dix::write_size_diff(&mut output, size_old, size_new)
                .map_err(|e| DryDiffError::SizeDiff(e.into()))?;
        }
    }

    Ok(output)
}

pub async fn dry_diff(opts: DryDiffOpts) -> Result<(), DryDiffError> {
    let profile_path = get_profile_path(opts.profile_path, opts.profile_user, opts.profile_name)?;

    print!("{}", render_dry_diff(&profile_path, &opts.new_closure)?);

    Ok(())
}

#[derive(Error, Debug)]
enum SessionError {
    #[error("{message}")]
    Failed { message: String, rolled_back: bool },
}

impl SessionError {
    fn failed(message: impl Into<String>) -> Self {
        SessionError::Failed {
            message: message.into(),
            rolled_back: false,
        }
    }

    fn rolled_back(message: impl Into<String>) -> Self {
        SessionError::Failed {
            message: message.into(),
            rolled_back: true,
        }
    }

    fn did_rollback(&self) -> bool {
        match self {
            SessionError::Failed { rolled_back, .. } => *rolled_back,
        }
    }
}

async fn read_stdin_json<T: DeserializeOwned>() -> Result<T, Box<dyn std::error::Error>> {
    let mut input = String::new();
    let bytes = std::io::stdin().read_line(&mut input)?;

    if bytes == 0 {
        return Err("expected JSON request on stdin".into());
    }

    Ok(serde_json::from_str(input.trim())?)
}

fn resolve_interactive_sudo_password<'a>(
    interactive_sudo: bool,
    sudo_requested: bool,
    sudo_password: Option<&'a str>,
    context: &str,
) -> Result<Option<&'a str>, String> {
    if interactive_sudo && sudo_requested {
        return sudo_password
            .map(Some)
            .ok_or_else(|| format!("interactive sudo requires sudo_password when {}", context));
    }

    Ok(None)
}

fn send_event(event: &RemoteEvent) -> Result<(), Box<dyn std::error::Error>> {
    let mut stdout = std::io::stdout();
    serde_json::to_writer(&mut stdout, event)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn profile_path_from_target(target: ProfileTarget) -> Result<String, GetProfilePathError> {
    match target {
        ProfileTarget::ProfilePath { profile_path } => {
            get_profile_path(Some(profile_path), None, None)
        }
        ProfileTarget::ProfileUserAndName {
            profile_user,
            profile_name,
        } => get_profile_path(None, Some(profile_user), Some(profile_name)),
    }
}

fn random_token() -> Result<String, std::io::Error> {
    let mut bytes = [0_u8; 16];
    let mut file = std::fs::File::open("/dev/urandom")?;
    file.read_exact(&mut bytes)?;

    Ok(bytes.iter().map(|byte| format!("{:02x}", byte)).collect())
}

fn confirm_path(temp_path: &Path, session_id: &str) -> PathBuf {
    temp_path.join(format!("deploy-rx-confirm-{}", session_id))
}

#[cfg(unix)]
fn session_request_root() -> PathBuf {
    PathBuf::from("/tmp")
}

#[cfg(not(unix))]
fn session_request_root() -> PathBuf {
    env::temp_dir()
}

fn write_confirmation_file(path: &Path, nonce: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create confirmation directory: {}", err))?;
    }

    let mut open_options = std::fs::OpenOptions::new();
    open_options.write(true).create_new(true);
    #[cfg(unix)]
    open_options.mode(0o600);

    match open_options.open(path) {
        Ok(mut file) => {
            file.write_all(nonce.as_bytes())
                .map_err(|err| format!("failed to write confirmation file: {}", err))?;
            file.write_all(b"\n")
                .map_err(|err| format!("failed to write confirmation file: {}", err))?;
            file.flush()
                .map_err(|err| format!("failed to flush confirmation file: {}", err))?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = std::fs::read_to_string(path)
                .map_err(|err| format!("failed to read confirmation file: {}", err))?;
            if existing.trim() != nonce {
                return Err("existing confirmation file has a different nonce".to_string());
            }
        }
        Err(err) => return Err(format!("failed to create confirmation file: {}", err)),
    }

    Ok(())
}

async fn write_confirmation_via_sudo(
    request: &ConfirmRequest,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let sudo = request
        .sudo
        .as_ref()
        .expect("sudo request checked before writing confirmation");
    let sudo_password = resolve_interactive_sudo_password(
        request.interactive_sudo,
        true,
        request.sudo_password.as_deref(),
        "writing the activation confirmation file",
    )
    .map_err(|err| -> Box<dyn std::error::Error> { err.into() })?;
    let current_exe = env::current_exe()?;
    let mut sudo_argv = sudo.argv_for_user(&request.profile_user, request.interactive_sudo);
    let program = sudo_argv.remove(0);
    let mut command = Command::new(program);
    command
        .args(sudo_argv)
        .arg(current_exe)
        .arg("write-confirmation")
        .arg("--path")
        .arg(path)
        .arg("--nonce")
        .arg(&request.nonce)
        .stdin(if sudo_password.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    let mut child = command.spawn()?;
    if let Some(sudo_password) = sudo_password {
        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(format!("{}\n", sudo_password).as_bytes())
                .await?;
            stdin.shutdown().await?;
        }
    }

    let status = child.wait().await?;
    if !status.success() {
        return Err(format!(
            "sudo confirmation write failed with status {:?}",
            status.code()
        )
        .into());
    }

    Ok(())
}

async fn wait_for_session_confirmation(
    temp_path: &Path,
    session_id: &str,
    nonce: &str,
    confirm_timeout: u16,
) -> Result<(), String> {
    fs::create_dir_all(temp_path)
        .await
        .map_err(|err| format!("failed to create confirmation directory: {}", err))?;

    let path = confirm_path(temp_path, session_id);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(confirm_timeout as u64);

    loop {
        match fs::read_to_string(&path).await {
            Ok(contents) if contents.trim() == nonce => {
                let _ = fs::remove_file(&path).await;
                return Ok(());
            }
            Ok(_) => {
                return Err("confirmation nonce did not match".to_string());
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (),
            Err(err) => return Err(format!("failed to read confirmation file: {}", err)),
        }

        if tokio::time::Instant::now() >= deadline {
            return Err("timeout elapsed for confirmation".to_string());
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn command_status_to_stderr(
    mut command: Command,
    timeout_secs: Option<u16>,
) -> Result<Option<i32>, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = command
        .spawn()
        .map_err(|err| format!("failed to spawn command: {}", err))?;

    let output = match timeout_secs {
        Some(timeout_secs) => match timeout(
            Duration::from_secs(timeout_secs as u64),
            child.wait_with_output(),
        )
        .await
        {
            Ok(output) => output.map_err(|err| format!("failed to wait for command: {}", err))?,
            Err(_) => return Err(format!("command timed out after {} seconds", timeout_secs)),
        },
        None => child
            .wait_with_output()
            .await
            .map_err(|err| format!("failed to wait for command: {}", err))?,
    };

    std::io::stderr()
        .write_all(&output.stdout)
        .map_err(|err| format!("failed to forward command stdout: {}", err))?;
    std::io::stderr()
        .write_all(&output.stderr)
        .map_err(|err| format!("failed to forward command stderr: {}", err))?;

    Ok(output.status.code())
}

fn profile_generation_id_from_link_target(link_target: &Path) -> Option<String> {
    let file_name = link_target.file_name()?.to_str()?;
    let file_name = file_name.strip_suffix("-link")?;
    let (_, generation_id) = file_name.rsplit_once('-')?;

    generation_id
        .chars()
        .all(|ch| ch.is_ascii_digit())
        .then(|| generation_id.to_string())
}

fn current_profile_generation_id(profile_path: &str) -> Option<String> {
    let link_target = std::fs::read_link(profile_path).ok()?;
    profile_generation_id_from_link_target(&link_target)
}

fn current_profile_target(profile_path: &str) -> Option<PathBuf> {
    let mut target = PathBuf::from(profile_path);

    for _ in 0..8 {
        let metadata = std::fs::symlink_metadata(&target).ok()?;
        if !metadata.file_type().is_symlink() {
            return Some(target);
        }

        let link_target = std::fs::read_link(&target).ok()?;
        target = if link_target.is_absolute() {
            link_target
        } else {
            target.parent()?.join(link_target)
        };
    }

    None
}

fn resolve_previous_profile_target(profile_path: &str) -> Option<(PathBuf, bool)> {
    if let Some(target) = current_profile_target(profile_path) {
        return Some((target, false));
    }

    let system_profile_path =
        get_profile_path(None, Some("root".to_string()), Some("system".to_string())).ok()?;
    if profile_path != system_profile_path {
        return None;
    }

    current_profile_target("/run/current-system").map(|target| (target, true))
}

async fn delete_profile_generation(profile_path: &str, generation_id: &str) -> Result<(), String> {
    warn!("Removing generation by ID {}", generation_id);
    let mut delete_generation = Command::new("nix-env");
    delete_generation
        .arg("-p")
        .arg(profile_path)
        .arg("--delete-generations")
        .arg(generation_id);

    match command_status_to_stderr(delete_generation, None).await? {
        Some(0) => Ok(()),
        code => Err(format!(
            "deleting generations resulted in a bad exit code: {:?}",
            code
        )),
    }
}

async fn reactivate_profile(profile_path: &str) -> Result<(), String> {
    info!("Attempting to re-activate the last generation");
    let deploy_rx_activate = Path::new(profile_path).join("deploy-rx-activate");
    let switch_to_configuration = Path::new(profile_path)
        .join("bin")
        .join("switch-to-configuration");

    // When rolling back a NixOS system profile, the previous generation may be a vanilla
    // NixOS system closure without deploy-rx's activation helpers. Fall back to the
    // standard switch-to-configuration script in that case.
    let reactivate = if deploy_rx_activate.exists() {
        let mut cmd = Command::new(deploy_rx_activate);
        cmd.env("PROFILE", profile_path).current_dir(profile_path);
        cmd
    } else if switch_to_configuration.exists() {
        let mut cmd = Command::new(switch_to_configuration);
        cmd.arg("switch").current_dir("/tmp");
        cmd
    } else {
        return Err("no activation script found after rollback".to_string());
    };

    match command_status_to_stderr(reactivate, None).await? {
        Some(0) => Ok(()),
        code => Err(format!(
            "re-activating the last generation resulted in a bad exit code: {:?}",
            code
        )),
    }
}

async fn deactivate_session(
    profile_path: &str,
    previous_profile_target: Option<&Path>,
) -> Result<(), String> {
    warn!("De-activating due to error");

    let failed_generation_id = current_profile_generation_id(profile_path);

    if let Some(previous_profile_target) = previous_profile_target {
        info!(
            "Restoring previous profile target {}",
            previous_profile_target.display()
        );

        let mut restore_profile = Command::new("nix-env");
        restore_profile
            .arg("-p")
            .arg(profile_path)
            .arg("--set")
            .arg(previous_profile_target);

        match command_status_to_stderr(restore_profile, None).await? {
            Some(0) => (),
            code => {
                return Err(format!(
                    "restoring previous profile target resulted in a bad exit code: {:?}",
                    code
                ))
            }
        }
    } else {
        let mut rollback = Command::new("nix-env");
        rollback.arg("-p").arg(profile_path).arg("--rollback");

        match command_status_to_stderr(rollback, None).await? {
            Some(0) => (),
            code => return Err(format!("rollback resulted in a bad exit code: {:?}", code)),
        }
    }

    let restored_generation_id = current_profile_generation_id(profile_path);
    if let Some(failed_generation_id) = failed_generation_id {
        if restored_generation_id.as_deref() != Some(failed_generation_id.as_str()) {
            delete_profile_generation(profile_path, &failed_generation_id).await?;
        }
    }

    reactivate_profile(profile_path).await
}

async fn rollback_after_confirmation_failure(
    profile_path: &str,
    previous_profile_target: Option<&Path>,
    err: impl Into<String>,
) -> SessionError {
    let err = err.into();
    let rollback = deactivate_session(profile_path, previous_profile_target).await;
    SessionError::rolled_back(match rollback {
        Ok(()) => format!("confirmation failed: {}", err),
        Err(rollback_err) => format!(
            "confirmation failed: {}; rollback also failed: {}",
            err, rollback_err
        ),
    })
}

async fn process_deploy_session(
    request: deploy::remote_protocol::RemoteDeployRequest,
) -> Result<String, SessionError> {
    let profile_path = profile_path_from_target(request.profile.clone())
        .map_err(|err| SessionError::failed(format!("failed to resolve profile path: {}", err)))?;
    let previous_profile_target = if request.dry_activate {
        None
    } else {
        let previous_profile_target = resolve_previous_profile_target(&profile_path);
        if let Some((_, true)) = &previous_profile_target {
            info!(
                "System profile is not initialized yet; using /run/current-system as the rollback target"
            );
        } else if previous_profile_target.is_none() {
            warn!(
                "Could not resolve current profile target before activation; rollback will fall back to nix-env --rollback"
            );
        }
        previous_profile_target.map(|(target, _)| target)
    };

    if request.review_changes {
        match render_dry_diff(&profile_path, &request.closure) {
            Ok(output) => eprint!("{}", output),
            Err(err) => warn!(
                "Failed to review derivation changes before activation: {}",
                err
            ),
        }
    }

    if !request.dry_activate {
        info!("Activating profile");
        // Only attempt a destructive rollback if `nix-env --set` actually advanced the profile to a
        // new generation. If `--set` fails without creating a new generation, rolling back here
        // would revert a previously-good deployment.
        let profile_link_before_set = std::fs::read_link(&profile_path).ok();
        let mut set_profile = Command::new("nix-env");
        set_profile
            .arg("-p")
            .arg(&profile_path)
            .arg("--set")
            .arg(&request.closure);

        match command_status_to_stderr(set_profile, None).await {
            Ok(Some(0)) => (),
            Ok(code) => {
                let profile_link_after_set = std::fs::read_link(&profile_path).ok();
                let should_rollback = request.auto_rollback
                    && profile_link_before_set.is_some()
                    && profile_link_after_set.is_some()
                    && profile_link_before_set != profile_link_after_set;
                if should_rollback {
                    let _ =
                        deactivate_session(&profile_path, previous_profile_target.as_deref()).await;
                    return Err(SessionError::rolled_back(format!(
                        "setting profile resulted in a bad exit code: {:?}",
                        code
                    )));
                }
                return Err(SessionError::failed(format!(
                    "setting profile resulted in a bad exit code: {:?}",
                    code
                )));
            }
            Err(err) => {
                let profile_link_after_set = std::fs::read_link(&profile_path).ok();
                let should_rollback = request.auto_rollback
                    && profile_link_before_set.is_some()
                    && profile_link_after_set.is_some()
                    && profile_link_before_set != profile_link_after_set;
                if should_rollback {
                    let _ =
                        deactivate_session(&profile_path, previous_profile_target.as_deref()).await;
                    return Err(SessionError::rolled_back(err));
                }
                return Err(SessionError::failed(err));
            }
        }
    }

    debug!("Running activation script");
    let activation_location = if request.dry_activate {
        &request.closure
    } else {
        &profile_path
    };
    let mut activate = Command::new(format!("{}/deploy-rx-activate", activation_location));
    activate
        .env("PROFILE", activation_location)
        .env("DRY_ACTIVATE", if request.dry_activate { "1" } else { "0" })
        .env("BOOT", if request.boot { "1" } else { "0" })
        .current_dir(activation_location);

    // `activation_timeout` is independent of `magic_rollback`.
    // We only skip it for dry-activation runs, since those aren't actually activating the profile.
    let activation_timeout = if request.dry_activate {
        None
    } else {
        request.activation_timeout
    };

    match command_status_to_stderr(activate, activation_timeout).await {
        Ok(Some(0)) => (),
        Ok(code) if request.dry_activate => {
            warn!("dry activation script exited with status {:?}", code);
        }
        Ok(code) => {
            if request.auto_rollback {
                let _ = deactivate_session(&profile_path, previous_profile_target.as_deref()).await;
                return Err(SessionError::rolled_back(format!(
                    "activation script resulted in a bad exit code: {:?}",
                    code
                )));
            }
            return Err(SessionError::failed(format!(
                "activation script resulted in a bad exit code: {:?}",
                code
            )));
        }
        Err(err) if request.dry_activate => warn!("dry activation failed: {}", err),
        Err(err) => {
            if request.auto_rollback {
                let _ = deactivate_session(&profile_path, previous_profile_target.as_deref()).await;
                return Err(SessionError::rolled_back(err));
            }
            return Err(SessionError::failed(err));
        }
    }

    if !request.dry_activate {
        info!("Activation succeeded!");
    }

    if request.magic_rollback && !request.boot && !request.dry_activate {
        info!("Magic rollback is enabled, waiting for fresh SSH confirmation...");
        let temp_path = PathBuf::from(&request.temp_path);
        let session_id = random_token().map_err(|err| {
            SessionError::failed(format!("failed to generate session id: {}", err))
        })?;
        let nonce = random_token()
            .map_err(|err| SessionError::failed(format!("failed to generate nonce: {}", err)))?;

        if let Err(err) = send_event(&RemoteEvent::AwaitingConfirm {
            session_id: session_id.clone(),
            nonce: nonce.clone(),
        }) {
            return Err(rollback_after_confirmation_failure(
                &profile_path,
                previous_profile_target.as_deref(),
                format!("failed to send confirmation event: {}", err),
            )
            .await);
        }

        if let Err(err) =
            wait_for_session_confirmation(&temp_path, &session_id, &nonce, request.confirm_timeout)
                .await
        {
            return Err(rollback_after_confirmation_failure(
                &profile_path,
                previous_profile_target.as_deref(),
                err,
            )
            .await);
        }
    }

    Ok("deployment finished".to_string())
}

async fn process_revoke_session(
    request: deploy::remote_protocol::RemoteRevokeRequest,
) -> Result<String, SessionError> {
    let profile_path = profile_path_from_target(request.profile)
        .map_err(|err| SessionError::failed(format!("failed to resolve profile path: {}", err)))?;
    deactivate_session(&profile_path, None)
        .await
        .map_err(SessionError::failed)?;
    Ok("revoke finished".to_string())
}

async fn privileged_session(opts: PrivilegedSessionOpts) -> Result<(), Box<dyn std::error::Error>> {
    let request = fs::read_to_string(&opts.request_path).await?;
    let operation: RemoteOperation = serde_json::from_str(&request)?;
    let _ = fs::remove_file(&opts.request_path).await;

    send_event(&RemoteEvent::Hello {
        protocol_version: REMOTE_PROTOCOL_VERSION,
    })?;

    let result = match operation {
        RemoteOperation::Deploy(request) => process_deploy_session(request).await,
        RemoteOperation::Revoke(request) => process_revoke_session(request).await,
    };

    match result {
        Ok(message) => send_event(&RemoteEvent::Finished {
            ok: true,
            rolled_back: false,
            message,
        })?,
        Err(err) => send_event(&RemoteEvent::Finished {
            ok: false,
            rolled_back: err.did_rollback(),
            message: err.to_string(),
        })?,
    }

    Ok(())
}

async fn bootstrap_session() -> Result<(), Box<dyn std::error::Error>> {
    let request: BootstrapRequest = read_stdin_json().await?;
    if request.protocol_version != REMOTE_PROTOCOL_VERSION {
        return Err(format!(
            "remote protocol version mismatch: local request={}, remote={}",
            request.protocol_version, REMOTE_PROTOCOL_VERSION
        )
        .into());
    }

    // Keep the handoff request in a neutral system temp dir so `tempPath` only needs to work for
    // the target profile user during confirmation.
    let session_dir = session_request_root().join(format!(
        "deploy-rx-session-{}-{}",
        std::process::id(),
        random_token()?
    ));

    #[cfg(unix)]
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o711)
        .create(&session_dir)?;
    #[cfg(not(unix))]
    std::fs::create_dir_all(&session_dir)?;

    let request_path = session_dir.join("request.json");
    let request_json = serde_json::to_vec(&request.operation)?;
    let mut open_options = std::fs::OpenOptions::new();
    open_options.write(true).create_new(true);
    #[cfg(unix)]
    open_options.mode(0o644);
    let mut request_file = open_options.open(&request_path)?;
    request_file.write_all(&request_json)?;
    request_file.flush()?;
    drop(request_file);

    let current_exe = env::current_exe()?;
    let mut child_args = Vec::new();
    if request.operation.debug_logs() {
        child_args.push("--debug-logs".to_string());
    }
    if let Some(log_dir) = request.operation.log_dir() {
        child_args.push("--log-dir".to_string());
        child_args.push(log_dir.to_string());
    }
    child_args.push("privileged-session".to_string());
    child_args.push("--request-path".to_string());
    child_args.push(request_path.display().to_string());
    let sudo_password = resolve_interactive_sudo_password(
        request.interactive_sudo,
        request.sudo.is_some(),
        request.sudo_password.as_deref(),
        "starting the privileged activation session",
    )
    .map_err(|err| -> Box<dyn std::error::Error> { err.into() })?;

    let mut command = if let Some(sudo) = request.sudo {
        let mut sudo_argv =
            sudo.argv_for_user(request.operation.profile_user(), request.interactive_sudo);
        let program = sudo_argv.remove(0);
        let mut command = Command::new(program);
        command.args(sudo_argv).arg(current_exe);
        command
    } else {
        Command::new(current_exe)
    };

    command
        .args(child_args)
        .stdin(if sudo_password.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    let mut child = command.spawn()?;
    if let Some(sudo_password) = sudo_password {
        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(format!("{}\n", sudo_password).as_bytes())
                .await?;
            stdin.shutdown().await?;
        }
    }

    let status = child.wait().await?;
    let _ = fs::remove_dir_all(&session_dir).await;
    if !status.success() {
        return Err(format!("privileged session exited with status {:?}", status.code()).into());
    }

    Ok(())
}

async fn confirm_session() -> Result<(), Box<dyn std::error::Error>> {
    let request: ConfirmRequest = read_stdin_json().await?;
    let path = confirm_path(Path::new(&request.temp_path), &request.session_id);

    if request.sudo.is_some() {
        write_confirmation_via_sudo(&request, &path).await?;
    } else {
        write_confirmation_file(&path, &request.nonce)?;
    }

    Ok(())
}

fn write_confirmation(opts: WriteConfirmationOpts) -> Result<(), Box<dyn std::error::Error>> {
    write_confirmation_file(&opts.path, &opts.nonce)?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Ensure that this process stays alive after the SSH connection dies
    let mut signals = Signals::new(&[SIGHUP])?;
    std::thread::spawn(move || {
        for _ in signals.forever() {
            eprintln!("Received SIGHUP - ignoring...");
        }
    });

    let opts: Opts = Opts::parse();

    deploy::init_logger(
        opts.debug_logs,
        opts.log_dir.as_deref(),
        &match &opts.subcmd {
            SubCommand::Activate(_) => deploy::LoggerType::Activate,
            SubCommand::Wait(_) => deploy::LoggerType::Wait,
            SubCommand::Revoke(_) => deploy::LoggerType::Revoke,
            SubCommand::DryDiff(_) => deploy::LoggerType::Activate,
            SubCommand::BootstrapSession => deploy::LoggerType::Activate,
            SubCommand::PrivilegedSession(_) => deploy::LoggerType::Activate,
            SubCommand::ConfirmSession => deploy::LoggerType::Activate,
            SubCommand::WriteConfirmation(_) => deploy::LoggerType::Activate,
        },
    )?;

    let r = match opts.subcmd {
        SubCommand::Activate(activate_opts) => activate(
            get_profile_path(
                activate_opts.profile_path,
                activate_opts.profile_user,
                activate_opts.profile_name,
            )?,
            activate_opts.closure,
            activate_opts.auto_rollback,
            activate_opts.temp_path,
            activate_opts.confirm_timeout,
            activate_opts.magic_rollback,
            activate_opts.dry_activate,
            activate_opts.boot,
        )
        .await
        .map_err(|x| Box::new(x) as Box<dyn std::error::Error>),

        SubCommand::Wait(wait_opts) => wait(
            wait_opts.temp_path,
            wait_opts.closure,
            wait_opts.activation_timeout,
        )
        .await
        .map_err(|x| Box::new(x) as Box<dyn std::error::Error>),

        SubCommand::Revoke(revoke_opts) => revoke(get_profile_path(
            revoke_opts.profile_path,
            revoke_opts.profile_user,
            revoke_opts.profile_name,
        )?)
        .await
        .map_err(|x| Box::new(x) as Box<dyn std::error::Error>),

        SubCommand::DryDiff(dry_diff_opts) => dry_diff(dry_diff_opts)
            .await
            .map_err(|x| Box::new(x) as Box<dyn std::error::Error>),

        SubCommand::BootstrapSession => bootstrap_session().await,

        SubCommand::PrivilegedSession(privileged_session_opts) => {
            privileged_session(privileged_session_opts).await
        }

        SubCommand::ConfirmSession => confirm_session().await,

        SubCommand::WriteConfirmation(write_confirmation_opts) => {
            write_confirmation(write_confirmation_opts)
        }
    };

    match r {
        Ok(()) => (),
        Err(err) => {
            error!("{}", err);
            std::process::exit(1)
        }
    }

    Ok(())
}
