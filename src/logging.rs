// SPDX-FileCopyrightText: 2026 deploy-rx contributors
//
// SPDX-License-Identifier: MPL-2.0

use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::{mpsc, Mutex, OnceLock};

use clap::ValueEnum;
use thiserror::Error;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Record};
use tracing::{Event, Id, Level, Subscriber};
use tracing_log::LogTracer;
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::fmt::{FmtContext, FormattedFields};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer};
use yansi::Paint;

use crate::remote_protocol::{RemoteEvent, RemoteLogLevel, RemoteLogSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum LogFormat {
    /// Pretty output on interactive terminals, compact plain text when piped
    #[default]
    Auto,
    /// Colored, human-friendly terminal output
    Pretty,
    /// Uncolored, compact single-header text output
    Compact,
    /// Newline-delimited JSON (JSONL)
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoggerType {
    Deploy,
    Activate,
    Revoke,
    RemoteSession,
}

impl LoggerType {
    pub fn default_source(self) -> &'static str {
        match self {
            LoggerType::Deploy => "deploy",
            LoggerType::Activate | LoggerType::RemoteSession => "activate",
            LoggerType::Revoke => "revoke",
        }
    }
}

#[derive(Error, Debug)]
pub enum InitLoggerError {
    #[error("invalid log filter: {0}")]
    Filter(#[from] tracing_subscriber::filter::ParseError),
    #[error("failed to bridge `log` crate: {0}")]
    LogBridge(#[from] tracing_log::log_tracer::SetLoggerError),
    #[error("failed to create log directory `{path}`: {source}")]
    CreateLogDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to open log file `{path}`: {source}")]
    OpenLogFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to install global tracing subscriber: {0}")]
    SetGlobalDefault(#[from] tracing::subscriber::SetGlobalDefaultError),
}

static STDOUT_EVENT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn stdout_lock() -> &'static Mutex<()> {
    STDOUT_EVENT_LOCK.get_or_init(|| Mutex::new(()))
}

/// Serializes a `RemoteEvent` as a single atomic JSON line on stdout.
pub fn emit_remote_event(event: &RemoteEvent) -> io::Result<()> {
    let _guard = stdout_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, event).map_err(io::Error::other)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

enum RemoteLogMessage {
    Output(Vec<u8>),
    Flush(mpsc::SyncSender<()>),
}

fn remote_stderr() -> &'static mpsc::SyncSender<RemoteLogMessage> {
    static OUTPUT: OnceLock<mpsc::SyncSender<RemoteLogMessage>> = OnceLock::new();
    OUTPUT.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel(64);
        std::thread::spawn(move || {
            let mut stderr = io::stderr();
            while let Ok(message) = receiver.recv() {
                match message {
                    RemoteLogMessage::Output(bytes) => {
                        if stderr.write_all(&bytes).is_err() {
                            break;
                        }
                    }
                    RemoteLogMessage::Flush(done) => {
                        let _ = stderr.flush();
                        let _ = done.try_send(());
                    }
                }
            }
        });
        sender
    })
}

/// Remote log backpressure must never delay activation or recovery.
pub fn write_remote_stderr(bytes: &[u8]) {
    for chunk in bytes.chunks(8192) {
        let _ = remote_stderr().try_send(RemoteLogMessage::Output(chunk.to_vec()));
    }
}

pub fn flush_remote_stderr() {
    let (sender, receiver) = mpsc::sync_channel(1);
    if remote_stderr()
        .try_send(RemoteLogMessage::Flush(sender))
        .is_ok()
    {
        let _ = receiver.recv_timeout(std::time::Duration::from_millis(100));
    }
}

fn dim_if(text: &str, use_ansi: bool) -> String {
    if use_ansi {
        text.dim().to_string()
    } else {
        text.to_string()
    }
}

/// Formats the prefix for raw stderr lines forwarded from a remote child process.
pub fn format_remote_raw_prefix(use_ansi: bool) -> String {
    dim_if("               │ ", use_ansi)
}

/// Formats the prefix for empty raw stderr lines forwarded from a remote child process.
pub fn format_remote_raw_empty_prefix(use_ansi: bool) -> String {
    dim_if("               │", use_ansi)
}

pub fn stderr_supports_ansi(log_format: LogFormat) -> bool {
    match log_format {
        LogFormat::Pretty => std::env::var_os("NO_COLOR").is_none(),
        LogFormat::Auto => io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        LogFormat::Compact | LogFormat::Json => false,
    }
}

#[derive(Debug, Clone, Default)]
struct ContextFields {
    source: Option<String>,
    phase: Option<String>,
    target: Option<String>,
    message: Option<String>,
    extras: Vec<(String, String)>,
}

impl Visit for ContextFields {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let rendered = format!("{value:?}");
        let trimmed = rendered
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(&rendered)
            .to_string();
        self.record_string_value(field.name(), trimmed);
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_string_value(field.name(), value.to_string());
    }
}

impl ContextFields {
    fn record_string_value(&mut self, name: &str, value: String) {
        match name {
            "source" => self.source = Some(value),
            "phase" => self.phase = Some(value),
            "target" => self.target = Some(value),
            "message" => self.message = Some(value),
            _ if name.starts_with("log.") => {}
            _ => self.extras.push((name.to_string(), value)),
        }
    }

    fn merge_from(&mut self, other: &ContextFields) {
        if other.source.is_some() {
            self.source = other.source.clone();
        }
        if other.phase.is_some() {
            self.phase = other.phase.clone();
        }
        if other.target.is_some() {
            self.target = other.target.clone();
        }
    }
}

/// Captures `source`, `phase`, and `target` fields on tracing spans so formatters
/// can render a unified `[phase | target]` context tag.
#[derive(Debug, Default)]
struct SpanContextLayer;

impl<S> Layer<S> for SpanContextLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut fields = ContextFields::default();
            attrs.record(&mut fields);
            span.extensions_mut().insert(fields);
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut extensions = span.extensions_mut();
            if let Some(fields) = extensions.get_mut::<ContextFields>() {
                values.record(fields);
            } else {
                let mut fields = ContextFields::default();
                values.record(&mut fields);
                extensions.insert(fields);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct CliEventFormatter {
    default_source: &'static str,
    ansi: bool,
    show_timestamp: bool,
}

impl CliEventFormatter {
    pub fn new(default_source: &'static str, ansi: bool) -> Self {
        Self {
            default_source,
            ansi,
            show_timestamp: true,
        }
    }

    fn format_context_tag(&self, context: &ContextFields) -> String {
        let source = context.source.as_deref().unwrap_or(self.default_source);
        match (context.phase.as_deref(), context.target.as_deref()) {
            (Some(phase), Some(target)) => format!("[{phase} | {target}]"),
            (Some(phase), None) => format!("[{phase}]"),
            (None, Some(target)) => format!("[{source} | {target}]"),
            (None, None) => format!("[{source}]"),
        }
    }

    fn format_level(&self, level: Level) -> String {
        let label = match level {
            Level::ERROR => "ERROR",
            Level::WARN => "WARN ",
            Level::INFO => "INFO ",
            Level::DEBUG => "DEBUG",
            Level::TRACE => "TRACE",
        };
        if !self.ansi {
            return label.to_string();
        }
        match level {
            Level::ERROR => label.red().bold().to_string(),
            Level::WARN => label.yellow().bold().to_string(),
            Level::INFO => label.cyan().bold().to_string(),
            Level::DEBUG => label.dim().to_string(),
            Level::TRACE => label.dim().to_string(),
        }
    }
}

fn format_hms_local() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: `now` is a valid `time_t` and `tm` is a valid stack pointer.
    let local = unsafe { libc::localtime_r(&now, tm.as_mut_ptr()) };
    if !local.is_null() {
        // SAFETY: `localtime_r` succeeded and initialized `tm`.
        let tm = unsafe { tm.assume_init() };
        return format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec);
    }
    let day_secs = (now as u64) % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        day_secs / 3_600,
        (day_secs % 3_600) / 60,
        day_secs % 60
    )
}

fn collect_context_fields<S>(
    scope: Option<tracing_subscriber::registry::Scope<'_, S>>,
    event: &Event<'_>,
) -> (ContextFields, ContextFields)
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let mut combined = ContextFields::default();
    if let Some(scope) = scope {
        for span in scope.from_root() {
            if let Some(span_fields) = span.extensions().get::<ContextFields>() {
                combined.merge_from(span_fields);
            }
        }
    }

    let mut event_fields = ContextFields::default();
    event.record(&mut event_fields);
    combined.merge_from(&event_fields);
    (combined, event_fields)
}

impl<S, N> FormatEvent<S, N> for CliEventFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let (combined, event_fields) = collect_context_fields(ctx.event_scope(), event);

        let timestamp = if self.show_timestamp {
            let ts = format_hms_local();
            if self.ansi {
                format!("{} ", ts.dim())
            } else {
                format!("{ts} ")
            }
        } else {
            String::new()
        };

        let level = self.format_level(*event.metadata().level());
        let tag = self.format_context_tag(&combined);
        let rendered_tag = if self.ansi {
            tag.bold().to_string()
        } else {
            tag
        };

        let mut message = event_fields.message.unwrap_or_default();
        if !event_fields.extras.is_empty() {
            let extras = event_fields
                .extras
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ");
            if message.is_empty() {
                message = extras;
            } else {
                message = format!("{message} ({extras})");
            }
        }

        let indent = if self.show_timestamp {
            "               "
        } else {
            "      "
        };

        let mut lines = message.split('\n');
        if let Some(first) = lines.next() {
            writeln!(writer, "{timestamp}{level} {rendered_tag} {first}")?;
        }
        for line in lines {
            if line.is_empty() {
                writeln!(writer)?;
            } else {
                writeln!(writer, "{indent}{line}")?;
            }
        }

        // Keep `FormattedFields` type parameter referenced for trait bounds.
        let _ = std::marker::PhantomData::<FormattedFields<N>>;
        Ok(())
    }
}

struct RemoteEventLogLayer {
    default_source: RemoteLogSource,
}

impl<S> Layer<S> for RemoteEventLogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let (combined, event_fields) = collect_context_fields(ctx.event_scope(event), event);

        let level = match *event.metadata().level() {
            Level::ERROR => RemoteLogLevel::Error,
            Level::WARN => RemoteLogLevel::Warn,
            Level::INFO => RemoteLogLevel::Info,
            Level::DEBUG => RemoteLogLevel::Debug,
            Level::TRACE => RemoteLogLevel::Trace,
        };
        let source = match combined.source.as_deref() {
            Some("revoke") => RemoteLogSource::Revoke,
            Some("activate") => RemoteLogSource::Activate,
            _ => self.default_source,
        };
        let message = event_fields.message.unwrap_or_default();

        // Remote log backpressure or closed stdout must never abort activation or rollback.
        let _ = emit_remote_event(&RemoteEvent::Log {
            level,
            source,
            phase: combined.phase,
            message,
        });
    }
}

struct NonBlockingFileWriter {
    file: Mutex<std::fs::File>,
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for NonBlockingFileWriter {
    type Writer = FileGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        FileGuard { file: &self.file }
    }
}

struct FileGuard<'a> {
    file: &'a Mutex<std::fs::File>,
}

impl io::Write for FileGuard<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.flush();
        }
        Ok(())
    }
}

struct BufferedRemoteStderrWriter;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufferedRemoteStderrWriter {
    type Writer = RemoteStderrLineWriter;

    fn make_writer(&'a self) -> Self::Writer {
        RemoteStderrLineWriter { buf: Vec::new() }
    }
}

struct RemoteStderrLineWriter {
    buf: Vec<u8>,
}

impl io::Write for RemoteStderrLineWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            write_remote_stderr(&self.buf);
            Vec::clear(&mut self.buf);
        }
        Ok(())
    }
}

impl Drop for RemoteStderrLineWriter {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

pub fn init_logger(
    debug_logs: bool,
    log_dir: Option<&str>,
    logger_type: &LoggerType,
    log_format: LogFormat,
) -> Result<(), InitLoggerError> {
    let _ = LogTracer::init();

    let default_directive = if debug_logs { "debug" } else { "info" };
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_directive));

    let file_layer = if let Some(dir) = log_dir {
        let dir_path = PathBuf::from(dir);
        std::fs::create_dir_all(&dir_path).map_err(|source| InitLoggerError::CreateLogDir {
            path: dir_path.clone(),
            source,
        })?;
        let file_path = dir_path.join(format!("{}.jsonl", logger_type.default_source()));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
            .map_err(|source| InitLoggerError::OpenLogFile {
                path: file_path,
                source,
            })?;
        let writer = NonBlockingFileWriter {
            file: Mutex::new(file),
        };
        Some(
            tracing_subscriber::fmt::layer()
                .json()
                .with_timer(UtcTime::rfc_3339())
                .with_writer(writer),
        )
    } else {
        None
    };

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(SpanContextLayer)
        .with(file_layer);

    match logger_type {
        LoggerType::RemoteSession => {
            let remote_layer = RemoteEventLogLayer {
                default_source: RemoteLogSource::Activate,
            };
            tracing::subscriber::set_global_default(registry.with(remote_layer))?;
        }
        LoggerType::Activate | LoggerType::Revoke => {
            let ansi = stderr_supports_ansi(log_format);
            let formatter = CliEventFormatter::new(logger_type.default_source(), ansi);
            let layer = tracing_subscriber::fmt::layer()
                .event_format(formatter)
                .with_writer(BufferedRemoteStderrWriter);
            tracing::subscriber::set_global_default(registry.with(layer))?;
        }
        LoggerType::Deploy => match log_format {
            LogFormat::Json => {
                let layer = tracing_subscriber::fmt::layer()
                    .json()
                    .with_timer(UtcTime::rfc_3339())
                    .with_writer(io::stderr);
                tracing::subscriber::set_global_default(registry.with(layer))?;
            }
            LogFormat::Pretty | LogFormat::Compact | LogFormat::Auto => {
                let ansi = stderr_supports_ansi(log_format);
                let formatter = CliEventFormatter::new(logger_type.default_source(), ansi);
                let layer = tracing_subscriber::fmt::layer()
                    .event_format(formatter)
                    .with_writer(io::stderr);
                tracing::subscriber::set_global_default(registry.with(layer))?;
            }
        },
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuffer {
        type Writer = SharedBufferGuard;

        fn make_writer(&'a self) -> Self::Writer {
            SharedBufferGuard(self.0.clone())
        }
    }

    struct SharedBufferGuard(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedBufferGuard {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn compact_formatter_renders_span_context_and_indents_multiline() {
        let buffer = SharedBuffer::default();
        let mut formatter = CliEventFormatter::new("deploy", false);
        formatter.show_timestamp = false;

        let subscriber = tracing_subscriber::registry().with(SpanContextLayer).with(
            tracing_subscriber::fmt::layer()
                .event_format(formatter)
                .with_writer(buffer.clone()),
        );

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("phase", phase = "activate", target = "web-1.system");
            let _guard = span.enter();
            tracing::error!("failure:\nfirst detail\nsecond detail");
        });

        assert_eq!(
            buffer.contents(),
            "ERROR [activate | web-1.system] failure:\n      first detail\n      second detail\n"
        );
    }

    #[test]
    fn format_remote_raw_prefix_uses_pipe_without_double_brackets() {
        assert_eq!(format_remote_raw_prefix(false), "               │ ");
        assert_eq!(format_remote_raw_empty_prefix(false), "               │");
    }

    #[test]
    fn format_hms_local_returns_hh_mm_ss() {
        let formatted = format_hms_local();
        let parts: Vec<&str> = formatted.split(':').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts
            .iter()
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_digit())));
    }

    #[test]
    fn json_layer_emits_valid_json_lines_with_span_fields() {
        let buffer = SharedBuffer::default();
        let subscriber = tracing_subscriber::registry().with(SpanContextLayer).with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_timer(UtcTime::rfc_3339())
                .with_writer(buffer.clone()),
        );

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("phase", phase = "push", target = "db-1.system");
            let _guard = span.enter();
            tracing::info!("Pushing closure");
        });

        let raw = buffer.contents();
        let value: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(value["level"], "INFO");
        assert_eq!(value["fields"]["message"], "Pushing closure");
        assert_eq!(value["span"]["phase"], "push");
        assert_eq!(value["span"]["target"], "db-1.system");
        assert!(value["timestamp"].as_str().is_some());
    }
}
