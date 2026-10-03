// SPDX-FileCopyrightText: 2026 deploy-rx contributors
//
// SPDX-License-Identifier: MPL-2.0

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::sudo::SudoCommand;

pub const REMOTE_PROTOCOL_VERSION: u16 = 3;

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
    pub review_changes: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RemoteEvent {
    Hello {
        protocol_version: u16,
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
