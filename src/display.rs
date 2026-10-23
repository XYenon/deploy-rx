// SPDX-FileCopyrightText: 2026 deploy-rx contributors
//
// SPDX-License-Identifier: MPL-2.0

//! One terminal owner; each nom keeps its own derivation state and terminal.
//! Never forward child terminal controls to the user's terminal.

use std::fs::File;
use std::future::Future;
use std::io::{self, IsTerminal, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{
    unix::AsyncFd, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader,
};

static TERMINAL: Mutex<Option<Display>> = Mutex::new(None);

struct Display {
    users: usize,
    started: std::time::Instant,
    interactive: bool,
    drawn: usize,
    panels: Vec<Panel>,
    connections: Vec<(String, String)>,
}

struct Panel {
    label: String,
    state: String,
    screen: vt100::Parser,
    terminal: Option<Arc<AsyncFd<File>>>,
}

fn dimensions() -> (u16, u16) {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut size) } == 0 {
        (size.ws_row.max(8), size.ws_col.max(20))
    } else {
        (30, 100)
    }
}

impl Display {
    fn clear(&mut self, out: &mut impl Write) -> io::Result<()> {
        if self.drawn > 0 {
            write!(out, "\r\x1b[{}A\x1b[J", self.drawn)?;
            self.drawn = 0;
        }
        Ok(())
    }

    fn render(&mut self, final_frame: bool) -> io::Result<()> {
        let mut out = io::stderr().lock();
        if !self.interactive && !final_frame {
            return Ok(());
        }
        self.clear(&mut out)?;
        let (height, width) = dimensions();
        let cols = width.saturating_sub(1);
        let running: Vec<_> = self
            .panels
            .iter()
            .enumerate()
            .filter_map(|(id, p)| (p.state == "running").then_some(id))
            .collect();
        let capacity = ((height as usize).saturating_sub(4) / 9).max(1);
        let page_count = running.len().div_ceil(capacity).max(1);
        let page = (self.started.elapsed().as_secs() as usize / 3) % page_count;
        let mut visible: Vec<_> = running
            .into_iter()
            .skip(page * capacity)
            .take(capacity)
            .collect();
        for (id, panel) in self.panels.iter().enumerate().rev() {
            if visible.len() >= capacity {
                break;
            }
            if panel.state != "running" {
                visible.push(id);
            }
        }
        visible.sort_unstable();
        let live_trees = visible
            .iter()
            .filter(|&&id| self.panels[id].state == "running" && self.panels[id].terminal.is_some())
            .count();
        let headers = visible.len() + 1;
        let rows =
            ((height as usize).saturating_sub(headers + 2) / live_trees.max(1)).max(2) as u16;
        if self.interactive && !final_frame {
            writeln!(
                out,
                "Preparation: {} tasks | page {}/{}",
                self.panels.len(),
                page + 1,
                page_count
            )?;
            self.drawn += 1;
        }
        for (index, panel) in self.panels.iter_mut().enumerate() {
            if !final_frame && !visible.contains(&index) {
                continue;
            }
            if let Some(terminal) = &panel.terminal {
                let size = libc::winsize {
                    ws_row: rows.max(8),
                    ws_col: cols,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                unsafe {
                    libc::ioctl(terminal.get_ref().as_raw_fd(), libc::TIOCSWINSZ, &size);
                }
                panel.screen.screen_mut().set_size(size.ws_row, cols);
            }
            let title = format!("[{}] {}", panel.state, panel.label);
            // Render text cells only. ANSI/OSC from builders cannot move the
            // compositor's cursor, change its title, or inject terminal modes.
            let mut title_screen = vt100::Parser::new(1, cols, 0);
            for character in title.chars() {
                if title_screen.screen().cursor_position().1 >= cols - 2 {
                    break;
                }
                title_screen.process(character.encode_utf8(&mut [0; 4]).as_bytes());
            }
            if self.interactive {
                let color = if panel.state.contains("FAILED") {
                    31
                } else if panel.state == "running" {
                    36
                } else {
                    32
                };
                write!(out, "\x1b[1;{color}m")?;
            }
            writeln!(
                out,
                "{}{}",
                title_screen.screen().contents(),
                if self.interactive { "\x1b[0m" } else { "" }
            )?;
            if self.interactive {
                self.drawn += 1;
            }
            if !final_frame && panel.state != "running" {
                continue;
            }
            let content = panel.screen.screen().contents();
            let lines: Vec<_> = content.lines().collect();
            for line in lines.iter().skip(lines.len().saturating_sub(rows as usize)) {
                writeln!(out, "{line}")?;
            }
            if self.interactive {
                self.drawn += lines.len().min(rows as usize);
            }
        }
        out.flush()
    }
}

/// Scope the renderer to preparation. No detached task survives the future.
pub(crate) async fn run<T>(enabled: bool, future: impl Future<Output = T>) -> T {
    if !enabled {
        return future.await;
    }
    let owns_display = {
        let mut terminal = TERMINAL.lock().unwrap();
        match terminal.as_mut() {
            Some(display) => {
                display.users += 1;
                false
            }
            None => {
                *terminal = Some(Display {
                    users: 1,
                    started: std::time::Instant::now(),
                    interactive: io::stderr().is_terminal(),
                    drawn: 0,
                    panels: Vec::new(),
                    connections: Vec::new(),
                });
                true
            }
        }
    };
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let mut terminal = TERMINAL.lock().unwrap();
            if let Some(display) = terminal.as_mut() {
                display.users -= 1;
                if display.users != 0 {
                    return;
                }
            }
            if let Some(mut display) = terminal.take() {
                for panel in &mut display.panels {
                    if panel.state == "running" {
                        panel.state = "interrupted".into();
                    }
                }
                let _ = display.render(true);
            }
        }
    }
    let _restore = Restore;
    if !owns_display {
        return future.await;
    }
    tokio::pin!(future);
    let mut clock = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        tokio::select! {
            result = &mut future => return result,
            _ = clock.tick() => {
                if let Some(display) = TERMINAL.lock().unwrap().as_mut() {
                    let _ = display.render(false);
                }
            }
        }
    }
}

pub(crate) fn active() -> bool {
    TERMINAL.lock().unwrap().is_some()
}

/// Log records and non-build commands share the same output owner as nom.
pub(crate) fn log(bytes: &[u8]) -> io::Result<()> {
    let mut terminal = TERMINAL.lock().unwrap();
    let mut out = io::stderr().lock();
    if let Some(display) = terminal.as_mut() {
        display.clear(&mut out)?;
    }
    out.write_all(bytes)?;
    out.flush()
}

pub(crate) fn command_label(command: &tokio::process::Command, operation: &str) -> String {
    let args: Vec<_> = command.as_std().get_args().collect();
    let address = args.windows(2).find_map(|pair| {
        (pair[0] == "--store" || pair[0] == "--to").then(|| pair[1].to_string_lossy().into_owned())
    });
    let Some(address) = address else {
        return format!("{operation} local batch");
    };
    let options = command
        .as_std()
        .get_envs()
        .find(|(key, _)| *key == "NIX_SSHOPTS")
        .and_then(|(_, value)| value)
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let identity = (
        address
            .split_once("://")
            .map_or(address.as_str(), |(_, host)| host)
            .to_owned(),
        options,
    );
    let mut terminal = TERMINAL.lock().unwrap();
    let number = terminal.as_mut().map(|display| {
        if let Some(index) = display
            .connections
            .iter()
            .position(|item| *item == identity)
        {
            index + 1
        } else {
            display.connections.push(identity);
            display.connections.len()
        }
    });
    format!("{operation} connection {} {address}", number.unwrap_or(1))
}

pub(crate) fn panel(label: String) -> usize {
    let mut terminal = TERMINAL.lock().unwrap();
    let display = terminal.as_mut().expect("display scope");
    let id = display.panels.len();
    let (rows, cols) = dimensions();
    display.panels.push(Panel {
        label: label.clone(),
        state: "running".into(),
        screen: vt100::Parser::new(rows, cols.saturating_sub(1), 0),
        terminal: None,
    });
    if !display.interactive {
        let _ = writeln!(io::stderr().lock(), "[running] {label}");
    }
    id
}

pub(crate) fn finish(id: usize, result: &str) {
    if let Some(display) = TERMINAL.lock().unwrap().as_mut() {
        display.panels[id].state = result.into();
        // The reader has settled; retain the screen, not the PTY descriptor.
        display.panels[id].terminal = None;
        let mut out = io::stderr().lock();
        let _ = display.clear(&mut out);
        let _ = writeln!(out, "[{result}] {}", display.panels[id].label);
    }
}

/// PTY provides dimensions to nom; stdin remains its isolated JSON pipe.
pub(crate) fn nom_terminal(id: usize) -> io::Result<(Arc<AsyncFd<File>>, Stdio)> {
    let (rows, cols) = dimensions();
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols.saturating_sub(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (mut master, mut slave) = (-1, -1);
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    for file in [&master, &slave] {
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    if unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // File owns this descriptor for the complete registration lifetime.
    let master =
        Arc::new(unsafe { AsyncFd::register(master) }.map_err(|error| error.into_parts().1)?);
    TERMINAL.lock().unwrap().as_mut().unwrap().panels[id].terminal = Some(master.clone());
    Ok((master, slave.into()))
}

pub(crate) async fn read_nom(id: usize, terminal: Arc<AsyncFd<File>>) -> io::Result<()> {
    let mut buffer = [0; 8192];
    loop {
        let mut ready = terminal.readable().await?;
        match ready.try_io(|terminal| {
            let count = unsafe {
                libc::read(
                    terminal.get_ref().as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if count < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(count as usize)
            }
        }) {
            Ok(Ok(0)) => return Ok(()),
            // Linux PTY EOF is EIO after the last slave closes.
            Ok(Err(error)) if error.raw_os_error() == Some(libc::EIO) => return Ok(()),
            Ok(Err(error)) => return Err(error),
            Ok(Ok(count)) => {
                if let Some(display) = TERMINAL.lock().unwrap().as_mut() {
                    display.panels[id].screen.process(&buffer[..count]);
                }
            }
            Err(_) => (),
        }
    }
}

pub(crate) async fn stream(label: &str, mut input: impl AsyncRead + Unpin) -> io::Result<()> {
    let mut buffer = [0; 8192];
    let mut parser = vt100::Parser::new(1, 4096, 0);
    loop {
        let count = input.read(&mut buffer).await?;
        if count == 0 {
            let tail = parser.screen().contents();
            if !tail.is_empty() {
                let _ = log(format!("[{label}] {tail}\n").as_bytes());
            }
            return Ok(());
        }
        // A virtual terminal also removes arbitrary controls in ordinary logs.
        for byte in &buffer[..count] {
            if parser.screen().cursor_position().1 >= 4094 {
                let _ = log(format!("[{label}] {}\n", parser.screen().contents()).as_bytes());
                parser = vt100::Parser::new(1, 4096, 0);
            }
            if *byte == b'\n' {
                let _ = log(format!("[{label}] {}\n", parser.screen().contents()).as_bytes());
                parser = vt100::Parser::new(1, 4096, 0);
            } else {
                parser.process(&[*byte]);
            }
        }
    }
}

pub(crate) async fn feed_nom(
    label: &str,
    input: impl AsyncRead + Unpin,
    output: tokio::process::ChildStdin,
) -> io::Result<()> {
    let mut input = BufReader::new(input);
    let mut output = Some(output);
    let mut error = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line).await? == 0 {
            break;
        }
        // Preserve full ordinary/build logs above the mutable panels. The
        // virtual nom viewport can scroll freely without losing diagnostics.
        let message = if let Some(json) = line.strip_prefix(b"@nix ") {
            serde_json::from_slice::<serde_json::Value>(json)
                .ok()
                .and_then(|value| match value["action"].as_str() {
                    Some("msg") => value["msg"].as_str().map(str::to_owned),
                    Some("result") if value["type"] == 101 => {
                        value["fields"][0].as_str().map(str::to_owned)
                    }
                    _ => None,
                })
        } else {
            Some(String::from_utf8_lossy(&line).trim_end().to_owned())
        };
        if let Some(message) = message {
            stream(label, format!("{message}\n").as_bytes()).await?;
        }
        // nom's passthrough lines have already been committed above. Sending
        // them twice would fill every panel with duplicate ordinary output.
        if !line.starts_with(b"@nix ") {
            continue;
        }
        if let Some(writer) = &mut output {
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(1), writer.write_all(&line))
                    .await
                    .unwrap_or_else(|_| {
                        Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "nom stopped reading its input",
                        ))
                    });
            if let Err(failure) = result {
                error = Some(failure);
                output = None;
            }
        }
    }
    error.map_or(Ok(()), Err)
}
