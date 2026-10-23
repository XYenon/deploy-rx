// SPDX-FileCopyrightText: 2025 Serokell <https://serokell.io/>
//
// SPDX-License-Identifier: MPL-2.0

use std::fmt;
use std::fmt::Debug;

use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::Command as TokioCommand;

pub trait HasCommandError {
    fn title() -> String;
}

#[derive(Error, Debug)]
pub enum CommandError<T> {
    RunError(std::io::Error),
    Exit(std::process::Output, String),
    ExitStatus(std::process::ExitStatus, String),
    OtherError(T),
}

impl<T: HasCommandError + Debug + fmt::Display> fmt::Display for CommandError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandError::RunError(err) => {
                write!(f, "Failed to run {} command: {}", T::title(), err)
            }
            CommandError::Exit(output, cmd) => {
                let stderr = match String::from_utf8(output.stderr.clone()) {
                    Ok(stderr) => stderr,
                    Err(_err) => format!("{:?}", output.stderr),
                };
                write!(
                    f,
                    "{} command resulted in a bad exit status: {}.\nThe failed command is provided below:\n{}\nThe stderr output is provided below:\n{}",
                    T::title(),
                    output.status,
                    unescape::unescape(cmd).unwrap_or(cmd.clone()),
                    stderr,
                )
            }
            CommandError::ExitStatus(status, cmd) => {
                write!(
                    f,
                    "{} command resulted in a bad exit status: {}.\nThe failed command is provided below:\n{}",
                    T::title(),
                    status,
                    unescape::unescape(cmd).unwrap_or(cmd.clone()),
                )
            }
            CommandError::OtherError(err) => write!(f, "{}", err),
        }
    }
}

#[derive(Debug)]
pub struct Command {
    pub command: TokioCommand,
}

impl Command {
    pub fn new(command: TokioCommand) -> Self {
        Self { command }
    }

    /// Nix owns the outcome; nom owns only its private terminal and log pipe.
    pub(crate) async fn run_with_tree<T: HasCommandError + Debug + fmt::Display>(
        &mut self,
        operation: &str,
        stdout: std::process::Stdio,
    ) -> Result<Vec<u8>, CommandError<T>> {
        use crate::display;
        use std::process::Stdio;

        display::run(true, async {
            let label = display::command_label(&self.command, operation);
            let panel = display::panel(label.clone());
            let mut monitor_failed = false;
            let result = async {
                let (terminal, nom_stderr) = display::nom_terminal(panel)?;
                self.command
                    .arg("--log-format")
                    .arg("internal-json")
                    .arg("--verbose")
                    .stdout(stdout)
                    .stderr(Stdio::piped());
                let mut nix_child = self.command.kill_on_drop(true).spawn()?;
                let nix_stdout = nix_child.stdout.take();
                let nix_stderr = nix_child.stderr.take().unwrap();
                let mut nom_command = TokioCommand::new("nom");
                if let Some((_, Some(path))) = self
                    .command
                    .as_std()
                    .get_envs()
                    .find(|(key, _)| *key == "PATH")
                {
                    nom_command.env("PATH", path);
                }
                let start_nom = nom_command
                    .arg("--json")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(nom_stderr)
                    .kill_on_drop(true)
                    .spawn();
                // Release Command's slave descriptor so monitor exit yields EOF.
                drop(nom_command);
                let mut nom_child = match start_nom {
                    Ok(child) => child,
                    Err(error) => {
                        let _ = nix_child.kill().await;
                        return Err(error);
                    }
                };
                let nom_stdin = nom_child.stdin.take().unwrap();
                let nom_stdout = nom_child.stdout.take().unwrap();
                let mut stdout = Vec::new();
                let monitor = async {
                    // Drain Nix even when nom exits or stops consuming input.
                    let feed = display::feed_nom(&label, nix_stderr, nom_stdin).await;
                    let status = match tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        nom_child.wait(),
                    )
                    .await
                    {
                        Ok(status) => status,
                        Err(_) => {
                            let _ = nom_child.kill().await;
                            Err(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "nom did not exit after EOF",
                            ))
                        }
                    };
                    (feed, status)
                };
                let (nix_status, (feed, nom_status), read, screen, logs) = tokio::join!(
                    nix_child.wait(),
                    monitor,
                    async {
                        if let Some(mut input) = nix_stdout {
                            input.read_to_end(&mut stdout).await?;
                        }
                        Ok::<_, std::io::Error>(())
                    },
                    display::read_nom(panel, terminal),
                    display::stream(&label, nom_stdout),
                );
                let nix_status = nix_status?;
                read?;
                // nom returning 1 for a Nix error is a successful error display.
                monitor_failed = (nix_status.success()
                    && !nom_status.as_ref().is_ok_and(|status| status.success()))
                    || feed.is_err()
                    || screen.is_err()
                    || logs.is_err();
                if monitor_failed {
                    log::warn!(
                        "{label}: monitor failed ({nom_status:?}); continuing based on Nix result"
                    );
                }
                Ok((nix_status, stdout))
            }
            .await;
            let success = result.as_ref().is_ok_and(|(status, _)| status.success());
            display::finish(
                panel,
                if success && monitor_failed {
                    "completed; monitor FAILED"
                } else if success {
                    "completed"
                } else {
                    "FAILED"
                },
            );
            let (status, stdout) = result.map_err(CommandError::RunError)?;
            if status.success() {
                Ok(stdout)
            } else {
                Err(CommandError::ExitStatus(
                    status,
                    format!("{:?}", self.command),
                ))
            }
        })
        .await
    }

    pub async fn run<T: HasCommandError + Debug + fmt::Display>(
        &mut self,
    ) -> Result<std::process::Output, CommandError<T>> {
        let output = self
            .command
            .output()
            .await
            .map_err(CommandError::RunError)?;
        match output.status.code() {
            Some(0) => Ok(output),
            _exit_code => Err(CommandError::Exit(output, format!("{:?}", self.command))),
        }
    }

    pub async fn status<T: HasCommandError + Debug + fmt::Display>(
        &mut self,
    ) -> Result<std::process::ExitStatus, CommandError<T>> {
        if crate::display::active() {
            let label = crate::display::command_label(&self.command, &T::title());
            let panel = crate::display::panel(label.clone());
            let result = async {
                let mut child = self
                    .command
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(CommandError::RunError)?;
                let stdout = child.stdout.take().unwrap();
                let stderr = child.stderr.take().unwrap();
                let (status, stdout, stderr) = tokio::join!(
                    child.wait(),
                    crate::display::stream(&label, stdout),
                    crate::display::stream(&label, stderr),
                );
                let status = status.map_err(CommandError::RunError)?;
                stdout.map_err(CommandError::RunError)?;
                stderr.map_err(CommandError::RunError)?;
                if status.success() {
                    Ok(status)
                } else {
                    Err(CommandError::ExitStatus(
                        status,
                        format!("{:?}", self.command),
                    ))
                }
            }
            .await;
            crate::display::finish(
                panel,
                if result.is_ok() {
                    "completed"
                } else {
                    "FAILED"
                },
            );
            return result;
        }
        let status = self
            .command
            .status()
            .await
            .map_err(CommandError::RunError)?;
        match status.code() {
            Some(0) => Ok(status),
            _exit_code => Err(CommandError::ExitStatus(
                status,
                format!("{:?}", self.command),
            )),
        }
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.command.fmt(f)
    }
}
