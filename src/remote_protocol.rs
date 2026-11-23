// SPDX-FileCopyrightText: 2026 deploy-rx contributors
//
// SPDX-License-Identifier: MPL-2.0

use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::sudo::SudoCommand;

pub const REMOTE_PROTOCOL_VERSION: u16 = 5;

/// How to review derivation changes before activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ReviewChanges {
    /// Do not review derivation changes.
    #[value(alias = "false")]
    Off,
    /// Review changes, hiding packages whose version did not change.
    #[value(alias = "true")]
    Filtered,
    /// Review changes, showing every package.
    Full,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackReceipt {
    pub session_id: String,
    pub previous_session: Option<String>,
    pub previous_link: Option<PathBuf>,
    pub previous_target: Option<PathBuf>,
    pub previous_running_target: Option<PathBuf>,
    pub expected_link: Option<PathBuf>,
    pub created_generation: Option<String>,
    pub boot: bool,
    pub test: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProfileTarget {
    ProfilePath {
        profile_path: String,
    },
    ProfileUserAndName {
        profile_user: String,
        profile_name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteDeployRequest {
    pub closure: String,
    pub profile: ProfileTarget,
    pub profile_user: String,
    pub review_changes: ReviewChanges,
    pub dry_activate: bool,
    pub boot: bool,
    pub test: bool,
    pub auto_rollback: bool,
    pub magic_rollback: bool,
    pub confirm_timeout: u16,
    pub activation_timeout: Option<u16>,
    pub temp_path: String,
    pub debug_logs: bool,
    pub log_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteRevokeRequest {
    pub closure: String,
    pub profile: ProfileTarget,
    pub rollback: RollbackReceipt,
    pub profile_user: String,
    pub temp_path: String,
    pub debug_logs: bool,
    pub log_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RemoteOperation {
    Deploy(RemoteDeployRequest),
    Revoke(RemoteRevokeRequest),
}

impl RemoteOperation {
    pub fn profile_user(&self) -> &str {
        match self {
            RemoteOperation::Deploy(request) => &request.profile_user,
            RemoteOperation::Revoke(request) => &request.profile_user,
        }
    }

    pub fn temp_path(&self) -> &str {
        match self {
            RemoteOperation::Deploy(request) => &request.temp_path,
            RemoteOperation::Revoke(request) => &request.temp_path,
        }
    }

    pub fn debug_logs(&self) -> bool {
        match self {
            RemoteOperation::Deploy(request) => request.debug_logs,
            RemoteOperation::Revoke(request) => request.debug_logs,
        }
    }

    pub fn log_dir(&self) -> Option<&str> {
        match self {
            RemoteOperation::Deploy(request) => request.log_dir.as_deref(),
            RemoteOperation::Revoke(request) => request.log_dir.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapRequest {
    pub protocol_version: u16,
    pub sudo: Option<SudoCommand>,
    pub sudo_password: Option<String>,
    pub interactive_sudo: bool,
    pub operation: RemoteOperation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmRequest {
    pub temp_path: String,
    pub session_id: String,
    pub nonce: String,
    pub sudo: Option<SudoCommand>,
    pub sudo_password: Option<String>,
    pub interactive_sudo: bool,
    pub profile_user: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteLogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteLogSource {
    Activate,
    Revoke,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RemoteEvent {
    Hello {
        protocol_version: u16,
    },
    Log {
        level: RemoteLogLevel,
        source: RemoteLogSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase: Option<String>,
        message: String,
    },
    RawOutput {
        text: String,
    },
    AwaitingConfirm {
        session_id: String,
        nonce: String,
    },
    Finished {
        ok: bool,
        rolled_back: bool,
        message: String,
        rollback: Option<RollbackReceipt>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_log_event_round_trips_through_json() {
        let event = RemoteEvent::Log {
            level: RemoteLogLevel::Warn,
            source: RemoteLogSource::Activate,
            phase: Some("confirm".to_string()),
            message: "Waiting for confirmation".to_string(),
        };

        let json = serde_json::to_string(&event).unwrap();
        let decoded: RemoteEvent = serde_json::from_str(&json).unwrap();
        match decoded {
            RemoteEvent::Log {
                level,
                source,
                phase,
                message,
            } => {
                assert_eq!(level, RemoteLogLevel::Warn);
                assert_eq!(source, RemoteLogSource::Activate);
                assert_eq!(phase.as_deref(), Some("confirm"));
                assert_eq!(message, "Waiting for confirmation");
            }
            other => panic!("expected Log event, got {:?}", other),
        }
    }

    #[test]
    fn remote_raw_output_event_round_trips_through_json() {
        let event = RemoteEvent::RawOutput {
            text: "<<< /nix/store/old\n>>> /nix/store/new\n".to_string(),
        };

        let json = serde_json::to_string(&event).unwrap();
        let decoded: RemoteEvent = serde_json::from_str(&json).unwrap();
        match decoded {
            RemoteEvent::RawOutput { text } => {
                assert_eq!(text, "<<< /nix/store/old\n>>> /nix/store/new\n");
            }
            other => panic!("expected RawOutput event, got {:?}", other),
        }
    }
}
