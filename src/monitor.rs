// SPDX-FileCopyrightText: 2026 deploy-rx contributors
//
// SPDX-License-Identifier: MPL-2.0

//! Render Nix build progress with rom.

use std::{
    env,
    io::{self, IsTerminal, Read},
    process::{Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    thread,
    time::{Duration, Instant},
};

use log::{debug, warn};
use rom::{
    cache::BuildReportCache,
    display::format_log,
    state::current_time,
    terminal::{Admission, LiveTerminal},
    EngineConfig, FilesystemResolver, Output, RenderConfig, StreamEngine,
};
use tokio::process::Command as TokioCommand;

const TIMER_INTERVAL: Duration = Duration::from_secs(1);

/// The outcome of a Nix command rendered through rom.
pub struct MonitoredCommand {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
}

/// Runs `command` with its build logs rendered by rom.
///
/// Nix is asked for `internal-json` logs and both output streams are drained
/// concurrently so a full pipe can never block the build. If monitoring fails,
/// the child is killed because nothing would drain its logs anymore.
pub fn run(mut command: Command) -> io::Result<MonitoredCommand> {
    command
        .args(["--log-format", "internal-json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("failed to capture Nix stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("failed to capture Nix stderr"))?;

    let (sender, chunks) = mpsc::sync_channel(64);
    spawn_reader(stdout, &sender, Chunk::Stdout);
    spawn_reader(stderr, &sender, Chunk::Stderr);
    drop(sender);

    let mut monitor = Monitor::new();
    let mut output = Vec::new();
    let result = monitor.drive(&chunks, &mut output);
    drop(chunks);

    if let Err(error) = result {
        if let Err(kill_error) = child.kill() {
            warn!("failed to stop monitored Nix command: {kill_error}");
        }
        let _ = child.wait();
        return Err(error);
    }

    let status = child.wait()?;
    if let Err(error) = monitor.finish(status.success()) {
        debug!("rom failed to present the Nix build: {error}");
    }

    Ok(MonitoredCommand {
        status,
        stdout: output,
    })
}

/// Runs `command` on a blocking thread so the async deployment can await it.
pub(crate) async fn run_async(
    command: TokioCommand,
    context: &str,
) -> io::Result<MonitoredCommand> {
    tokio::task::spawn_blocking(move || run(command.into_std()))
        .await
        .map_err(|error| io::Error::other(format!("failed waiting for {context}: {error}")))?
}

enum Chunk {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

fn spawn_reader<R>(
    mut reader: R,
    sender: &SyncSender<io::Result<Chunk>>,
    chunk: fn(Vec<u8>) -> Chunk,
) where
    R: Read + Send + 'static,
{
    let sender = sender.clone();
    thread::spawn(move || {
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return,
                Ok(count) => {
                    if sender.send(Ok(chunk(buffer[..count].to_vec()))).is_err() {
                        return;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            }
        }
    });
}

struct Monitor {
    stream: StreamEngine,
    terminal: LiveTerminal<io::Stderr>,
    render: RenderConfig,
    history: Option<BuildReportCache>,
    dirty: bool,
}

impl Monitor {
    fn new() -> Self {
        let mut stream = StreamEngine::new(EngineConfig::default());
        stream
            .engine_mut()
            .set_resolver(FilesystemResolver::default());

        let history = match BuildReportCache::default_cache_path() {
            Ok(path) => Some(BuildReportCache::new(path)),
            Err(error) => {
                debug!("failed to locate rom build history: {error}");
                None
            }
        };
        if let Some(history) = &history {
            stream.engine_mut().load_history(history);
        }

        let dumb = env::var("TERM").as_deref() == Ok("dumb");
        let render = RenderConfig {
            ansi: io::stderr().is_terminal() && !dumb && env::var_os("NO_COLOR").is_none(),
            ..RenderConfig::default()
        };

        let mut terminal = LiveTerminal::new(io::stderr());
        if dumb || rom::terminal::admission() != Admission::Live {
            if let Err(error) = terminal.retire() {
                debug!("failed to retire rom terminal: {error}");
            }
        }

        Self {
            stream,
            terminal,
            render,
            history,
            dirty: false,
        }
    }

    fn drive(
        &mut self,
        chunks: &Receiver<io::Result<Chunk>>,
        stdout: &mut Vec<u8>,
    ) -> io::Result<()> {
        self.render(false)?;
        let mut last_frame = Instant::now();
        let frame_interval = self.terminal.frame_interval();
        let mut animating = false;

        loop {
            let wait = if self.dirty || animating {
                frame_interval.saturating_sub(last_frame.elapsed())
            } else {
                frame_interval
            };

            match chunks.recv_timeout(wait) {
                Ok(Ok(Chunk::Stdout(bytes))) => stdout.extend_from_slice(&bytes),
                Ok(Ok(Chunk::Stderr(bytes))) => self.push(&bytes)?,
                Ok(Err(error)) => {
                    return Err(io::Error::other(format!(
                        "failed to read Nix output: {error}"
                    )));
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.dirty |= self.stream.engine_mut().poll_lookups(current_time());
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }

            let since_frame = last_frame.elapsed();
            animating =
                !self.terminal.is_retired() && self.stream.engine().state().has_unfinished();
            if ((self.dirty || animating) && since_frame >= frame_interval)
                || since_frame >= TIMER_INTERVAL
            {
                self.render(false)?;
                last_frame = Instant::now();
            }
        }
    }

    fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
        let processed = self
            .stream
            .push_at(bytes, current_time())
            .map_err(rom_error)?;
        self.dirty |= processed.changed;
        self.output(processed.output)
    }

    fn output(&mut self, output: Vec<Output>) -> io::Result<()> {
        self.dirty |= !output.is_empty();
        for item in output {
            match item {
                Output::Passthrough(bytes) => self.terminal.write_passthrough(&bytes)?,
                Output::Log(line) => {
                    let line = format_log(&line, &self.render);
                    self.terminal.write_passthrough(line.as_bytes())?;
                    self.terminal.write_passthrough(b"\n")?;
                }
            }
        }
        Ok(())
    }

    fn render(&mut self, final_render: bool) -> io::Result<()> {
        self.terminal.render(
            self.stream.engine().state(),
            &self.render,
            current_time(),
            final_render,
        )?;
        self.dirty = false;
        Ok(())
    }

    fn finish(&mut self, succeeded: bool) -> io::Result<()> {
        let processed = self.stream.finish_at(current_time()).map_err(rom_error)?;
        self.output(processed.output)?;
        if succeeded {
            self.stream.engine_mut().mark_planned_available();
        }
        self.render(true)?;
        if self.terminal.is_retired() {
            self.terminal.append_final(
                self.stream.engine().state(),
                &self.render,
                current_time(),
            )?;
        }
        self.terminal.finish()?;

        if let Some(history) = &self.history {
            if let Err(error) = self.stream.engine().save_history(history) {
                debug!("failed to save rom build history: {error}");
            }
        }
        Ok(())
    }
}

fn rom_error(error: rom::RomError) -> io::Error {
    io::Error::other(format!("rom failed to monitor the Nix build: {error}"))
}
