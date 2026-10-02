// SPDX-FileCopyrightText: 2024 deploy-rs contributors
//
// SPDX-License-Identifier: MPL-2.0

use log::{debug, info, warn};
use std::collections::HashMap;
use std::env;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::process::Command;
use tokio::sync::Mutex;

const MAX_RUNTIME_DIR_LEN: usize = 32;
static SOCKET_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn get_runtime_dir() -> PathBuf {
    let preferred = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("TMPDIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/tmp"));

    // OpenSSH's Unix-domain ControlPath is usually limited to roughly 100 bytes.
    // Keep enough room for the per-run directory and socket filename even when
    // the configured runtime directory is unusually long.
    if preferred.as_os_str().len() <= MAX_RUNTIME_DIR_LEN {
        preferred
    } else {
        PathBuf::from("/tmp")
    }
}

fn unique_socket_dir() -> PathBuf {
    let sequence = SOCKET_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    get_runtime_dir().join(format!(
        "deploy-rx-{:x}-{:x}-{:x}",
        std::process::id(),
        timestamp,
        sequence
    ))
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct MasterIdentity {
    hostname: String,
    ssh_user: Option<String>,
    ssh_opts: Vec<String>,
}

impl MasterIdentity {
    fn new(hostname: &str, ssh_user: Option<&str>, ssh_opts: &[String]) -> Self {
        Self {
            hostname: hostname.to_owned(),
            ssh_user: ssh_user.map(str::to_owned),
            ssh_opts: ssh_opts.to_vec(),
        }
    }

    fn socket_name(&self) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut hasher);
        format!("cm-{:016x}", hasher.finish())
    }
}

#[derive(Error, Debug)]
pub enum SshError {
    #[error("Failed to create control path directory: {0}")]
    CreateControlDir(std::io::Error),
    #[error("Failed to spawn SSH master: {0}")]
    SpawnMaster(std::io::Error),
    #[error("SSH master exited with error: {0:?}")]
    MasterFailed(Option<i32>),
    #[error("Failed to close SSH master: {0}")]
    CloseMaster(std::io::Error),
}

pub struct SshControlMaster {
    control_path: PathBuf,
    hostname: String,
    ssh_user: Option<String>,
    ssh_opts: Vec<String>,
}

impl SshControlMaster {
    pub fn new(
        hostname: &str,
        ssh_user: Option<&str>,
        ssh_opts: &[String],
        temp_path: &Path,
    ) -> Self {
        let identity = MasterIdentity::new(hostname, ssh_user, ssh_opts);
        let control_path = temp_path.join(identity.socket_name());

        Self {
            control_path,
            hostname: hostname.to_string(),
            ssh_user: ssh_user.map(|s| s.to_string()),
            ssh_opts: ssh_opts.to_vec(),
        }
    }

    fn ssh_addr(&self) -> String {
        match &self.ssh_user {
            Some(user) => format!("{}@{}", user, self.hostname),
            None => self.hostname.clone(),
        }
    }

    pub fn control_path(&self) -> &Path {
        &self.control_path
    }

    pub fn control_opts(&self) -> Vec<String> {
        vec!["-S".to_string(), self.control_path.display().to_string()]
    }

    pub async fn start(&self) -> Result<(), SshError> {
        if let Some(parent) = self.control_path.parent() {
            create_private_dir(parent).map_err(SshError::CreateControlDir)?;
        }

        let ssh_addr = self.ssh_addr();
        info!("Establishing SSH control master to {}", ssh_addr);

        let mut cmd = Command::new("ssh");
        cmd.arg("-o")
            .arg("ControlMaster=yes")
            .arg("-o")
            .arg("ControlPersist=yes")
            .arg("-N")
            .arg("-f");

        for opt in &self.ssh_opts {
            cmd.arg(opt);
        }

        cmd.args(self.control_opts()).arg(&ssh_addr);

        debug!("SSH master command: {:?}", cmd);

        let status = cmd.status().await.map_err(SshError::SpawnMaster)?;

        if !status.success() {
            return Err(SshError::MasterFailed(status.code()));
        }

        debug!(
            "SSH control master established at {}",
            self.control_path.display()
        );
        Ok(())
    }

    pub async fn stop(&self) -> Result<(), SshError> {
        if !self.control_path.exists() {
            return Ok(());
        }

        let ssh_addr = self.ssh_addr();
        debug!("Closing SSH control master to {}", ssh_addr);

        let mut cmd = Command::new("ssh");
        cmd.arg("-o")
            .arg(format!("ControlPath={}", self.control_path.display()))
            .arg("-O")
            .arg("exit")
            .arg(&ssh_addr);

        let _ = cmd.status().await;
        Ok(())
    }
}

impl Drop for SshControlMaster {
    fn drop(&mut self) {
        if self.control_path.exists() {
            let _ = std::process::Command::new("ssh")
                .arg("-o")
                .arg(format!("ControlPath={}", self.control_path.display()))
                .arg("-O")
                .arg("exit")
                .arg(self.ssh_addr())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
}

pub struct SshMultiplexer {
    masters: Arc<Mutex<HashMap<MasterIdentity, Arc<SshControlMaster>>>>,
    socket_dir: PathBuf,
}

impl SshMultiplexer {
    pub fn new() -> Self {
        Self {
            masters: Arc::new(Mutex::new(HashMap::new())),
            socket_dir: unique_socket_dir(),
        }
    }

    pub async fn get_or_create(
        &self,
        hostname: &str,
        ssh_user: Option<&str>,
        ssh_opts: &[String],
    ) -> Result<Arc<SshControlMaster>, SshError> {
        let key = MasterIdentity::new(hostname, ssh_user, ssh_opts);

        let mut masters = self.masters.lock().await;

        if let Some(master) = masters.get(&key) {
            return Ok(Arc::clone(master));
        }

        let master = SshControlMaster::new(hostname, ssh_user, ssh_opts, &self.socket_dir);
        master.start().await?;

        let master = Arc::new(master);
        masters.insert(key, Arc::clone(&master));

        Ok(master)
    }

    pub async fn close_all(&self) {
        let mut masters = self.masters.lock().await;

        for (key, master) in masters.drain() {
            if let Err(e) = master.stop().await {
                warn!("Failed to close SSH master for {:?}: {}", key, e);
            }
        }
        let _ = std::fs::remove_dir(&self.socket_dir);
    }
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

impl Default for SshMultiplexer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_path_uses_the_complete_master_identity() {
        let dir = Path::new("/tmp/test");
        let base = SshControlMaster::new("example.org", Some("alice"), &[], dir);
        let other_user = SshControlMaster::new("example.org", Some("bob"), &[], dir);
        let other_port = SshControlMaster::new(
            "example.org",
            Some("alice"),
            &["-p".into(), "2222".into()],
            dir,
        );

        assert_ne!(base.control_path(), other_user.control_path());
        assert_ne!(base.control_path(), other_port.control_path());
        assert!(base.control_path().file_name().unwrap().len() <= 19);
    }

    #[test]
    #[ignore = "requires OpenSSH"]
    fn explicit_ssh_socket_options_cannot_override_the_session_socket() {
        for opts in [
            vec!["-o".into(), "controlpath=/tmp/other-socket".into()],
            vec!["-S".into(), "/tmp/other-socket".into()],
        ] {
            let master =
                SshControlMaster::new("example.org", Some("alice"), &opts, Path::new("/tmp/test"));
            let output = std::process::Command::new("ssh")
                .args(["-F", "/dev/null", "-G"])
                .args(&opts)
                .args(master.control_opts())
                .arg("alice@example.org")
                .output()
                .unwrap();
            assert!(output.status.success());
            let config = String::from_utf8(output.stdout).unwrap();
            assert!(config
                .lines()
                .any(|line| line == format!("controlpath {}", master.control_path().display())));
        }
    }

    #[test]
    fn each_multiplexer_has_an_exclusive_socket_directory() {
        let first = SshMultiplexer::new();
        let second = SshMultiplexer::new();

        assert_ne!(first.socket_dir, second.socket_dir);
    }

    #[cfg(unix)]
    #[test]
    fn socket_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("run");
        create_private_dir(&path).unwrap();

        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o700);
    }
}
