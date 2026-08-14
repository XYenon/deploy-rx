// SPDX-FileCopyrightText: 2020 Serokell <https://serokell.io/>
//
// SPDX-License-Identifier: MPL-2.0

use log::{debug, info, warn};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::process::{Command as StdCommand, Stdio};
use thiserror::Error;
use tokio::process::Command;

use crate::command;

#[derive(Error, Debug)]
pub enum ShowDerivationError {
    #[error("Nix show-derivation command output contained an invalid UTF-8 sequence: {0}")]
    Utf8(std::str::Utf8Error),
    #[error("Failed to parse the output of nix show-derivation: {0}")]
    Parse(serde_json::Error),
    #[error("Nix show derivation output is not an object")]
    Invalid,
    #[error("Nix show-derivation output is empty")]
    Empty,
}

impl command::HasCommandError for ShowDerivationError {
    fn title() -> String {
        "Nix show derivation".to_string()
    }
}

#[derive(Error, Debug)]
pub enum BuildError {}
impl command::HasCommandError for BuildError {
    fn title() -> String {
        "Nix build".to_string()
    }
}

#[derive(Error, Debug)]
pub enum CopyError {}
impl command::HasCommandError for CopyError {
    fn title() -> String {
        "Nix copy".to_string()
    }
}

#[derive(Error, Debug)]
pub enum SignError {}
impl command::HasCommandError for SignError {
    fn title() -> String {
        "Nix sign".to_string()
    }
}

#[derive(Error, Debug)]
pub enum PathInfoError {}
impl command::HasCommandError for PathInfoError {
    fn title() -> String {
        "Nix path-info".to_string()
    }
}

#[derive(Error, Debug)]
pub enum StoreLsError {}
impl command::HasCommandError for StoreLsError {
    fn title() -> String {
        "Nix store ls".to_string()
    }
}

#[derive(Error, Debug)]
pub enum PushProfileError {
    #[error("{0}")]
    ShowDerivation(#[from] command::CommandError<ShowDerivationError>),
    #[error("{0}")]
    Build(#[from] command::CommandError<BuildError>),
    #[error(
        "Activation script deploy-rx-activate does not exist in profile.\n\
             Did you forget to use deploy-rx#lib.<...>.activate.<...> on your profile path?"
    )]
    DeployRsActivateDoesntExist,
    #[error("Activation script activate-rs does not exist in profile.\n\
             Is there a mismatch in deploy-rx used in the flake you're deploying and deploy-rx command you're running?")]
    ActivateRsDoesntExist,
    #[error("{0}")]
    Sign(#[from] command::CommandError<SignError>),
    #[error("{0}")]
    Copy(#[from] command::CommandError<CopyError>),
    #[error("Failed to run Nix copy command to {target} for {profiles}: {source}")]
    CopyGroup {
        nodes: String,
        target: String,
        profiles: String,
        source: Box<command::CommandError<CopyError>>,
    },

    #[error("{0}")]
    PathInfo(#[from] command::CommandError<PathInfoError>),
    #[error("{0}")]
    StoreLs(#[from] command::CommandError<StoreLsError>),
    #[error("Failed to parse the JSON output of nix store ls: {0}")]
    StoreLsParse(serde_json::Error),
    #[error("Nix build command output contained an invalid UTF-8 sequence: {0}")]
    BuildStdoutUtf8(std::str::Utf8Error),
    #[error("Nix build command succeeded but printed no output path")]
    BuildStdoutEmpty,
    #[error("Nix build command printed {actual} output paths, expected {expected}: {paths}")]
    BuildStdoutPathCount {
        actual: usize,
        expected: usize,
        paths: String,
    },
    #[error("Failed to parse the JSON output of nix build: {0}")]
    BuildStdoutParse(serde_json::Error),
    #[error("Nix build JSON output did not contain the requested derivation output {0}")]
    BuildStdoutMissingDerivation(String),
    #[error("Nix build JSON output contained an invalid derivation identity")]
    BuildStdoutInvalidDerivation,
    #[error("The legacy nix-build command cannot build the non-default `{0}` derivation output")]
    LegacyNonDefaultOutput(String),
    #[error("Failed to encode SSH options: {0}")]
    SshOptionsQuote(#[from] shlex::QuoteError),
}

impl PushProfileError {
    pub fn node_context(&self) -> Option<&str> {
        match self {
            PushProfileError::CopyGroup { nodes, .. } => Some(nodes.as_str()),
            _ => None,
        }
    }
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

async fn command_exists(command: &str, path: Option<&OsStr>) -> bool {
    let mut command = Command::new(command);
    if let Some(path) = path {
        command.env("PATH", path);
    }

    command
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
) -> Result<Vec<u8>, PushProfileError> {
    debug!("build command: {:?}", build_command);

    let path = build_command
        .as_std()
        .get_envs()
        .find(|(key, _)| *key == "PATH")
        .and_then(|(_, value)| value.map(|value| value.to_os_string()));

    if build_tree {
        if !command_exists("nom", path.as_deref()).await {
            warn!(
                "Build tree visualization requested but `nom` is not available in PATH; falling back to regular build logs"
            );
        } else {
            build_command
                .arg("--log-format")
                .arg("internal-json")
                .arg("--verbose")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            let (nix_status, nom_status, stdout) =
                tokio::task::spawn_blocking(move || -> Result<_, PushProfileError> {
                    let mut nix_child = build_command.into_std().spawn().map_err(|err| {
                        PushProfileError::Build(command::CommandError::RunError(err))
                    })?;

                    let mut nix_stdout = nix_child.stdout.take().ok_or_else(|| {
                        PushProfileError::Build(command::CommandError::RunError(
                            std::io::Error::other("failed to capture nix build stdout"),
                        ))
                    })?;
                    let stdout_task = std::thread::spawn(move || {
                        let mut stdout = Vec::new();
                        nix_stdout.read_to_end(&mut stdout).map(|_| stdout)
                    });
                    let nix_stderr = nix_child.stderr.take().ok_or_else(|| {
                        PushProfileError::Build(command::CommandError::RunError(
                            std::io::Error::other("failed to capture nix build stderr for nom"),
                        ))
                    })?;

                    let mut nom_command = StdCommand::new("nom");
                    if let Some(path) = path {
                        nom_command.env("PATH", path);
                    }

                    let nom_status = nom_command
                        .arg("--json")
                        .stdin(Stdio::from(nix_stderr))
                        .stdout(Stdio::inherit())
                        .stderr(Stdio::inherit())
                        .status()
                        .map_err(|err| {
                            PushProfileError::Build(command::CommandError::RunError(err))
                        })?;

                    let nix_status = nix_child.wait().map_err(|err| {
                        PushProfileError::Build(command::CommandError::RunError(err))
                    })?;
                    let stdout = stdout_task
                        .join()
                        .map_err(|_| {
                            PushProfileError::Build(command::CommandError::RunError(
                                std::io::Error::other("failed joining nix build stdout reader"),
                            ))
                        })?
                        .map_err(|err| {
                            PushProfileError::Build(command::CommandError::RunError(err))
                        })?;

                    Ok((nix_status, nom_status, stdout))
                })
                .await
                .map_err(|err| {
                    PushProfileError::Build(command::CommandError::RunError(std::io::Error::other(
                        format!("failed waiting for build tree process: {}", err),
                    )))
                })??;

            if nom_status.code() != Some(0) {
                warn!(
                    "`nom` exited with status {:?}; continuing based on Nix build result",
                    nom_status.code()
                );
            }

            return match nix_status.code() {
                Some(0) => Ok(stdout),
                a => Err(PushProfileError::Build(command::CommandError::RunError(
                    std::io::Error::other(format!(
                        "Nix build command resulted in a bad exit code: {:?}",
                        a
                    )),
                ))),
            };
        }
    }

    build_command
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let output = command::Command::new(build_command)
        .run()
        .await
        .map_err(PushProfileError::Build)?;

    Ok(output.stdout)
}

fn make_remote_derivation_copy_command(
    store_address: &str,
    ssh_opts: &str,
    derivation_name: &str,
) -> Command {
    // A nested dynamic installable such as `outer.drv^out^out` must be
    // discovered on the build host. Copying only the concrete outer `.drv`
    // avoids realising the intermediate derivation locally.
    let outer_derivation = derivation_name.split('^').next().unwrap_or(derivation_name);
    let mut copy_command = Command::new("nix");
    copy_command
        .arg("--experimental-features")
        .arg("nix-command")
        .arg("copy")
        .arg("-s") // fetch dependencies from substitutes, not localhost
        .arg("--to")
        .arg(store_address)
        .arg("--derivation")
        .arg(outer_derivation)
        .env("NIX_SSHOPTS", ssh_opts)
        .stdout(Stdio::null());

    copy_command
}

fn make_remote_build_command(
    store_address: &str,
    ssh_opts: &str,
    derivation_name: &str,
    extra_build_args: &[String],
) -> Command {
    let mut build_command = Command::new("nix");
    build_command
        .arg("--experimental-features")
        .arg("nix-command")
        .arg("build")
        .arg(derivation_name)
        .arg("--eval-store")
        .arg("auto")
        .arg("--store")
        .arg(store_address)
        .arg("--json")
        .args(extra_build_args)
        .env("NIX_SSHOPTS", ssh_opts);

    build_command
}

fn encode_ssh_opts(ssh_opts: &[String]) -> Result<String, PushProfileError> {
    Ok(shlex::try_join(ssh_opts.iter().map(String::as_str))?)
}

fn remote_store(data: &PushProfileData<'_>) -> Result<(String, String), PushProfileError> {
    let hostname = match data.deploy_data.cmd_overrides.hostname {
        Some(ref x) => x,
        None => &data.deploy_data.node.node_settings.hostname,
    };

    Ok((
        format!("ssh-ng://{}@{}", data.deploy_defs.ssh_user, hostname),
        encode_ssh_opts(&data.deploy_data.merged_settings.ssh_opts)?,
    ))
}

pub async fn build_profile_remotely(
    data: &PushProfileData<'_>,
    derivation_name: &str,
) -> Result<String, PushProfileError> {
    info!(
        "Building profile `{}.{}` on remote host",
        data.deploy_data.node_name, data.deploy_data.profile_name
    );

    let (store_address, ssh_opts_str) = remote_store(data)?;

    // copy the derivation to remote host so it can be built there
    command::Command::new(make_remote_derivation_copy_command(
        &store_address,
        &ssh_opts_str,
        derivation_name,
    ))
    .status()
    .await
    .map_err(PushProfileError::Copy)?;

    let build_command = make_remote_build_command(
        &store_address,
        &ssh_opts_str,
        derivation_name,
        data.extra_build_args,
    );

    let stdout = run_build_command(build_command, data.build_tree && data.supports_flakes).await?;

    parse_build_json(&stdout, &[derivation_name]).map(|mut paths| paths.remove(0))
}

fn make_remote_store_ls_command(store_address: &str, ssh_opts: &str, path: &str) -> Command {
    let mut command = Command::new("nix");
    command
        .arg("--experimental-features")
        .arg("nix-command")
        .arg("store")
        .arg("ls")
        .arg("--json")
        .arg("--store")
        .arg(store_address)
        .arg(path)
        .env("NIX_SSHOPTS", ssh_opts)
        .stdout(Stdio::null());
    command
}

fn make_remote_sign_command(
    store_address: &str,
    ssh_opts: &str,
    key_file: &str,
    closure: &str,
) -> Command {
    let mut command = Command::new("nix");
    command
        .arg("--experimental-features")
        .arg("nix-command")
        .arg("store")
        .arg("sign")
        .arg("--store")
        .arg(store_address)
        .arg("--recursive")
        .arg("--key-file")
        .arg(key_file)
        .arg(closure)
        .env("NIX_SSHOPTS", ssh_opts);
    command
}

async fn check_and_sign_remote_profile(
    data: &PushProfileData<'_>,
    closure: &str,
) -> Result<(), PushProfileError> {
    let (store_address, ssh_opts) = remote_store(data)?;

    // Fetch both activation-script entries in one request so a later transport
    // failure cannot be misclassified as a missing script.
    let output = command::Command::new(make_remote_store_ls_command(
        &store_address,
        &ssh_opts,
        closure,
    ))
    .run()
    .await
    .map_err(PushProfileError::StoreLs)?;
    let listing: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(PushProfileError::StoreLsParse)?;
    let entries = listing
        .get("entries")
        .and_then(serde_json::Value::as_object);

    if !entries.is_some_and(|entries| entries.contains_key("deploy-rx-activate")) {
        return Err(PushProfileError::DeployRsActivateDoesntExist);
    }
    if !entries.is_some_and(|entries| entries.contains_key("activate-rs")) {
        return Err(PushProfileError::ActivateRsDoesntExist);
    }

    if let Ok(local_key) = std::env::var("LOCAL_KEY") {
        info!(
            "Signing key present! Signing profile `{}` for node `{}`",
            data.deploy_data.profile_name, data.deploy_data.node_name
        );
        command::Command::new(make_remote_sign_command(
            &store_address,
            &ssh_opts,
            &local_key,
            closure,
        ))
        .status()
        .await
        .map_err(PushProfileError::Sign)?;
    }

    Ok(())
}

/// Resolve the derivation path for a profile, returning the derivation name suitable for building.
pub async fn resolve_derivation(data: &PushProfileData<'_>) -> Result<String, PushProfileError> {
    let profile_settings = &data.deploy_data.profile.profile_settings;
    let supports_caret = data.supports_flakes
        || data
            .deploy_data
            .merged_settings
            .remote_build
            .unwrap_or(false);

    // The eval transformation in `nix/transform-deploy.nix` attaches `drvPath`
    // to every derivation-typed profile path, so this branch is hit whenever
    // the user's `path` resolves to a derivation. Using `drvPath` directly also
    // bypasses `nix show-derivation`, which cannot resolve floating-output
    // placeholder paths. The legacy branch below remains for the case where
    // the user wrote a literal store path string in their `deploy` attribute.
    if let Some(drv_path) = &profile_settings.drv_path {
        debug!("Using drvPath from flake: {}", drv_path);
        return deriver_for_build(
            drv_path.clone(),
            &profile_settings.output_name,
            supports_caret,
        )
        .await;
    }

    debug!(
        "Finding the deriver of store path for {}",
        &profile_settings.path
    );

    // `nix-store --query --deriver` doesn't work on invalid paths, so we parse output of show-derivation :(
    let mut show_derivation_command = Command::new("nix");
    show_derivation_command
        .arg("--experimental-features")
        .arg("nix-command")
        .arg("show-derivation")
        .arg(&profile_settings.path);

    let show_derivation_output = command::Command::new(show_derivation_command)
        .run()
        .await
        .map_err(PushProfileError::ShowDerivation)?;

    let show_derivation_json: serde_json::value::Value = serde_json::from_str(
        std::str::from_utf8(&show_derivation_output.stdout).map_err(|err| {
            PushProfileError::ShowDerivation(command::CommandError::OtherError(
                ShowDerivationError::Utf8(err),
            ))
        })?,
    )
    .map_err(|err| {
        PushProfileError::ShowDerivation(command::CommandError::OtherError(
            ShowDerivationError::Parse(err),
        ))
    })?;

    // Nix 2.33+ nests derivations under a "derivations" key, so try to get that first
    let derivation_info = show_derivation_json
        .get("derivations")
        .unwrap_or(&show_derivation_json)
        .as_object()
        .ok_or(PushProfileError::ShowDerivation(
            command::CommandError::OtherError(ShowDerivationError::Invalid),
        ))?;

    let deriver_key = derivation_info
        .keys()
        .next()
        .ok_or(PushProfileError::ShowDerivation(
            command::CommandError::OtherError(ShowDerivationError::Empty),
        ))?;

    // Nix 2.32+ returns relative paths (without /nix/store/ prefix) in show-derivation output
    // Normalize to always use full store paths
    let deriver = if deriver_key.starts_with("/nix/store/") {
        deriver_key.to_string()
    } else {
        format!("/nix/store/{}", deriver_key)
    };

    deriver_for_build(deriver, &profile_settings.output_name, supports_caret).await
}

/// Picks the `nix build` argument shape for a given deriver, accounting for the
/// pre/post 2.15 split: on 2.15 and newer, `nix build <drv>` builds only the
/// `.drv` itself and `^out` is needed to select outputs; on older Nix,
/// `nix build <drv>` already builds outputs and `^out` is not understood. We
/// detect which case applies by asking `nix path-info <drv>`; on 2.15 and newer
/// it echoes the `.drv` back, while on older versions it resolves to the
/// realised output or errors out if the output is not yet built.
async fn deriver_for_build(
    deriver: String,
    output_name: &str,
    supports_caret: bool,
) -> Result<String, PushProfileError> {
    if !supports_caret {
        if output_name != "out" {
            return Err(PushProfileError::LegacyNonDefaultOutput(
                output_name.to_string(),
            ));
        }
        return Ok(deriver);
    }

    let mut path_info_command = Command::new("nix");
    path_info_command
        .arg("--experimental-features")
        .arg("nix-command")
        .arg("path-info")
        .arg(&deriver);
    let path_info_output = command::Command::new(path_info_command)
        .run()
        .await
        .map_err(PushProfileError::PathInfo)?;

    if std::str::from_utf8(&path_info_output.stdout).map(|s| s.trim()) == Ok(deriver.as_str()) {
        Ok(format!("{}^{}", deriver, output_name))
    } else if output_name != "out" {
        Err(PushProfileError::LegacyNonDefaultOutput(
            output_name.to_string(),
        ))
    } else {
        Ok(deriver)
    }
}

/// Check that the built profile contains the expected activation scripts, and sign if needed.
pub async fn check_and_sign_profile(
    data: &PushProfileData<'_>,
    closure: &str,
) -> Result<(), PushProfileError> {
    if !Path::new(format!("{}/deploy-rx-activate", closure).as_str()).exists() {
        return Err(PushProfileError::DeployRsActivateDoesntExist);
    }

    if !Path::new(format!("{}/activate-rs", closure).as_str()).exists() {
        return Err(PushProfileError::ActivateRsDoesntExist);
    }

    if let Ok(local_key) = std::env::var("LOCAL_KEY") {
        info!(
            "Signing key present! Signing profile `{}` for node `{}`",
            data.deploy_data.profile_name, data.deploy_data.node_name
        );

        let mut sign_command = Command::new("nix");
        sign_command
            .arg("sign-paths")
            .arg("-r")
            .arg("-k")
            .arg(local_key)
            .arg(closure);
        command::Command::new(sign_command)
            .status()
            .await
            .map_err(PushProfileError::Sign)?;
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
        // JSON associates each realised output with its derivation identity,
        // avoiding any dependency on the order of `--print-out-paths` lines.
        // `nix-build` writes output paths to stdout by default, so the legacy
        // branch continues to use its plain-text output.
        build_command.arg("build").arg("--json");
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
        let out_link = match profiles {
            [info] => Path::new(result_path)
                .join(info.node_name)
                .join(info.profile_name),
            _ => Path::new(result_path).join("profiles"),
        };

        build_command.arg("--out-link").arg(out_link);
    }

    build_command.args(extra_build_args);

    build_command
}

/// Extracts the realised `/nix/store/...` paths from `nix build`'s stdout.
///
/// Both `nix build --print-out-paths` and `nix-build` write one path per
/// line. A batched deploy-rx build asks for one output per profile, so any
/// other number is rejected rather than silently misassigning closures.
fn parse_build_out_paths(stdout: &[u8], expected: usize) -> Result<Vec<String>, PushProfileError> {
    let text = std::str::from_utf8(stdout).map_err(PushProfileError::BuildStdoutUtf8)?;
    let trimmed = text.trim();

    if trimmed.is_empty() {
        return Err(PushProfileError::BuildStdoutEmpty);
    }

    let paths: Vec<String> = trimmed
        .lines()
        .map(|line| line.trim().to_string())
        .collect();
    if paths.len() != expected {
        return Err(PushProfileError::BuildStdoutPathCount {
            actual: paths.len(),
            expected,
            paths: trimmed.to_string(),
        });
    }

    for path in &paths {
        debug!("Built closure {}", path);
    }
    Ok(paths)
}

fn json_drv_path(value: &serde_json::Value) -> Result<String, PushProfileError> {
    if let Some(path) = value.as_str() {
        return Ok(path.to_string());
    }

    let object = value
        .as_object()
        .ok_or(PushProfileError::BuildStdoutInvalidDerivation)?;
    let drv_path = object
        .get("drvPath")
        .ok_or(PushProfileError::BuildStdoutInvalidDerivation)?;
    let output = object
        .get("output")
        .and_then(serde_json::Value::as_str)
        .ok_or(PushProfileError::BuildStdoutInvalidDerivation)?;

    Ok(format!("{}^{}", json_drv_path(drv_path)?, output))
}

/// Extracts realised outputs from `nix build --json`, associating them by
/// derivation identity rather than by array position. Recursive `drvPath`
/// objects represent nested dynamic derivations and are converted back to
/// their textual `outer.drv^output` form for matching.
fn parse_build_json(stdout: &[u8], derivations: &[&str]) -> Result<Vec<String>, PushProfileError> {
    let results: Vec<serde_json::Value> =
        serde_json::from_slice(stdout).map_err(PushProfileError::BuildStdoutParse)?;
    let mut realised_outputs = HashMap::new();

    for result in results {
        let drv_path = result
            .get("drvPath")
            .ok_or(PushProfileError::BuildStdoutInvalidDerivation)?;
        let identity = json_drv_path(drv_path)?;
        let outputs = result
            .get("outputs")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| PushProfileError::BuildStdoutMissingDerivation(identity.clone()))?;
        for (output_name, output_path) in outputs {
            let output_path = output_path
                .as_str()
                .ok_or_else(|| PushProfileError::BuildStdoutMissingDerivation(identity.clone()))?;
            realised_outputs.insert(
                (identity.clone(), output_name.clone()),
                output_path.to_string(),
            );
        }
    }

    derivations
        .iter()
        .map(|derivation| {
            let (drv_path, output_name) =
                derivation.rsplit_once('^').unwrap_or((derivation, "out"));
            realised_outputs
                .get(&(drv_path.to_string(), output_name.to_string()))
                .cloned()
                .ok_or_else(|| {
                    PushProfileError::BuildStdoutMissingDerivation((*derivation).to_string())
                })
        })
        .collect()
}

/// Build multiple profiles locally in a single nix build invocation.
pub async fn build_profiles_locally(
    items: &[(&PushProfileData<'_>, &str)],
) -> Result<Vec<String>, PushProfileError> {
    if items.is_empty() {
        return Ok(Vec::new());
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

    let profiles_str = items
        .iter()
        .map(|(d, _)| {
            format!(
                "`{}.{}`",
                d.deploy_data.node_name, d.deploy_data.profile_name
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    info!(
        "Building {} {}: {}",
        items.len(),
        if items.len() > 1 {
            "profiles"
        } else {
            "profile"
        },
        profiles_str
    );

    // Nix implementations may multiply results for repeated installables.
    // Build each derivation once, then map its realised closure back to every
    // profile that requested it.
    let mut derivations: Vec<&str> = Vec::new();
    let derivation_indexes: Vec<usize> = items
        .iter()
        .map(|&(_, derivation)| {
            if let Some(index) = derivations.iter().position(|&item| item == derivation) {
                index
            } else {
                derivations.push(derivation);
                derivations.len() - 1
            }
        })
        .collect();
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

    let stdout = run_build_command(build_command, data.build_tree && data.supports_flakes).await?;
    let built_closures = if data.supports_flakes {
        parse_build_json(&stdout, &derivations)?
    } else {
        parse_build_out_paths(&stdout, derivations.len())?
    };
    let closures: Vec<String> = derivation_indexes
        .into_iter()
        .map(|index| built_closures[index].clone())
        .collect();

    for ((d, _), closure) in items.iter().zip(&closures) {
        check_and_sign_profile(d, closure).await?;
    }

    Ok(closures)
}

/// Resolve derivations, then build all profiles (dispatching remote vs local).
///
/// Remote profiles are built individually; local profiles are batched into a
/// single `nix build` invocation for efficiency.
pub async fn build_profiles(
    datas: &[PushProfileData<'_>],
) -> Result<Vec<String>, PushProfileError> {
    // Resolve derivations for every profile concurrently
    let derivations: Vec<String> =
        futures_util::future::try_join_all(datas.iter().map(resolve_derivation)).await?;

    // Separate remote vs local, building remote ones immediately
    let mut closures = vec![None; datas.len()];
    let mut local_builds: Vec<(&PushProfileData<'_>, &str)> = Vec::new();
    let mut local_indexes = Vec::new();
    for (index, (data, deriver)) in datas.iter().zip(derivations.iter()).enumerate() {
        if data
            .deploy_data
            .merged_settings
            .remote_build
            .unwrap_or(false)
        {
            if !data.supports_flakes {
                warn!("remote builds using non-flake nix are experimental");
            }
            let closure = build_profile_remotely(data, deriver).await?;
            check_and_sign_remote_profile(data, &closure).await?;
            closures[index] = Some(closure);
        } else {
            local_builds.push((data, deriver.as_str()));
            local_indexes.push(index);
        }
    }

    // Build all local profiles in a single nix build invocation
    if !local_builds.is_empty() {
        let local_closures = build_profiles_locally(&local_builds).await?;
        for (index, closure) in local_indexes.into_iter().zip(local_closures) {
            closures[index] = Some(closure);
        }
    }

    Ok(closures
        .into_iter()
        .map(|closure| closure.expect("every profile should have a build closure recorded"))
        .collect())
}

pub async fn build_profile(data: PushProfileData<'_>) -> Result<String, PushProfileError> {
    build_profiles(&[data])
        .await
        .map(|mut closures| closures.remove(0))
}

#[derive(Debug, PartialEq, Eq)]
struct CopyGroupKey {
    hostname: String,
    ssh_user: String,
    ssh_opts: String,
    fast_connection: Option<bool>,
    check_sigs: bool,
}

struct CopyGroup {
    key: CopyGroupKey,
    indexes: Vec<usize>,
}

fn copy_group_key(data: &PushProfileData<'_>) -> Result<CopyGroupKey, PushProfileError> {
    let hostname = match data.deploy_data.cmd_overrides.hostname {
        Some(ref x) => x,
        None => &data.deploy_data.node.node_settings.hostname,
    };

    Ok(CopyGroupKey {
        hostname: hostname.clone(),
        ssh_user: data.deploy_defs.ssh_user.clone(),
        ssh_opts: encode_ssh_opts(&data.deploy_data.merged_settings.ssh_opts)?,
        fast_connection: data.deploy_data.merged_settings.fast_connection,
        check_sigs: data.check_sigs,
    })
}

fn make_copy_command(key: &CopyGroupKey, paths: &[&str]) -> Command {
    let mut copy_command = Command::new("nix");
    copy_command.arg("copy");

    if key.fast_connection != Some(true) {
        copy_command.arg("--substitute-on-destination");
    }

    if !key.check_sigs {
        copy_command.arg("--no-check-sigs");
    }

    copy_command
        .arg("--to")
        .arg(format!("ssh://{}@{}", key.ssh_user, key.hostname))
        .args(paths)
        .env("NIX_SSHOPTS", &key.ssh_opts);

    copy_command
}

fn copy_group_nodes(datas: &[PushProfileData<'_>], group: &CopyGroup) -> String {
    let mut nodes = Vec::new();

    for &index in &group.indexes {
        let node_name = datas[index].deploy_data.node_name;
        if !nodes.contains(&node_name) {
            nodes.push(node_name);
        }
    }

    nodes.join(", ")
}

pub async fn push_profiles(
    datas: &[PushProfileData<'_>],
    closures: &[String],
) -> Result<(), PushProfileError> {
    debug_assert_eq!(datas.len(), closures.len());
    let mut copy_groups: Vec<CopyGroup> = Vec::new();

    for (index, data) in datas.iter().enumerate() {
        // Remote building guarantees that the resulting derivation is stored on the target system,
        // so there is no need to copy after building.
        if data
            .deploy_data
            .merged_settings
            .remote_build
            .unwrap_or(false)
        {
            continue;
        }

        let key = copy_group_key(data)?;
        if let Some(group) = copy_groups.iter_mut().find(|group| group.key == key) {
            group.indexes.push(index);
        } else {
            copy_groups.push(CopyGroup {
                key,
                indexes: vec![index],
            });
        }
    }

    for group in copy_groups {
        let profiles_str = group
            .indexes
            .iter()
            .map(|&index| {
                let data = &datas[index];
                format!(
                    "`{}.{}`",
                    data.deploy_data.node_name, data.deploy_data.profile_name
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let nodes_str = copy_group_nodes(datas, &group);
        let target = format!("ssh://{}@{}", group.key.ssh_user, group.key.hostname);
        info!(
            "Copying {} {} to node `{}`: {}",
            group.indexes.len(),
            if group.indexes.len() > 1 {
                "profiles"
            } else {
                "profile"
            },
            group.key.hostname,
            profiles_str
        );

        let paths: Vec<&str> = group
            .indexes
            .iter()
            .map(|&index| closures[index].as_str())
            .collect();

        command::Command::new(make_copy_command(&group.key, &paths))
            .status()
            .await
            .map_err(|source| PushProfileError::CopyGroup {
                nodes: nodes_str.clone(),
                target: target.clone(),
                profiles: profiles_str.clone(),
                source: Box::new(source),
            })?;
    }

    Ok(())
}

pub async fn push_profile(
    data: PushProfileData<'_>,
    closure: &str,
) -> Result<(), PushProfileError> {
    push_profiles(&[data], &[closure.to_string()]).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[cfg(unix)]
    use std::path::Path;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn get_args(cmd: &Command) -> Vec<String> {
        let std_cmd = cmd.as_std();
        std::iter::once(std_cmd.get_program())
            .chain(std_cmd.get_args())
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn ssh_option_encoding_preserves_argument_boundaries() {
        let options = vec![
            "-o".to_string(),
            "ProxyCommand=ssh jump host -W %h:%p".to_string(),
            "-i".to_string(),
            "/keys/user's key".to_string(),
        ];

        let encoded = encode_ssh_opts(&options).unwrap();
        assert_eq!(shlex::split(&encoded).unwrap(), options);
    }

    #[test]
    fn test_make_build_command_flakes_single_derivation() {
        let cmd = make_build_command(true, false, None, &[], &["/nix/store/abc.drv^out"], &[]);
        assert_eq!(
            get_args(&cmd),
            vec![
                "nix",
                "build",
                "--json",
                "/nix/store/abc.drv^out",
                "--no-link"
            ]
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
                "--json",
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
    fn test_nested_remote_commands_copy_outer_drv_and_build_full_installable() {
        let derivation = "/nix/store/outer.drv^out^out";
        let copy = make_remote_derivation_copy_command(
            "ssh-ng://deploy@example.com",
            "-p 2222",
            derivation,
        );
        let build =
            make_remote_build_command("ssh-ng://deploy@example.com", "-p 2222", derivation, &[]);

        assert_eq!(
            get_args(&copy),
            vec![
                "nix",
                "--experimental-features",
                "nix-command",
                "copy",
                "-s",
                "--to",
                "ssh-ng://deploy@example.com",
                "--derivation",
                "/nix/store/outer.drv",
            ]
        );
        assert_eq!(
            get_args(&build),
            vec![
                "nix",
                "--experimental-features",
                "nix-command",
                "build",
                "/nix/store/outer.drv^out^out",
                "--eval-store",
                "auto",
                "--store",
                "ssh-ng://deploy@example.com",
                "--json",
            ]
        );
    }

    #[test]
    fn test_remote_profile_check_and_sign_commands_use_remote_store() {
        let store = "ssh-ng://deploy@example.com";
        let ssh_opts = "-p 2222";
        let closure = "/nix/store/abc-profile";
        let check =
            make_remote_store_ls_command(store, ssh_opts, "/nix/store/abc-profile/activate-rs");
        let sign = make_remote_sign_command(store, ssh_opts, "/keys/cache.sec", closure);

        assert_eq!(
            get_args(&check),
            vec![
                "nix",
                "--experimental-features",
                "nix-command",
                "store",
                "ls",
                "--json",
                "--store",
                "ssh-ng://deploy@example.com",
                "/nix/store/abc-profile/activate-rs",
            ]
        );
        assert_eq!(
            get_args(&sign),
            vec![
                "nix",
                "--experimental-features",
                "nix-command",
                "store",
                "sign",
                "--store",
                "ssh-ng://deploy@example.com",
                "--recursive",
                "--key-file",
                "/keys/cache.sec",
                "/nix/store/abc-profile",
            ]
        );
    }

    #[test]
    fn test_make_build_command_keep_result() {
        let profiles = vec![BuildCommandInfo {
            node_name: "node1",
            profile_name: "system",
        }];
        let cmd = make_build_command(
            true,
            true,
            Some("./results"),
            &[],
            &["/nix/store/abc.drv^out"],
            &profiles,
        );
        assert_eq!(
            get_args(&cmd),
            vec![
                "nix",
                "build",
                "--json",
                "/nix/store/abc.drv^out",
                "--out-link",
                "./results/node1/system",
            ]
        );
    }

    #[test]
    fn test_make_build_command_keep_result_multiple_profiles_uses_shared_out_link() {
        let profiles = vec![
            BuildCommandInfo {
                node_name: "node1",
                profile_name: "logs",
            },
            BuildCommandInfo {
                node_name: "node1",
                profile_name: "metrics",
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
                "--json",
                "/nix/store/abc.drv^out",
                "/nix/store/def.drv^out",
                "--out-link",
                "./results/profiles",
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
                "--json",
                "/nix/store/abc.drv^out",
                "--out-link",
                "./.deploy-gc/mynode/web",
            ]
        );
    }

    #[test]
    fn test_make_build_command_extra_args() {
        let extra = vec!["--option".to_string(), "foo".to_string(), "bar".to_string()];
        let cmd = make_build_command(true, false, None, &extra, &["/nix/store/abc.drv^out"], &[]);
        assert_eq!(
            get_args(&cmd),
            vec![
                "nix",
                "build",
                "--json",
                "/nix/store/abc.drv^out",
                "--no-link",
                "--option",
                "foo",
                "bar"
            ]
        );
    }

    #[test]
    fn parse_build_out_paths_returns_batch_in_order() {
        let stdout = b"/nix/store/abc123-example\n/nix/store/def456-example\n";
        let paths = parse_build_out_paths(stdout, 2).expect("two-line stdout must parse");
        assert_eq!(
            paths,
            vec![
                "/nix/store/abc123-example".to_string(),
                "/nix/store/def456-example".to_string()
            ]
        );
    }

    #[test]
    fn parse_build_out_paths_rejects_empty_output() {
        let err = parse_build_out_paths(b"", 1).expect_err("empty stdout must error");
        assert!(matches!(err, PushProfileError::BuildStdoutEmpty));
    }

    #[test]
    fn parse_build_out_paths_rejects_wrong_number_of_outputs() {
        let stdout = b"/nix/store/a\n/nix/store/b\n";
        let err = parse_build_out_paths(stdout, 1).expect_err("wrong path count must error");
        assert!(matches!(
            err,
            PushProfileError::BuildStdoutPathCount {
                actual: 2,
                expected: 1,
                ..
            }
        ));
    }

    #[test]
    fn parse_build_json_matches_reordered_results_by_drv_path() {
        let stdout = br#"[
            {"drvPath":"/nix/store/b.drv","outputs":{"out":"/nix/store/b"}},
            {"drvPath":"/nix/store/a.drv","outputs":{"out":"/nix/store/a"}}
        ]"#;
        let paths = parse_build_json(stdout, &["/nix/store/a.drv^out", "/nix/store/b.drv^out"])
            .expect("JSON results must be associated by drvPath");

        assert_eq!(paths, vec!["/nix/store/a", "/nix/store/b"]);
    }

    #[test]
    fn parse_build_json_matches_nested_dynamic_drv_path() {
        let stdout = br#"[{
            "drvPath": {
                "drvPath": "/nix/store/outer.drv",
                "output": "out",
                "outputPath": "/nix/store/generated.drv"
            },
            "outputs": {"out":"/nix/store/final"}
        }]"#;
        let paths = parse_build_json(stdout, &["/nix/store/outer.drv^out^out"])
            .expect("recursive drvPath identity must match nested installable");

        assert_eq!(paths, vec!["/nix/store/final"]);
    }

    #[test]
    fn parse_build_json_selects_requested_output() {
        let stdout = br#"[{
            "drvPath":"/nix/store/a.drv",
            "outputs":{
                "out":"/nix/store/a",
                "dev":"/nix/store/a-dev"
            }
        }]"#;
        let paths = parse_build_json(stdout, &["/nix/store/a.drv^dev"])
            .expect("the requested non-default output must be selected");

        assert_eq!(paths, vec!["/nix/store/a-dev"]);
    }

    #[test]
    fn parse_build_json_selects_nested_dynamic_non_default_output() {
        let stdout = br#"[{
            "drvPath": {
                "drvPath": "/nix/store/outer.drv",
                "output": "out",
                "outputPath": "/nix/store/generated.drv"
            },
            "outputs": {
                "out":"/nix/store/final",
                "dev":"/nix/store/final-dev"
            }
        }]"#;
        let paths = parse_build_json(stdout, &["/nix/store/outer.drv^out^dev"])
            .expect("the final output of a nested derivation must be selected");

        assert_eq!(paths, vec!["/nix/store/final-dev"]);
    }

    #[tokio::test]
    async fn legacy_build_rejects_non_default_output() {
        let err = deriver_for_build("/nix/store/a.drv".to_string(), "dev", false)
            .await
            .expect_err("nix-build cannot reliably select a non-default output");

        assert!(matches!(
            err,
            PushProfileError::LegacyNonDefaultOutput(output) if output == "dev"
        ));
    }

    #[test]
    fn parse_build_json_rejects_missing_derivation() {
        let stdout = br#"[{"drvPath":"/nix/store/a.drv","outputs":{"out":"/nix/store/a"}}]"#;
        let err = parse_build_json(stdout, &["/nix/store/b.drv^out"])
            .expect_err("missing derivation identity must error");

        assert!(matches!(
            err,
            PushProfileError::BuildStdoutMissingDerivation(path)
                if path == "/nix/store/b.drv^out"
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_build_command_uses_nom_when_available() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let nix_args = dir.path().join("nix.args");
        let nom_args = dir.path().join("nom.args");

        write_executable(
            &bin.join("nix"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nprintf '{{\"action\":\"msg\"}}\\n' >&2\nexit 0\n",
                nix_args.display()
            ),
        );
        write_executable(
            &bin.join("nom"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then exit 0; fi\nprintf '%s\\n' \"$@\" > {}\n/bin/cat >/dev/null\nexit 0\n",
                nom_args.display()
            ),
        );

        let mut command = Command::new(bin.join("nix"));
        command.arg("build").env("PATH", &bin);
        let result = run_build_command(command, true).await;

        result.unwrap();
        assert!(std::fs::read_to_string(nix_args)
            .unwrap()
            .contains("--log-format\ninternal-json\n--verbose"));
        assert_eq!(std::fs::read_to_string(nom_args).unwrap(), "--json\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_run_build_command_falls_back_without_nom() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let nix_args = dir.path().join("nix.args");

        write_executable(
            &bin.join("nix"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nexit 0\n",
                nix_args.display()
            ),
        );

        let mut command = Command::new(bin.join("nix"));
        command.arg("build").env("PATH", &bin);
        let result = run_build_command(command, true).await;

        result.unwrap();
        assert_eq!(std::fs::read_to_string(nix_args).unwrap(), "build\n");
    }

    #[test]
    fn test_make_copy_command_multiple_paths() {
        let key = CopyGroupKey {
            hostname: "example.com".to_string(),
            ssh_user: "deploy".to_string(),
            ssh_opts: "-p 2222".to_string(),
            fast_connection: Some(false),
            check_sigs: false,
        };
        let cmd = make_copy_command(&key, &["/nix/store/abc-profile", "/nix/store/def-profile"]);

        assert_eq!(
            get_args(&cmd),
            vec![
                "nix",
                "copy",
                "--substitute-on-destination",
                "--no-check-sigs",
                "--to",
                "ssh://deploy@example.com",
                "/nix/store/abc-profile",
                "/nix/store/def-profile",
            ]
        );

        let nix_sshopts = cmd
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == "NIX_SSHOPTS")
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned());
        assert_eq!(nix_sshopts, Some("-p 2222".to_string()));
    }

    #[test]
    fn test_make_copy_command_fast_connection_and_check_sigs() {
        let key = CopyGroupKey {
            hostname: "example.com".to_string(),
            ssh_user: "deploy".to_string(),
            ssh_opts: String::new(),
            fast_connection: Some(true),
            check_sigs: true,
        };
        let cmd = make_copy_command(&key, &["/nix/store/abc-profile"]);

        assert_eq!(
            get_args(&cmd),
            vec![
                "nix",
                "copy",
                "--to",
                "ssh://deploy@example.com",
                "/nix/store/abc-profile",
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
                tags: vec![],
                drv_path: None,
                output_name: "out".to_string(),
            },
            generic_settings: empty_settings(),
        };
        let cmd_overrides = empty_cmd_overrides();
        let deploy_data = crate::make_deploy_data(
            &settings,
            &node,
            "testnode",
            &profile,
            "system",
            &cmd_overrides,
            false,
            None,
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
        let result = rt.block_on(check_and_sign_profile(&data, "/nonexistent/path"));
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
                tags: vec![],
                drv_path: None,
                output_name: "out".to_string(),
            },
            generic_settings: empty_settings(),
        };
        let cmd_overrides = empty_cmd_overrides();
        let deploy_data = crate::make_deploy_data(
            &settings,
            &node,
            "testnode",
            &profile,
            "system",
            &cmd_overrides,
            false,
            None,
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
        let result = rt.block_on(check_and_sign_profile(
            &data,
            dir.path().to_string_lossy().as_ref(),
        ));
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
                tags: vec![],
                drv_path: None,
                output_name: "out".to_string(),
            },
            generic_settings: empty_settings(),
        };
        let cmd_overrides = empty_cmd_overrides();
        let deploy_data = crate::make_deploy_data(
            &settings,
            &node,
            "testnode",
            &profile,
            "system",
            &cmd_overrides,
            false,
            None,
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
        let result = rt.block_on(check_and_sign_profile(
            &data,
            dir.path().to_string_lossy().as_ref(),
        ));
        assert!(result.is_ok());
    }
}
