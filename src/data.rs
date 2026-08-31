// SPDX-FileCopyrightText: 2020 Serokell <https://serokell.io/>
//
// SPDX-License-Identifier: MPL-2.0

use merge::Merge;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::sudo::SudoCommand;

fn default_output_name() -> String {
    "out".to_string()
}

#[derive(Deserialize, Debug, Clone, Merge)]
#[merge(strategy = merge::option::overwrite_none)]
pub struct GenericSettings {
    #[serde(rename(deserialize = "sshUser"))]
    pub ssh_user: Option<String>,
    pub user: Option<String>,
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        default,
        rename(deserialize = "sshOpts")
    )]
    #[merge(strategy = merge::vec::append)]
    pub ssh_opts: Vec<String>,
    #[serde(rename(deserialize = "fastConnection"))]
    pub fast_connection: Option<bool>,
    #[serde(rename(deserialize = "autoRollback"))]
    pub auto_rollback: Option<bool>,
    #[serde(rename(deserialize = "confirmTimeout"))]
    pub confirm_timeout: Option<u16>,
    #[serde(rename(deserialize = "activationTimeout"))]
    pub activation_timeout: Option<u16>,
    #[serde(rename(deserialize = "tempPath"))]
    pub temp_path: Option<PathBuf>,
    #[serde(rename(deserialize = "magicRollback"))]
    pub magic_rollback: Option<bool>,
    #[serde(rename(deserialize = "sudo"))]
    pub sudo: Option<SudoCommand>,
    #[serde(default, rename(deserialize = "remoteBuild"))]
    pub remote_build: Option<bool>,
    #[serde(rename(deserialize = "interactiveSudo"))]
    pub interactive_sudo: Option<bool>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct NodeSettings {
    pub hostname: String,
    pub profiles: HashMap<String, Profile>,
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        default,
        rename(deserialize = "profilesOrder")
    )]
    pub profiles_order: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ProfileSettings {
    pub path: String,
    #[serde(rename(deserialize = "profilePath"))]
    pub profile_path: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// `.drv` path of the derivation that produces `path`. Populated by the
    /// internal eval transformation in `nix/transform-deploy.nix` so the binary
    /// knows which derivation to build when `path` is only a placeholder, as
    /// happens for content-addressed and floating-output derivations. The field
    /// is deliberately omitted from `interface.json` and kept `pub(crate)`; it
    /// is wire-format plumbing, not a user setting.
    #[serde(rename(deserialize = "drvPath"))]
    pub(crate) drv_path: Option<String>,
    /// Output selected from `drv_path`. Like `drv_path`, this is populated by
    /// the internal eval transformation and is not part of the public interface.
    #[serde(default = "default_output_name", rename(deserialize = "outputName"))]
    pub(crate) output_name: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Profile {
    #[serde(flatten)]
    pub profile_settings: ProfileSettings,
    #[serde(flatten)]
    pub generic_settings: GenericSettings,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Node {
    #[serde(flatten)]
    pub generic_settings: GenericSettings,
    #[serde(flatten)]
    pub node_settings: NodeSettings,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Data {
    #[serde(flatten)]
    pub generic_settings: GenericSettings,
    pub nodes: HashMap<String, Node>,
}

#[cfg(test)]
mod tests {
    use super::{GenericSettings, ProfileSettings};
    use merge::Merge;

    #[test]
    fn test_generic_settings_merge_preserves_precedence() {
        let mut profile: GenericSettings = serde_json::from_str(
            r#"{"user":"profile","sshOpts":["profile-opt"],"remoteBuild":false}"#,
        )
        .unwrap();
        let node: GenericSettings = serde_json::from_str(
            r#"{"user":"node","sshUser":"node-ssh","sshOpts":["node-opt"],"remoteBuild":true}"#,
        )
        .unwrap();

        profile.merge(node);

        assert_eq!(profile.user.as_deref(), Some("profile"));
        assert_eq!(profile.ssh_user.as_deref(), Some("node-ssh"));
        assert_eq!(profile.remote_build, Some(false));
        assert_eq!(profile.ssh_opts, vec!["profile-opt", "node-opt"]);
    }

    #[test]
    fn test_profile_settings_tags_default_to_empty() {
        let profile: ProfileSettings =
            serde_json::from_str(r#"{"path":"/nix/store/profile"}"#).unwrap();

        assert!(profile.tags.is_empty());
        assert_eq!(profile.output_name, "out");
    }

    #[test]
    fn test_profile_settings_tags_deserialize() {
        let profile: ProfileSettings =
            serde_json::from_str(r#"{"path":"/nix/store/profile","tags":["prod","system"]}"#)
                .unwrap();

        assert_eq!(profile.tags, vec!["prod", "system"]);
    }

    #[test]
    fn test_sudo_deserializes_structured_argv() {
        let settings: GenericSettings =
            serde_json::from_str(r#"{"sudo":["sudo","-u"],"sshOpts":[],"remoteBuild":false}"#)
                .unwrap();

        assert_eq!(
            settings.sudo.unwrap().argv(),
            &["sudo".to_string(), "-u".to_string()]
        );
    }

    #[test]
    fn test_sudo_deserializes_legacy_string() {
        let settings: GenericSettings =
            serde_json::from_str(r#"{"sudo":"doas -u","sshOpts":[],"remoteBuild":false}"#).unwrap();

        assert_eq!(
            settings.sudo.unwrap().argv(),
            &["doas".to_string(), "-u".to_string()]
        );
    }

    #[test]
    fn test_sudo_rejects_legacy_shell_syntax() {
        let err = serde_json::from_str::<GenericSettings>(
            r#"{"sudo":"sudo -u root; sh","sshOpts":[],"remoteBuild":false}"#,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("structured sudo"));
    }
}
