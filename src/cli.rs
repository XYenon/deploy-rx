// SPDX-FileCopyrightText: 2020 Serokell <https://serokell.io/>
// SPDX-FileCopyrightText: 2021 Yannik Sander <contact@ysndr.de>
//
// SPDX-License-Identifier: MPL-2.0

use std::collections::{HashMap, HashSet};
use std::io::{stdin, stdout, Write};

use clap::{ArgMatches, FromArgMatches, Parser};

use crate as deploy;

use self::deploy::{DeployFlake, ParseFlakeError};
use futures_util::future::try_join_all;
use log::{debug, error, info, warn};
use serde::Serialize;
use std::path::PathBuf;
use std::process::Stdio;
use thiserror::Error;
use tokio::process::Command;

fn add_nix_command_and_flakes(cmd: &mut Command) {
    cmd.args([
        "--extra-experimental-features",
        "nix-command",
        "--extra-experimental-features",
        "flakes",
    ]);
}

/// Simple Rust rewrite of a simple Nix Flake deployment tool
#[derive(Parser, Debug, Clone)]
#[command(version = "1.0", author = "Serokell <https://serokell.io/>")]
pub struct Opts {
    /// The flake to deploy
    #[arg(group = "deploy")]
    target: Option<String>,

    /// A list of flakes to deploy alternatively
    #[arg(long, group = "deploy", num_args = 1..)]
    targets: Option<Vec<String>>,
    /// Only deploy profiles matching all of the given tags
    #[arg(short = 't', long = "tag")]
    tags: Vec<String>,
    /// Treat targets as files instead of flakes
    #[clap(short, long)]
    file: Option<String>,
    /// Check signatures when using `nix copy`
    #[arg(short, long)]
    checksigs: bool,
    /// Use the interactive prompt before deployment
    #[arg(short, long)]
    interactive: bool,
    /// Show Nix build trees using nix-output-monitor (`nom`) when available (enabled by default)
    #[arg(long, default_value_t = true)]
    build_tree: bool,
    /// Disable Nix build tree visualization
    #[arg(long)]
    no_build_tree: bool,
    /// Review derivation changes on the target host before activating profiles (enabled by default)
    #[arg(long, default_value_t = true)]
    review_changes: bool,
    /// Disable derivation change review before activation
    #[arg(long)]
    no_review_changes: bool,
    /// Extra arguments to be passed to nix build
    #[arg(last = true)]
    extra_build_args: Vec<String>,

    /// Print debug logs to output
    #[arg(short, long)]
    debug_logs: bool,
    /// Directory to print logs to (including the background activation process)
    #[arg(long)]
    log_dir: Option<String>,

    /// Keep the build outputs of each built profile
    #[arg(short, long)]
    keep_result: bool,
    /// Location to keep outputs from built profiles in
    #[arg(short, long)]
    result_path: Option<String>,

    /// Skip the automatic pre-build checks
    #[arg(short, long)]
    skip_checks: bool,

    /// Build on remote host
    #[arg(long)]
    remote_build: bool,

    /// Override the SSH user with the given value
    #[arg(long)]
    ssh_user: Option<String>,
    /// Override the profile user with the given value
    #[arg(long)]
    profile_user: Option<String>,
    /// Override the SSH options used
    #[arg(long, allow_hyphen_values = true)]
    ssh_opts: Option<String>,
    /// Override if the connecting to the target node should be considered fast
    #[arg(long)]
    fast_connection: Option<bool>,
    /// Override if a rollback should be attempted if activation fails
    #[arg(long)]
    auto_rollback: Option<bool>,
    /// Override hostname used for the node
    #[arg(long)]
    hostname: Option<String>,
    /// Make activation wait for confirmation, or roll back after a period of time
    #[arg(long)]
    magic_rollback: Option<bool>,
    /// How long activation should wait for confirmation (if using magic-rollback)
    #[arg(long)]
    confirm_timeout: Option<u16>,
    /// How long we should wait for profile activation
    #[arg(long)]
    activation_timeout: Option<u16>,
    /// Where to store temporary files (only used by magic-rollback)
    #[arg(long)]
    temp_path: Option<PathBuf>,
    /// Show what will be activated on the machines
    #[arg(long)]
    dry_activate: bool,
    /// Don't activate, but update the boot loader to boot into the new profile
    #[arg(long)]
    boot: bool,
    /// Revoke all previously succeeded deploys when deploying multiple profiles
    #[arg(long)]
    rollback_succeeded: Option<bool>,
    /// Which sudo command to use. Must accept at least two arguments: user name to execute commands as and the rest is the command to execute
    #[arg(long)]
    sudo: Option<String>,
    /// Prompt for sudo password during activation.
    #[arg(long)]
    interactive_sudo: Option<bool>,
    /// Disable SSH connection multiplexing (reusing connections for multiple profiles)
    #[arg(long)]
    no_ssh_multiplexing: bool,
    /// Disable fresh SSH connection for rollback check.
    /// When disabled, rollback check may reuse existing SSH connections, which can cause
    /// false-positive success even if SSH is broken (see https://github.com/serokell/deploy-rs/issues/106)
    #[arg(long)]
    no_rollback_fresh_connection: bool,
}

/// Returns if the available Nix installation supports flakes
async fn test_flake_support() -> Result<bool, std::io::Error> {
    debug!("Checking for flake support");

    let mut cmd = Command::new("nix");
    add_nix_command_and_flakes(&mut cmd);
    Ok(cmd
        .arg("eval")
        .arg("--expr")
        .arg("builtins.getFlake")
        // This will error on some machines "intentionally", and we don't really need that printing
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?
        .success())
}

#[derive(Error, Debug)]
pub enum CheckDeploymentError {
    #[error("Failed to execute Nix checking command: {0}")]
    NixCheck(#[from] std::io::Error),
    #[error("Nix checking command resulted in a bad exit code: {0:?}")]
    NixCheckExit(Option<i32>),
}

async fn check_deployment(
    supports_flakes: bool,
    repo: &str,
    extra_build_args: &[String],
) -> Result<(), CheckDeploymentError> {
    info!("Running checks for flake in {}", repo);

    let mut check_command = match supports_flakes {
        true => Command::new("nix"),
        false => Command::new("nix-build"),
    };

    if supports_flakes {
        add_nix_command_and_flakes(&mut check_command);
        check_command.arg("flake").arg("check").arg(repo);
    } else {
        check_command.arg("-E")
                .arg("--no-out-link")
                .arg(format!("let r = import {}/.; x = (if builtins.isFunction r then (r {{}}) else r); in if x ? checks then x.checks.${{builtins.currentSystem}} else {{}}", repo));
    }

    check_command.args(extra_build_args);

    let check_status = check_command.status().await?;

    match check_status.code() {
        Some(0) => (),
        a => return Err(CheckDeploymentError::NixCheckExit(a)),
    };

    Ok(())
}

#[derive(Error, Debug)]
pub enum GetDeploymentDataError {
    #[error("Failed to execute nix eval command: {0}")]
    NixEval(std::io::Error),
    #[error("Failed to read output from evaluation: {0}")]
    NixEvalOut(std::io::Error),
    #[error("Evaluation resulted in a bad exit code: {0:?}")]
    NixEvalExit(Option<i32>),
    #[error("Error converting evaluation output to utf8: {0}")]
    DecodeUtf8(#[from] std::string::FromUtf8Error),
    #[error("Error decoding the JSON from evaluation: {0}")]
    DecodeJson(#[from] serde_json::error::Error),
    #[error("Impossible happened: profile is set but node is not")]
    ProfileNoNode,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
struct NodeReq<'a> {
    all_profiles: bool,
    profiles: std::collections::HashSet<&'a str>,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
struct RepoReq<'a> {
    all_nodes: bool,
    nodes: std::collections::HashMap<&'a str, NodeReq<'a>>,
}

fn build_repo_reqs<'a>(
    flakes: &'a [deploy::DeployFlake<'_>],
) -> Result<HashMap<&'a str, RepoReq<'a>>, GetDeploymentDataError> {
    let mut repo_reqs: HashMap<&str, RepoReq<'_>> = HashMap::new();
    for f in flakes {
        let req = repo_reqs.entry(f.repo).or_insert_with(|| RepoReq {
            all_nodes: false,
            nodes: HashMap::new(),
        });
        match (&f.node, &f.profile) {
            (Some(node), Some(profile)) => {
                let n_req = req.nodes.entry(node.as_str()).or_insert_with(|| NodeReq {
                    all_profiles: false,
                    profiles: std::collections::HashSet::new(),
                });
                n_req.profiles.insert(profile.as_str());
            }
            (Some(node), None) => {
                let n_req = req.nodes.entry(node.as_str()).or_insert_with(|| NodeReq {
                    all_profiles: false,
                    profiles: std::collections::HashSet::new(),
                });
                n_req.all_profiles = true;
            }
            (None, None) => {
                req.all_nodes = true;
            }
            (None, Some(_)) => return Err(GetDeploymentDataError::ProfileNoNode),
        }
    }
    Ok(repo_reqs)
}

/// Evaluates the Nix in the given `repo` and return the processed Data from it
async fn get_deployment_data(
    supports_flakes: bool,
    flakes: &[deploy::DeployFlake<'_>],
    extra_build_args: &[String],
) -> Result<Vec<deploy::data::Data>, GetDeploymentDataError> {
    if flakes.is_empty() {
        return Ok(Vec::new());
    }

    let flakes_str = flakes
        .iter()
        .map(|f| {
            let mut name = f.repo.to_string();
            if let Some(node) = &f.node {
                name.push_str(&format!("#{}", node));
                if let Some(profile) = &f.profile {
                    name.push_str(&format!(".{}", profile));
                }
            }
            format!("`{}`", name)
        })
        .collect::<Vec<_>>()
        .join(", ");
    info!(
        "Evaluating {} {}: {}",
        flakes.len(),
        if flakes.len() > 1 { "flakes" } else { "flake" },
        flakes_str
    );

    let repo_reqs = build_repo_reqs(flakes)?;

    let mut repo_data_futures = Vec::new();
    for (repo, req) in repo_reqs {
        let extra_build_args = extra_build_args.to_vec();
        repo_data_futures.push(async move {
            let mut c = if supports_flakes {
                let req_json = serde_json::to_string(&req).expect("failed to serialize request");
                let filter_expr = r#"
req: deploy:
let
  filterNode = name: node:
    if builtins.hasAttr name req.nodes then
      let
        nReq = req.nodes.${name};
      in
        if nReq.all_profiles then
          node
        else
          node // {
            profiles = builtins.intersectAttrs
              (builtins.listToAttrs (map (p: { name = p; value = true; }) nReq.profiles))
              node.profiles;
          }
    else
      {};
in
  if req.all_nodes then
    deploy
  else
    deploy // {
      nodes = builtins.intersectAttrs
        (builtins.listToAttrs (map (n: { name = n; value = true; }) (builtins.attrNames req.nodes)))
        (builtins.mapAttrs filterNode deploy.nodes);
    }
"#;

                let mut c = Command::new("nix");
                add_nix_command_and_flakes(&mut c);
                c.arg("eval")
                    .arg("--json")
                    .arg(format!("{}#deploy", repo))
                    .arg("--apply")
                    .arg(format!("({}) (builtins.fromJSON ''{}'')", filter_expr, req_json));
                c
            } else {
                let mut c = Command::new("nix-instantiate");
                c.arg("--strict")
                    .arg("--read-write-mode")
                    .arg("--json")
                    .arg("--eval")
                    .arg("-E")
                    .arg(format!("let r = import {}/.; in if builtins.isFunction r then (r {{}}).deploy else r.deploy", repo));
                c
            };
            c.args(extra_build_args);

            let build_child = c
                .stdout(Stdio::piped())
                .spawn()
                .map_err(GetDeploymentDataError::NixEval)?;

            let build_output = build_child
                .wait_with_output()
                .await
                .map_err(GetDeploymentDataError::NixEvalOut)?;

            match build_output.status.code() {
                Some(0) => (),
                a => return Err(GetDeploymentDataError::NixEvalExit(a)),
            };

            let data_json = String::from_utf8(build_output.stdout)?;
            let parsed_data: deploy::data::Data = serde_json::from_str(&data_json)?;
            Ok::<(&str, deploy::data::Data), GetDeploymentDataError>((repo, parsed_data))
        });
    }

    let repo_data: HashMap<&str, deploy::data::Data> =
        try_join_all(repo_data_futures).await?.into_iter().collect();

    let output = flakes
        .iter()
        .map(|f| repo_data.get(f.repo).unwrap().clone())
        .collect();

    Ok(output)
}

#[derive(Serialize)]
struct PromptPart<'a> {
    user: &'a str,
    ssh_user: &'a str,
    path: &'a str,
    hostname: &'a str,
    tags: &'a [String],
    ssh_opts: &'a [String],
}

fn print_deployment(
    parts: &[(
        &deploy::DeployFlake<'_>,
        deploy::DeployData,
        deploy::DeployDefs,
    )],
) -> Result<(), toml::ser::Error> {
    let mut part_map: HashMap<String, HashMap<String, PromptPart>> = HashMap::new();

    for (_, data, defs) in parts {
        part_map
            .entry(data.node_name.to_string())
            .or_default()
            .insert(
                data.profile_name.to_string(),
                PromptPart {
                    user: &defs.profile_user,
                    ssh_user: &defs.ssh_user,
                    path: &data.profile.profile_settings.path,
                    hostname: &data.node.node_settings.hostname,
                    tags: &data.profile.profile_settings.tags,
                    ssh_opts: &data.merged_settings.ssh_opts,
                },
            );
    }

    let toml = toml::to_string(&part_map)?;

    info!("The following profiles are going to be deployed:\n{}", toml);

    Ok(())
}
#[derive(Error, Debug)]
pub enum PromptDeploymentError {
    #[error("Failed to make printable TOML of deployment: {0}")]
    TomlFormat(#[from] toml::ser::Error),
    #[error("Failed to flush stdout prior to query: {0}")]
    StdoutFlush(std::io::Error),
    #[error("Failed to read line from stdin: {0}")]
    StdinRead(std::io::Error),
    #[error("User cancelled deployment")]
    Cancelled,
}

fn prompt_deployment(
    parts: &[(
        &deploy::DeployFlake<'_>,
        deploy::DeployData,
        deploy::DeployDefs,
    )],
) -> Result<(), PromptDeploymentError> {
    print_deployment(parts)?;

    info!("Are you sure you want to deploy these profiles?");
    print!("> ");

    stdout()
        .flush()
        .map_err(PromptDeploymentError::StdoutFlush)?;

    let mut s = String::new();
    stdin()
        .read_line(&mut s)
        .map_err(PromptDeploymentError::StdinRead)?;

    if !yn::yes(&s) {
        if yn::is_somewhat_yes(&s) {
            info!("Sounds like you might want to continue, to be more clear please just say \"yes\". Do you want to deploy these profiles?");
            print!("> ");

            stdout()
                .flush()
                .map_err(PromptDeploymentError::StdoutFlush)?;

            let mut s = String::new();
            stdin()
                .read_line(&mut s)
                .map_err(PromptDeploymentError::StdinRead)?;

            if !yn::yes(&s) {
                return Err(PromptDeploymentError::Cancelled);
            }
        } else {
            if !yn::no(&s) {
                info!(
                    "That was unclear, but sounded like a no to me. Please say \"yes\" or \"no\" to be more clear."
                );
            }

            return Err(PromptDeploymentError::Cancelled);
        }
    }

    Ok(())
}

#[derive(Error, Debug)]
pub enum RunDeployError {
    #[error("Failed to deploy profile to node {0}: {1}")]
    DeployProfile(String, deploy::deploy::DeployProfileError),
    #[error("Failed to build profile on node {0}: {1}")]
    BuildProfile(String, deploy::push::PushProfileError),
    #[error("Failed to push profile to node {0}: {1}")]
    PushProfile(String, deploy::push::PushProfileError),
    #[error("No profile named `{0}` was found")]
    ProfileNotFound(String),
    #[error("No node named `{0}` was found")]
    NodeNotFound(String),
    #[error("Profile was provided without a node name")]
    ProfileWithoutNode,
    #[error("Error processing deployment definitions: {0}")]
    DeployDataDefs(#[from] deploy::DeployDataDefsError),
    #[error("Failed to make printable TOML of deployment: {0}")]
    TomlFormat(#[from] toml::ser::Error),
    #[error("{0}")]
    PromptDeployment(#[from] PromptDeploymentError),
    #[error("Failed to revoke profile for node {0}: {1}")]
    RevokeProfile(String, deploy::deploy::RevokeProfileError),
    #[error("Deployment to node {0} failed, rolled back to previous generation")]
    Rollback(String),
    #[error("Failed to establish SSH control master: {0}")]
    SshControlMaster(#[from] deploy::ssh::SshError),
    #[error("No profiles matched the requested tags: {0}")]
    NoProfilesMatchedTags(String),
    #[error("Failed to prompt for sudo password for {0}: {1}")]
    PromptSudoPassword(String, std::io::Error),
}

type ToDeploy<'a> = Vec<(
    &'a deploy::DeployFlake<'a>,
    &'a deploy::data::Data,
    (&'a str, &'a deploy::data::Node),
    (&'a str, &'a deploy::data::Profile),
)>;

type DeployPart<'a> = (
    &'a deploy::DeployFlake<'a>,
    deploy::DeployData<'a>,
    deploy::DeployDefs,
);

struct PushProfileDataOptions<'a> {
    supports_flakes: bool,
    check_sigs: bool,
    keep_result: bool,
    result_path: Option<&'a str>,
    extra_build_args: &'a [String],
    build_tree: bool,
}

fn make_push_profile_datas<'a>(
    parts: &'a [DeployPart<'a>],
    options: &PushProfileDataOptions<'a>,
) -> Vec<deploy::push::PushProfileData<'a>> {
    parts
        .iter()
        .map(
            |(deploy_flake, deploy_data, deploy_defs)| deploy::push::PushProfileData {
                supports_flakes: options.supports_flakes,
                check_sigs: options.check_sigs,
                repo: deploy_flake.repo,
                deploy_data,
                deploy_defs,
                keep_result: options.keep_result,
                result_path: options.result_path,
                extra_build_args: options.extra_build_args,
                build_tree: options.build_tree,
            },
        )
        .collect()
}

fn profile_matches_tags(profile: &deploy::data::Profile, tags: &HashSet<&str>) -> bool {
    tags.iter()
        .all(|tag| profile.profile_settings.tags.iter().any(|t| t == *tag))
}

fn ordered_profiles_for_node<'a>(
    node: &'a deploy::data::Node,
    tags: &HashSet<&str>,
) -> Result<Vec<(&'a str, &'a deploy::data::Profile)>, RunDeployError> {
    let mut profiles_list = Vec::new();
    let mut seen = HashSet::new();

    for profile_name in node
        .node_settings
        .profiles_order
        .iter()
        .map(|s| s.as_str())
        .chain(node.node_settings.profiles.keys().map(|s| s.as_str()))
    {
        if seen.insert(profile_name) {
            let profile = node
                .node_settings
                .profiles
                .get(profile_name)
                .ok_or_else(|| RunDeployError::ProfileNotFound(profile_name.to_string()))?;

            if profile_matches_tags(profile, tags) {
                profiles_list.push((profile_name, profile));
            }
        }
    }

    Ok(profiles_list)
}

fn collect_to_deploy<'a>(
    deploy_flakes: &'a [deploy::DeployFlake<'a>],
    data: &'a [deploy::data::Data],
    tags: &[String],
) -> Result<ToDeploy<'a>, RunDeployError> {
    let requested_tags: HashSet<&str> = tags.iter().map(|tag| tag.as_str()).collect();

    let to_deploy = deploy_flakes
        .iter()
        .zip(data)
        .map(|(deploy_flake, data)| {
            let to_deploys: ToDeploy = match (&deploy_flake.node, &deploy_flake.profile) {
                (Some(node_name), Some(profile_name)) => {
                    let node = match data.nodes.get(node_name) {
                        Some(x) => x,
                        None => return Err(RunDeployError::NodeNotFound(node_name.clone())),
                    };
                    let profile = match node.node_settings.profiles.get(profile_name) {
                        Some(x) => x,
                        None => return Err(RunDeployError::ProfileNotFound(profile_name.clone())),
                    };

                    if profile_matches_tags(profile, &requested_tags) {
                        vec![(
                            deploy_flake,
                            data,
                            (node_name.as_str(), node),
                            (profile_name.as_str(), profile),
                        )]
                    } else {
                        Vec::new()
                    }
                }
                (Some(node_name), None) => {
                    let node = match data.nodes.get(node_name) {
                        Some(x) => x,
                        None => return Err(RunDeployError::NodeNotFound(node_name.clone())),
                    };

                    ordered_profiles_for_node(node, &requested_tags)?
                        .into_iter()
                        .map(|x| (deploy_flake, data, (node_name.as_str(), node), x))
                        .collect()
                }
                (None, None) => {
                    let mut l = Vec::new();

                    for (node_name, node) in &data.nodes {
                        let ll: ToDeploy = ordered_profiles_for_node(node, &requested_tags)?
                            .into_iter()
                            .map(|x| (deploy_flake, data, (node_name.as_str(), node), x))
                            .collect();

                        l.extend(ll);
                    }

                    l
                }
                (None, Some(_)) => return Err(RunDeployError::ProfileWithoutNode),
            };
            Ok(to_deploys)
        })
        .collect::<Result<Vec<ToDeploy>, RunDeployError>>()?
        .into_iter()
        .flatten()
        .collect();

    Ok(to_deploy)
}

#[allow(clippy::too_many_arguments)]
async fn run_deploy(
    deploy_flakes: Vec<deploy::DeployFlake<'_>>,
    data: Vec<deploy::data::Data>,
    supports_flakes: bool,
    check_sigs: bool,
    interactive: bool,
    cmd_overrides: &deploy::CmdOverrides,
    keep_result: bool,
    result_path: Option<&str>,
    extra_build_args: &[String],
    debug_logs: bool,
    dry_activate: bool,
    boot: bool,
    log_dir: &Option<String>,
    rollback_succeeded: bool,
    ssh_multiplexing: bool,
    rollback_fresh_connection: bool,
    build_tree: bool,
    review_changes: bool,
    tags: &[String],
) -> Result<(), RunDeployError> {
    let to_deploy = collect_to_deploy(&deploy_flakes, &data, tags)?;

    if to_deploy.is_empty() && !tags.is_empty() {
        return Err(RunDeployError::NoProfilesMatchedTags(tags.join(", ")));
    }

    let mut parts: Vec<DeployPart<'_>> = Vec::new();

    for (deploy_flake, data, (node_name, node), (profile_name, profile)) in to_deploy {
        let deploy_data = deploy::make_deploy_data(
            &data.generic_settings,
            node,
            node_name,
            profile,
            profile_name,
            cmd_overrides,
            debug_logs,
            log_dir.as_deref(),
        );

        let mut deploy_defs = deploy_data.defs()?;

        if deploy_data
            .merged_settings
            .interactive_sudo
            .unwrap_or(false)
            && deploy_defs.sudo.is_some()
        {
            warn!("Interactive sudo is enabled! Using a sudo password is less secure than correctly configured SSH keys.\nPlease use keys in production environments.");

            if deploy_defs
                .sudo
                .as_ref()
                .map(|sudo| !sudo.is_sudo())
                .unwrap_or(false)
            {
                warn!("Custom sudo commands should be configured to accept password input from stdin when using the 'interactive sudo' option. Deployment may fail if the custom command ignores stdin.");
            }

            info!(
                "You will now be prompted for the sudo password for {}.",
                node.node_settings.hostname
            );
            let sudo_password = rpassword::prompt_password(format!(
                "(sudo for {}) Password: ",
                node.node_settings.hostname
            ))
            .map_err(|err| {
                RunDeployError::PromptSudoPassword(node.node_settings.hostname.clone(), err)
            })?;

            deploy_defs.sudo_password = Some(sudo_password);
        }

        parts.push((deploy_flake, deploy_data, deploy_defs));
    }

    if interactive {
        prompt_deployment(&parts[..])?;
    } else {
        print_deployment(&parts[..])?;
    }

    let push_profile_data_options = PushProfileDataOptions {
        supports_flakes,
        check_sigs,
        keep_result,
        result_path,
        extra_build_args,
        build_tree,
    };

    let push_profile_datas = make_push_profile_datas(&parts, &push_profile_data_options);

    // Resolve derivations, then build all profiles (remote individually, local batched)
    deploy::push::build_profiles(&push_profile_datas)
        .await
        .map_err(|e| {
            let node_names: Vec<_> = push_profile_datas
                .iter()
                .map(|d| d.deploy_data.node_name.to_string())
                .collect();
            RunDeployError::BuildProfile(node_names.join(", "), e)
        })?;

    let ssh_multiplexer = if ssh_multiplexing {
        let multiplexer = deploy::ssh::SshMultiplexer::new();

        for (_, deploy_data, deploy_defs) in &mut parts {
            let hostname = cmd_overrides
                .hostname
                .as_deref()
                .unwrap_or(&deploy_data.node.node_settings.hostname);

            let control_master = multiplexer
                .get_or_create(
                    hostname,
                    Some(&deploy_defs.ssh_user),
                    &deploy_data.merged_settings.ssh_opts,
                )
                .await?;

            deploy_data
                .merged_settings
                .ssh_opts
                .extend(control_master.control_opts());
        }

        Some(multiplexer)
    } else {
        None
    };

    let push_profile_datas = make_push_profile_datas(&parts, &push_profile_data_options);

    deploy::push::push_profiles(&push_profile_datas)
        .await
        .map_err(|e| {
            let node_names = e.node_context().map(str::to_string).unwrap_or_else(|| {
                push_profile_datas
                    .iter()
                    .map(|d| d.deploy_data.node_name.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            });
            RunDeployError::PushProfile(node_names, e)
        })?;

    let mut succeeded: Vec<(&deploy::DeployData, &deploy::DeployDefs)> = vec![];

    // Run all deployments
    // In case of an error rollback any previoulsy made deployment.
    // Rollbacks adhere to the global seeting to auto_rollback and secondary
    // the profile's configuration
    for (_, deploy_data, deploy_defs) in &parts {
        if let Err(e) = deploy::deploy::deploy_profile(
            deploy_data,
            deploy_defs,
            dry_activate,
            boot,
            rollback_fresh_connection,
            review_changes,
        )
        .await
        {
            error!("{}", e);
            if dry_activate {
                info!("dry run, not rolling back");
            }
            if rollback_succeeded && cmd_overrides.auto_rollback.unwrap_or(true) {
                info!("Revoking previous deploys");
                // revoking all previous deploys
                // (adheres to profile configuration if not set explicitely by
                //  the command line)
                for (deploy_data, deploy_defs) in &succeeded {
                    if deploy_data.merged_settings.auto_rollback.unwrap_or(true) {
                        deploy::deploy::revoke(deploy_data, deploy_defs)
                            .await
                            .map_err(|e| {
                                RunDeployError::RevokeProfile(deploy_data.node_name.to_string(), e)
                            })?;
                    }
                }
                return Err(RunDeployError::Rollback(deploy_data.node_name.to_string()));
            }
            return Err(RunDeployError::DeployProfile(
                deploy_data.node_name.to_string(),
                e,
            ));
        }
        succeeded.push((deploy_data, deploy_defs))
    }

    if let Some(multiplexer) = ssh_multiplexer {
        multiplexer.close_all().await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DeployFlake;
    use std::collections::{HashMap, HashSet};

    fn empty_generic_settings() -> crate::data::GenericSettings {
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

    fn profile_with_tags(tags: &[&str]) -> crate::data::Profile {
        crate::data::Profile {
            profile_settings: crate::data::ProfileSettings {
                path: "/nix/store/test-profile".to_string(),
                profile_path: None,
                tags: tags.iter().map(|tag| tag.to_string()).collect(),
            },
            generic_settings: empty_generic_settings(),
        }
    }

    fn second_node_with_profiles() -> crate::data::Node {
        let mut profiles = HashMap::new();
        profiles.insert("api".to_string(), profile_with_tags(&["edge", "prod"]));
        profiles.insert("batch".to_string(), profile_with_tags(&["batch"]));

        crate::data::Node {
            generic_settings: empty_generic_settings(),
            node_settings: crate::data::NodeSettings {
                hostname: "example.net".to_string(),
                profiles,
                profiles_order: vec!["api".to_string()],
            },
        }
    }

    fn data_with_nodes() -> crate::data::Data {
        let mut nodes = HashMap::new();
        nodes.insert("node1".to_string(), node_with_profiles());
        nodes.insert("node2".to_string(), second_node_with_profiles());

        crate::data::Data {
            generic_settings: empty_generic_settings(),
            nodes,
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

    fn node_with_profiles() -> crate::data::Node {
        let mut profiles = HashMap::new();
        profiles.insert("system".to_string(), profile_with_tags(&["core", "prod"]));
        profiles.insert("metrics".to_string(), profile_with_tags(&["observability"]));
        profiles.insert(
            "logs".to_string(),
            profile_with_tags(&["observability", "prod"]),
        );

        crate::data::Node {
            generic_settings: empty_generic_settings(),
            node_settings: crate::data::NodeSettings {
                hostname: "example.com".to_string(),
                profiles,
                profiles_order: vec!["logs".to_string(), "system".to_string()],
            },
        }
    }

    #[test]
    fn test_build_repo_reqs_single_target() {
        let flakes = vec![DeployFlake {
            repo: "repo1",
            node: Some("node1".to_string()),
            profile: Some("profile1".to_string()),
        }];
        let reqs = build_repo_reqs(&flakes).unwrap();

        assert_eq!(reqs.len(), 1);
        let req = reqs.get("repo1").unwrap();
        assert!(!req.all_nodes);
        assert_eq!(req.nodes.len(), 1);
        let n_req = req.nodes.get("node1").unwrap();
        assert!(!n_req.all_profiles);
        assert_eq!(
            n_req.profiles,
            vec!["profile1"].into_iter().collect::<HashSet<_>>()
        );
    }

    #[test]
    fn test_build_repo_reqs_multiple_targets_same_repo() {
        let flakes = vec![
            DeployFlake {
                repo: "repo1",
                node: Some("node1".to_string()),
                profile: Some("profile1".to_string()),
            },
            DeployFlake {
                repo: "repo1",
                node: Some("node1".to_string()),
                profile: Some("profile2".to_string()),
            },
            DeployFlake {
                repo: "repo1",
                node: Some("node2".to_string()),
                profile: None,
            },
        ];
        let reqs = build_repo_reqs(&flakes).unwrap();

        assert_eq!(reqs.len(), 1);
        let req = reqs.get("repo1").unwrap();
        assert_eq!(req.nodes.len(), 2);

        let n1_req = req.nodes.get("node1").unwrap();
        assert_eq!(n1_req.profiles.len(), 2);
        assert!(n1_req.profiles.contains("profile1"));
        assert!(n1_req.profiles.contains("profile2"));

        let n2_req = req.nodes.get("node2").unwrap();
        assert!(n2_req.all_profiles);
    }

    #[test]
    fn test_build_repo_reqs_all_nodes() {
        let flakes = vec![DeployFlake {
            repo: "repo1",
            node: None,
            profile: None,
        }];
        let reqs = build_repo_reqs(&flakes).unwrap();

        assert_eq!(reqs.len(), 1);
        let req = reqs.get("repo1").unwrap();
        assert!(req.all_nodes);
    }

    #[test]
    fn test_build_repo_reqs_multiple_repos() {
        let flakes = vec![
            DeployFlake {
                repo: "repo1",
                node: Some("node1".to_string()),
                profile: None,
            },
            DeployFlake {
                repo: "repo2",
                node: Some("node2".to_string()),
                profile: None,
            },
        ];
        let reqs = build_repo_reqs(&flakes).unwrap();

        assert_eq!(reqs.len(), 2);
        assert!(reqs.contains_key("repo1"));
        assert!(reqs.contains_key("repo2"));
    }

    #[test]
    fn test_build_repo_reqs_invalid_profile_no_node() {
        let flakes = vec![DeployFlake {
            repo: "repo1",
            node: None,
            profile: Some("profile1".to_string()),
        }];
        let res = build_repo_reqs(&flakes);
        assert!(matches!(res, Err(GetDeploymentDataError::ProfileNoNode)));
    }

    #[test]
    fn test_ordered_profiles_for_node_filters_tags_and_preserves_order() {
        let node = node_with_profiles();
        let tags = HashSet::from(["observability"]);

        let profiles = ordered_profiles_for_node(&node, &tags).unwrap();
        let names: Vec<_> = profiles.into_iter().map(|(name, _)| name).collect();

        assert_eq!(names, vec!["logs", "metrics"]);
    }

    #[test]
    fn test_ordered_profiles_for_node_without_tags_returns_all_profiles_once() {
        let node = node_with_profiles();

        let profiles = ordered_profiles_for_node(&node, &HashSet::new()).unwrap();
        let names: Vec<_> = profiles.into_iter().map(|(name, _)| name).collect();

        assert_eq!(names, vec!["logs", "system", "metrics"]);
    }

    #[test]
    fn test_profile_matches_tags_supports_empty_matching_all_matching_and_non_matching_sets() {
        let profile = profile_with_tags(&["prod", "blue"]);

        assert!(profile_matches_tags(&profile, &HashSet::new()));
        assert!(profile_matches_tags(&profile, &HashSet::from(["prod"])));
        assert!(profile_matches_tags(
            &profile,
            &HashSet::from(["blue", "prod"])
        ));
        assert!(!profile_matches_tags(
            &profile,
            &HashSet::from(["missing", "blue"])
        ));
        assert!(!profile_matches_tags(&profile, &HashSet::from(["missing"])));
    }

    #[test]
    fn test_ordered_profiles_for_node_errors_when_profiles_order_references_missing_profile() {
        let mut node = node_with_profiles();
        node.node_settings
            .profiles_order
            .push("missing".to_string());

        let result = ordered_profiles_for_node(&node, &HashSet::new());

        assert!(matches!(
            result,
            Err(RunDeployError::ProfileNotFound(name)) if name == "missing"
        ));
    }

    #[test]
    fn test_collect_to_deploy_explicit_profile_matches_tag() {
        let deploy_flakes = vec![DeployFlake {
            repo: "repo1",
            node: Some("node1".to_string()),
            profile: Some("system".to_string()),
        }];
        let data = vec![data_with_nodes()];
        let tags = vec!["prod".to_string()];

        let to_deploy = collect_to_deploy(&deploy_flakes, &data, &tags).unwrap();
        let names: Vec<_> = to_deploy
            .into_iter()
            .map(|(_, _, (node_name, _), (profile_name, _))| (node_name, profile_name))
            .collect();

        assert_eq!(names, vec![("node1", "system")]);
    }

    #[test]
    fn test_collect_to_deploy_explicit_profile_errors_on_missing_node() {
        let deploy_flakes = vec![DeployFlake {
            repo: "repo1",
            node: Some("missing-node".to_string()),
            profile: Some("system".to_string()),
        }];
        let data = vec![data_with_nodes()];
        let tags = vec!["prod".to_string()];

        let result = collect_to_deploy(&deploy_flakes, &data, &tags);

        assert!(matches!(
            result,
            Err(RunDeployError::NodeNotFound(name)) if name == "missing-node"
        ));
    }

    #[test]
    fn test_collect_to_deploy_explicit_profile_errors_on_missing_profile() {
        let deploy_flakes = vec![DeployFlake {
            repo: "repo1",
            node: Some("node1".to_string()),
            profile: Some("missing-profile".to_string()),
        }];
        let data = vec![data_with_nodes()];
        let tags = vec!["prod".to_string()];

        let result = collect_to_deploy(&deploy_flakes, &data, &tags);

        assert!(matches!(
            result,
            Err(RunDeployError::ProfileNotFound(name)) if name == "missing-profile"
        ));
    }

    #[test]
    fn test_collect_to_deploy_node_target_filters_profiles_by_tag() {
        let deploy_flakes = vec![DeployFlake {
            repo: "repo1",
            node: Some("node1".to_string()),
            profile: None,
        }];
        let data = vec![data_with_nodes()];
        let tags = vec!["observability".to_string()];

        let to_deploy = collect_to_deploy(&deploy_flakes, &data, &tags).unwrap();
        let names: Vec<_> = to_deploy
            .into_iter()
            .map(|(_, _, (node_name, _), (profile_name, _))| (node_name, profile_name))
            .collect();

        assert_eq!(names, vec![("node1", "logs"), ("node1", "metrics")]);
    }

    #[test]
    fn test_collect_to_deploy_node_target_errors_on_missing_node() {
        let deploy_flakes = vec![DeployFlake {
            repo: "repo1",
            node: Some("missing-node".to_string()),
            profile: None,
        }];
        let data = vec![data_with_nodes()];
        let tags = vec!["observability".to_string()];

        let result = collect_to_deploy(&deploy_flakes, &data, &tags);

        assert!(matches!(
            result,
            Err(RunDeployError::NodeNotFound(name)) if name == "missing-node"
        ));
    }

    #[test]
    fn test_collect_to_deploy_all_nodes_filters_matching_profiles_across_nodes() {
        let deploy_flakes = vec![DeployFlake {
            repo: "repo1",
            node: None,
            profile: None,
        }];
        let data = vec![data_with_nodes()];
        let tags = vec!["prod".to_string()];

        let mut names: Vec<_> = collect_to_deploy(&deploy_flakes, &data, &tags)
            .unwrap()
            .into_iter()
            .map(|(_, _, (node_name, _), (profile_name, _))| (node_name, profile_name))
            .collect();
        names.sort();

        assert_eq!(
            names,
            vec![("node1", "logs"), ("node1", "system"), ("node2", "api")]
        );
    }

    #[test]
    fn test_collect_to_deploy_rejects_profile_without_node() {
        let deploy_flakes = vec![DeployFlake {
            repo: "repo1",
            node: None,
            profile: Some("system".to_string()),
        }];
        let data = vec![data_with_nodes()];

        let result = collect_to_deploy(&deploy_flakes, &data, &[]);

        assert!(matches!(result, Err(RunDeployError::ProfileWithoutNode)));
    }

    #[tokio::test]
    async fn test_run_deploy_errors_when_tags_match_no_profiles() {
        let deploy_flakes = vec![DeployFlake {
            repo: "repo1",
            node: Some("node1".to_string()),
            profile: Some("system".to_string()),
        }];
        let data = vec![data_with_nodes()];
        let tags = vec!["staging".to_string()];
        let log_dir = None;

        let result = run_deploy(
            deploy_flakes,
            data,
            true,
            false,
            false,
            &empty_cmd_overrides(),
            false,
            None,
            &[],
            false,
            false,
            false,
            &log_dir,
            true,
            false,
            false,
            false,
            false,
            &tags,
        )
        .await;

        assert!(matches!(
            result,
            Err(RunDeployError::NoProfilesMatchedTags(requested)) if requested == "staging"
        ));
    }

    #[tokio::test]
    async fn test_get_deployment_data_integration() {
        // This test requires 'nix' to be installed.
        if std::process::Command::new("nix")
            .arg("--version")
            .status()
            .is_err()
        {
            return;
        }

        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let flake_path = dir.path().join("flake.nix");
        let flake_content = r#"
{
  outputs = { self }: {
    deploy = {
      nodes = {
        node1 = {
          hostname = "node1-host";
          profiles = {
            p1 = { path = "/nix/store/p1"; tags = [ "web" ]; };
            p2 = { path = "/nix/store/p2"; tags = [ "db" "prod" ]; };
          };
        };
        node2 = {
          hostname = "node2-host";
          profiles = {
            pA = { path = "/nix/store/pA"; tags = [ "web" ]; };
          };
        };
      };
    };
  };
}
"#;
        fs::write(&flake_path, flake_content).unwrap();

        let repo = dir.path().to_str().unwrap();

        // Branch 1: req.all_nodes = true
        let flakes = vec![DeployFlake {
            repo,
            node: None,
            profile: None,
        }];
        let data = get_deployment_data(true, &flakes, &[]).await.unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].nodes.len(), 2);
        assert!(data[0].nodes.contains_key("node1"));
        assert!(data[0].nodes.contains_key("node2"));
        assert_eq!(
            data[0].nodes["node1"].node_settings.profiles["p1"]
                .profile_settings
                .tags,
            vec!["web".to_string()]
        );

        // Branch 2: req.all_nodes = false
        // Branch 2a: node1 in req.nodes
        // Branch 2a-i: node1.all_profiles = true (node1 has both p1 and p2)
        let flakes = vec![DeployFlake {
            repo,
            node: Some("node1".to_string()),
            profile: None,
        }];
        let data = get_deployment_data(true, &flakes, &[]).await.unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].nodes.len(), 1);
        assert!(data[0].nodes.contains_key("node1"));
        assert_eq!(data[0].nodes["node1"].node_settings.profiles.len(), 2);

        // Branch 2a-ii: node1.all_profiles = false (node1 only has p1)
        let flakes = vec![DeployFlake {
            repo,
            node: Some("node1".to_string()),
            profile: Some("p1".to_string()),
        }];
        let data = get_deployment_data(true, &flakes, &[]).await.unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].nodes.len(), 1);
        assert_eq!(data[0].nodes["node1"].node_settings.profiles.len(), 1);
        assert!(data[0].nodes["node1"]
            .node_settings
            .profiles
            .contains_key("p1"));
        assert!(!data[0].nodes["node1"]
            .node_settings
            .profiles
            .contains_key("p2"));

        // Branch 2b: Multiple repos and mixed targets (implicitly tests filtering out node2 when only node1 is requested)
        // Branch 2b: Multiple repos and mixed targets
        // Note: Currently, all targets for the same repo get the same combined batched result.
        let flakes = vec![
            DeployFlake {
                repo,
                node: Some("node1".to_string()),
                profile: Some("p1".to_string()),
            },
            DeployFlake {
                repo,
                node: Some("node2".to_string()),
                profile: None,
            },
        ];
        let data = get_deployment_data(true, &flakes, &[]).await.unwrap();
        assert_eq!(data.len(), 2);
        // Both targets share the same combined result for the repo
        for d in data {
            assert_eq!(d.nodes.len(), 2);
            assert!(d.nodes.contains_key("node1"));
            assert!(d.nodes.contains_key("node2"));
            // node1 should have reached here with only p1 (filtered by Nix)
            assert_eq!(d.nodes["node1"].node_settings.profiles.len(), 1);
            assert!(d.nodes["node1"].node_settings.profiles.contains_key("p1"));
            // node2 should have all its profiles (pA)
            assert_eq!(d.nodes["node2"].node_settings.profiles.len(), 1);
            assert!(d.nodes["node2"].node_settings.profiles.contains_key("pA"));
        }
    }
}

#[derive(Error, Debug)]
pub enum RunError {
    #[error("Failed to deploy profile: {0}")]
    DeployProfile(#[from] deploy::deploy::DeployProfileError),
    #[error("Failed to push profile: {0}")]
    PushProfile(#[from] deploy::push::PushProfileError),
    #[error("Failed to test for flake support: {0}")]
    FlakeTest(std::io::Error),
    #[error("Failed to check deployment: {0}")]
    CheckDeployment(#[from] CheckDeploymentError),
    #[error("Failed to evaluate deployment data: {0}")]
    GetDeploymentData(#[from] GetDeploymentDataError),
    #[error("Error parsing flake: {0}")]
    ParseFlake(#[from] deploy::ParseFlakeError),
    #[error("Error parsing arguments: {0}")]
    ParseArgs(#[from] clap::Error),
    #[error("Error parsing sudo configuration: {0}")]
    SudoParse(#[from] deploy::sudo::SudoParseError),
    #[error("Error initiating logger: {0}")]
    Logger(#[from] flexi_logger::FlexiLoggerError),
    #[error("{0}")]
    RunDeploy(#[from] RunDeployError),
}

pub async fn run(args: Option<&ArgMatches>) -> Result<(), RunError> {
    let opts = match args {
        Some(o) => <Opts as FromArgMatches>::from_arg_matches(o)?,
        None => Opts::parse(),
    };

    deploy::init_logger(
        opts.debug_logs,
        opts.log_dir.as_deref(),
        &deploy::LoggerType::Deploy,
    )?;

    if opts.dry_activate && opts.boot {
        error!("Cannot use both --dry-activate & --boot!");
    }

    let deploys = opts
        .clone()
        .targets
        .unwrap_or_else(|| vec![opts.clone().target.unwrap_or_else(|| ".".to_string())]);

    let deploy_flakes: Vec<DeployFlake> = if let Some(file) = &opts.file {
        deploys
            .iter()
            .map(|f| deploy::parse_file(file.as_str(), f.as_str()))
            .collect::<Result<Vec<DeployFlake>, ParseFlakeError>>()?
    } else {
        deploys
            .iter()
            .map(|f| deploy::parse_flake(f.as_str()))
            .collect::<Result<Vec<DeployFlake>, ParseFlakeError>>()?
    };

    let sudo = opts
        .sudo
        .as_deref()
        .map(deploy::sudo::SudoCommand::parse_legacy)
        .transpose()?;

    let cmd_overrides = deploy::CmdOverrides {
        ssh_user: opts.ssh_user,
        profile_user: opts.profile_user,
        ssh_opts: opts.ssh_opts,
        fast_connection: opts.fast_connection,
        auto_rollback: opts.auto_rollback,
        hostname: opts.hostname,
        magic_rollback: opts.magic_rollback,
        temp_path: opts.temp_path,
        confirm_timeout: opts.confirm_timeout,
        activation_timeout: opts.activation_timeout,
        dry_activate: opts.dry_activate,
        remote_build: opts.remote_build,
        sudo,
        interactive_sudo: opts.interactive_sudo,
    };

    let supports_flakes = test_flake_support().await.map_err(RunError::FlakeTest)?;
    let do_not_want_flakes = opts.file.is_some();

    if !supports_flakes {
        warn!("A Nix version without flakes support was detected, support for this is work in progress");
    }

    if do_not_want_flakes {
        warn!("The --file option for deployments without flakes is experimental");
    }

    let using_flakes = supports_flakes && !do_not_want_flakes;

    if !opts.skip_checks {
        for deploy_flake in &deploy_flakes {
            check_deployment(using_flakes, deploy_flake.repo, &opts.extra_build_args).await?;
        }
    }
    let result_path = opts.result_path.as_deref();
    let data = get_deployment_data(using_flakes, &deploy_flakes, &opts.extra_build_args).await?;
    let build_tree = opts.build_tree && !opts.no_build_tree;
    let review_changes = opts.review_changes && !opts.no_review_changes;

    run_deploy(
        deploy_flakes,
        data,
        using_flakes,
        opts.checksigs,
        opts.interactive,
        &cmd_overrides,
        opts.keep_result,
        result_path,
        &opts.extra_build_args,
        opts.debug_logs,
        opts.dry_activate,
        opts.boot,
        &opts.log_dir,
        opts.rollback_succeeded.unwrap_or(true),
        !opts.no_ssh_multiplexing,
        !opts.no_rollback_fresh_connection,
        build_tree,
        review_changes,
        &opts.tags,
    )
    .await?;

    Ok(())
}
