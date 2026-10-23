// SPDX-FileCopyrightText: 2025 Serokell <https://serokell.io/>
//
// SPDX-License-Identifier: MPL-2.0

use std::fmt;
use std::fmt::Debug;

use thiserror::Error;
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
