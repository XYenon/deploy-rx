// SPDX-FileCopyrightText: 2020 Serokell <https://serokell.io/>
//
// SPDX-License-Identifier: MPL-2.0

use merge::Merge;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::sudo::SudoCommand;

#[derive(Deserialize, Debug, Clone, Merge)]
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

    #[test]
    fn test_profile_settings_tags_default_to_empty() {
        let profile: ProfileSettings =
            serde_json::from_str(r#"{"path":"/nix/store/profile"}"#).unwrap();

        assert!(profile.tags.is_empty());
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
