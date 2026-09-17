//! Logging infrastructure for jcode
//!
//! Logs to ~/.jcode/logs/ with automatic rotation
//!
//! Supports thread-local context for server, session, provider, and model info.

pub mod watchdog;

mod redaction;

use self::redaction::{redact_field_value, sanitize_log_value, sanitize_payload_for_log};

use chrono::{DateTime, Local, SecondsFormat};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const LOG_SCHEMA_VERSION: u8 = 1;
const DEFAULT_LOG_MAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ROTATION_INDEX: u32 = 10_000;
const MAX_EVENT_FIELDS: usize = 64;
const LOG_RETENTION_DAYS: i64 = 7;

static LOGGER: Mutex<Option<Logger>> = Mutex::new(None);
static CLEANUP_STARTED: AtomicBool = AtomicBool::new(false);
static TASK_LOG_CONTEXTS: OnceLock<Mutex<HashMap<String, LogContext>>> = OnceLock::new();
static RATE_LIMITS: OnceLock<Mutex<HashMap<String, RateLimitState>>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
    Debug,
}

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
            Self::Debug => "DEBUG",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Debug => 0,
            Self::Info => 1,
            Self::Warn => 2,
            Self::Error => 3,
        }
    }

    fn is_enabled(self) -> bool {
        minimum_log_level().is_some_and(|minimum| self.rank() >= minimum.rank())
    }
}

fn minimum_log_level() -> Option<LogLevel> {
    let trace_enabled = std::env::var_os("JCODE_TRACE").is_some();
    let configured = std::env::var("JCODE_LOG_LEVEL").ok();
    minimum_log_level_from_env(trace_enabled, configured.as_deref())
}

fn minimum_log_level_from_env(trace_enabled: bool, configured: Option<&str>) -> Option<LogLevel> {
    if trace_enabled {
        return Some(LogLevel::Debug);
    }

    match configured.map(|value| value.trim().to_ascii_lowercase()) {
        Some(value) if matches!(value.as_str(), "off" | "none" | "silent") => None,
        Some(value) => match value.as_str() {
            "debug" | "trace" => Some(LogLevel::Debug),
            "warn" | "warning" => Some(LogLevel::Warn),
            "error" => Some(LogLevel::Error),
            "info" | "" => Some(LogLevel::Info),
            _ => Some(LogLevel::Info),
        },
        None => Some(LogLevel::Info),
    }
}

#[derive(Clone, Copy, Debug)]
struct RateLimitState {
    last_emit: Instant,
    suppressed: u64,
}

/// Thread-local logging context
#[derive(Default, Clone)]
pub struct LogContext {
    pub server: Option<String>,
    pub session: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
}

thread_local! {
    static LOG_CONTEXT: RefCell<LogContext> = RefCell::new(LogContext::default());
}

/// Update just the session in the current context
pub fn set_session(session: &str) {
    if with_task_context_mut(|ctx| {
        ctx.session = Some(session.to_string());
    }) {
        return;
    }

    LOG_CONTEXT.with(|c| {
        c.borrow_mut().session = Some(session.to_string());
    });
}

/// Update just the server in the current context
pub fn set_server(server: &str) {
    if with_task_context_mut(|ctx| {
        ctx.server = Some(server.to_string());
    }) {
        return;
    }

    LOG_CONTEXT.with(|c| {
        c.borrow_mut().server = Some(server.to_string());
    });
}

/// Update provider and model in the current context
pub fn set_provider_info(provider: &str, model: &str) {
    if with_task_context_mut(|ctx| {
        ctx.provider = Some(provider.to_string());
        ctx.model = Some(model.to_string());
    }) {
        return;
    }

    LOG_CONTEXT.with(|c| {
        let mut ctx = c.borrow_mut();
        ctx.provider = Some(provider.to_string());
        ctx.model = Some(model.to_string());
    });
}

fn current_task_id() -> Option<String> {
    tokio::task::try_id().map(|id| id.to_string())
}

fn with_task_context_mut(update: impl FnOnce(&mut LogContext)) -> bool {
    let Some(task_id) = current_task_id() else {
        return false;
    };

    let store = TASK_LOG_CONTEXTS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut contexts) = store.lock() {
        let ctx = contexts.entry(task_id).or_default();
        update(ctx);
        true
    } else {
        false
    }
}

fn task_context_snapshot() -> Option<LogContext> {
    let task_id = current_task_id()?;
    let store = TASK_LOG_CONTEXTS.get()?;
    let contexts = store.lock().ok()?;
    contexts.get(&task_id).cloned()
}

/// Snapshot the current logging context for diagnostics that need stable,
/// session-scoped in-memory keys in addition to the rendered log prefix.
pub fn current_context_snapshot() -> LogContext {
    task_context_snapshot().unwrap_or_else(|| LOG_CONTEXT.with(|c| c.borrow().clone()))
}

pub struct Logger {
    file: Option<File>,
    path: PathBuf,
    bytes_written: u64,
    max_bytes: u64,
}

fn log_dir() -> Option<PathBuf> {
    jcode_storage::logs_dir().ok()
}

fn configured_log_max_bytes() -> u64 {
    std::env::var("JCODE_LOG_MAX_BYTES")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_LOG_MAX_BYTES)
}

fn daily_log_path(log_dir: &Path, now: DateTime<Local>) -> PathBuf {
    let date = now.format("%Y-%m-%d");
    log_dir.join(format!("jcode-{}.log", date))
}

fn rotated_log_path(path: &Path, index: u32) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("jcode.log");
    let stem = file_name.strip_suffix(".log").unwrap_or(file_name);
    path.with_file_name(format!("{stem}.{index}.log"))
}

fn rotate_file_without_overwrite(source: &Path) -> io::Result<()> {
    for index in 1..=MAX_ROTATION_INDEX {
        let rotated = rotated_log_path(source, index);
        match fs::hard_link(source, &rotated) {
            Ok(()) => {
                fs::remove_file(source)?;
                return Ok(());
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "log rotation index limit reached",
    ))
}

fn is_jsonl_log(path: &Path) -> io::Result<bool> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(true);
        }
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim_end()) else {
            return Ok(false);
        };
        return Ok(value.is_object() && value.get("schema_version").is_some());
    }
}

impl Logger {
    fn new() -> Option<Self> {
        let log_dir = log_dir()?;
        jcode_storage::ensure_dir(&log_dir).ok()?;

        Self::open(
            daily_log_path(&log_dir, Local::now()),
            configured_log_max_bytes(),
        )
        .ok()
    }

    fn open(path: PathBuf, max_bytes: u64) -> io::Result<Self> {
        let has_legacy_content = fs::metadata(&path)
            .map(|metadata| metadata.is_file() && metadata.len() > 0)
            .unwrap_or(false)
            && !is_jsonl_log(&path).unwrap_or(true);
        if has_legacy_content {
            rotate_file_without_overwrite(&path)?;
        }

        let (file, bytes_written) = Self::open_file(&path)?;
        Ok(Self {
            file: Some(file),
            path,
            bytes_written,
            max_bytes: max_bytes.max(1),
        })
    }

    fn open_file(path: &Path) -> io::Result<(File, u64)> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let bytes_written = file.metadata()?.len();
        Ok((file, bytes_written))
    }

    fn reopen(&mut self) -> io::Result<()> {
        let (file, bytes_written) = Self::open_file(&self.path)?;
        self.file = Some(file);
        self.bytes_written = bytes_written;
        Ok(())
    }

    fn ensure_daily_path(&mut self, now: DateTime<Local>) -> io::Result<()> {
        let Some(log_dir) = self.path.parent().map(Path::to_path_buf) else {
            return Ok(());
        };
        let expected = daily_log_path(&log_dir, now);
        if expected == self.path {
            if self.file.is_none() {
                self.reopen()?;
            }
            return Ok(());
        }

        if let Some(file) = self.file.as_mut() {
            file.flush()?;
        }
        let (file, bytes_written) = Self::open_file(&expected)?;
        self.file = Some(file);
        self.path = expected;
        self.bytes_written = bytes_written;
        Ok(())
    }

    fn rotate_size(&mut self) -> io::Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.flush()?;
        }
        self.file = None;

        let source = self.path.clone();
        let rotation = rotate_file_without_overwrite(&source);

        let reopen = self.reopen();
        match rotation {
            Err(err) => Err(err),
            Ok(()) => reopen,
        }
    }

    fn append_line(&mut self, line: &str, now: DateTime<Local>) -> io::Result<()> {
        self.ensure_daily_path(now)?;
        let line_len = line.len() as u64;
        if self.bytes_written > 0 && self.bytes_written.saturating_add(line_len) > self.max_bytes {
            self.rotate_size()?;
        }

        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("logger file is unavailable"))?;
        file.write_all(line.as_bytes())?;
        file.flush()?;
        self.bytes_written = self.bytes_written.saturating_add(line_len);
        Ok(())
    }

    fn write_record(&mut self, record: &str, now: DateTime<Local>) {
        let line = format!("{record}\n");
        if let Err(err) = self.append_line(&line, now) {
            eprintln!(
                "jcode logger write failed: {}",
                sanitize_log_value(&err.to_string())
            );
        }
    }
}

/// Initialize the logger (call once at startup)
pub fn init() {
    let mut guard = match LOGGER.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.is_none() {
        *guard = Logger::new();
    }
    drop(guard);

    // Prune stale daily log files once per process, off the startup path so
    // disk I/O never blocks launch. cleanup_old_logs is scoped to our own
    // `jcode-*.log` files, so it is safe to run unconditionally.
    // Detect silent hangs in long sessions: without this a freeze leaves the
    // log simply stopping, with no phase or thread-state evidence.
    watchdog::start();

    if !CLEANUP_STARTED.swap(true, Ordering::SeqCst) {
        std::thread::Builder::new()
            .name("jcode-log-cleanup".to_string())
            .spawn(cleanup_old_logs)
            .ok();
    }
}

/// Log an info message.
pub fn info(message: &str) {
    write_message(LogLevel::Info, message);
}

/// Log an error message.
pub fn error(message: &str) {
    write_message(LogLevel::Error, message);
}

/// Log a warning message.
pub fn warn(message: &str) {
    write_message(LogLevel::Warn, message);
}

/// Truncate a value for inclusion in a log line, appending an ellipsis marker
/// with the original length when cut. Char-boundary safe. Use this for
/// payload-bearing values (event debug dumps, request bodies) so a routine
/// warning cannot flood the log with a multi-kilobyte line.
pub fn truncate_for_log(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let truncated: String = value.chars().take(max_chars).collect();
    format!("{}… [{} chars total]", truncated, value.chars().count())
}

/// Log a debug message when the configured threshold allows it.
pub fn debug(message: &str) {
    write_message(LogLevel::Debug, message);
}

pub fn event_info<K, V, I>(event_name: &str, fields: I)
where
    K: AsRef<str>,
    V: ToString,
    I: IntoIterator<Item = (K, V)>,
{
    event(LogLevel::Info, event_name, fields);
}

pub fn event_warn<K, V, I>(event_name: &str, fields: I)
where
    K: AsRef<str>,
    V: ToString,
    I: IntoIterator<Item = (K, V)>,
{
    event(LogLevel::Warn, event_name, fields);
}

pub fn event_error<K, V, I>(event_name: &str, fields: I)
where
    K: AsRef<str>,
    V: ToString,
    I: IntoIterator<Item = (K, V)>,
{
    event(LogLevel::Error, event_name, fields);
}

pub fn event_debug<K, V, I>(event_name: &str, fields: I)
where
    K: AsRef<str>,
    V: ToString,
    I: IntoIterator<Item = (K, V)>,
{
    event(LogLevel::Debug, event_name, fields);
}

pub fn event<K, V, I>(level: LogLevel, event: &str, fields: I)
where
    K: AsRef<str>,
    V: ToString,
    I: IntoIterator<Item = (K, V)>,
{
    if !level.is_enabled() {
        return;
    }
    write_event(level, event, fields);
}

pub fn event_rate_limited<K, V, I>(
    level: LogLevel,
    rate_key: &str,
    min_interval: Duration,
    event: &str,
    fields: I,
) where
    K: AsRef<str>,
    V: ToString,
    I: IntoIterator<Item = (K, V)>,
{
    if !level.is_enabled() {
        return;
    }

    let now = Instant::now();
    let store = RATE_LIMITS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut suppressed = 0;
    let should_emit = match store.lock() {
        Ok(mut guard) => match guard.get_mut(rate_key) {
            Some(state) if now.duration_since(state.last_emit) < min_interval => {
                state.suppressed = state.suppressed.saturating_add(1);
                false
            }
            Some(state) => {
                suppressed = state.suppressed;
                state.suppressed = 0;
                state.last_emit = now;
                true
            }
            None => {
                guard.insert(
                    rate_key.to_string(),
                    RateLimitState {
                        last_emit: now,
                        suppressed: 0,
                    },
                );
                true
            }
        },
        Err(_) => true,
    };

    if !should_emit {
        return;
    }

    let mut fields: Vec<(String, String)> = fields
        .into_iter()
        .map(|(key, value)| (key.as_ref().to_string(), value.to_string()))
        .collect();
    if suppressed > 0 {
        fields.push(("suppressed".to_string(), suppressed.to_string()));
    }
    write_event(level, event, fields);
}

fn write_message(level: LogLevel, message: &str) {
    if !level.is_enabled() {
        return;
    }
    let now = Local::now();
    let context = current_context_snapshot();
    let record = serialize_record(
        now,
        level,
        None,
        Some(message),
        &context,
        std::iter::empty::<(&str, &str)>(),
    );
    write_serialized_record(record, now);
}

fn write_event<K, V, I>(level: LogLevel, event: &str, fields: I)
where
    K: AsRef<str>,
    V: ToString,
    I: IntoIterator<Item = (K, V)>,
{
    if !level.is_enabled() {
        return;
    }
    let now = Local::now();
    let context = current_context_snapshot();
    let record = serialize_record(now, level, Some(event), Some(event), &context, fields);
    write_serialized_record(record, now);
}

fn write_serialized_record(record: Result<String, serde_json::Error>, now: DateTime<Local>) {
    let Ok(record) = record else {
        eprintln!("jcode logger serialization failed");
        return;
    };

    if let Ok(mut guard) = LOGGER.lock()
        && let Some(logger) = guard.as_mut()
    {
        logger.write_record(&record, now);
    }
}

fn serialize_record<K, V, I>(
    timestamp: DateTime<Local>,
    level: LogLevel,
    event: Option<&str>,
    message: Option<&str>,
    context: &LogContext,
    fields: I,
) -> Result<String, serde_json::Error>
where
    K: AsRef<str>,
    V: ToString,
    I: IntoIterator<Item = (K, V)>,
{
    let mut object = serde_json::Map::new();
    object.insert(
        "schema_version".to_string(),
        serde_json::Value::from(LOG_SCHEMA_VERSION),
    );
    object.insert(
        "timestamp".to_string(),
        serde_json::Value::String(timestamp.to_rfc3339_opts(SecondsFormat::Millis, true)),
    );
    object.insert(
        "timestamp_ms".to_string(),
        serde_json::Value::from(timestamp.timestamp_millis()),
    );
    object.insert(
        "level".to_string(),
        serde_json::Value::String(level.as_str().to_string()),
    );
    object.insert(
        "pid".to_string(),
        serde_json::Value::from(std::process::id()),
    );

    for (key, value) in [
        ("server", context.server.as_deref()),
        ("session", context.session.as_deref()),
        ("provider", context.provider.as_deref()),
        ("model", context.model.as_deref()),
    ] {
        if let Some(value) = value {
            object.insert(
                key.to_string(),
                serde_json::Value::String(sanitize_log_value(value)),
            );
        }
    }

    if let Some(event) = event {
        object.insert(
            "event".to_string(),
            serde_json::Value::String(sanitize_log_value(event)),
        );
    }
    if let Some(message) = message {
        object.insert(
            "message".to_string(),
            serde_json::Value::String(sanitize_log_value(message)),
        );
    }

    let mut fields_truncated = false;
    for (index, (raw_key, raw_value)) in fields.into_iter().enumerate() {
        if index >= MAX_EVENT_FIELDS {
            fields_truncated = true;
            break;
        }
        let base_key = safe_record_field_key(&sanitize_log_key(raw_key.as_ref()));
        let mut key = base_key;
        while object.contains_key(&key) {
            key = format!("field.{key}");
        }
        let value = redact_field_value(raw_key.as_ref(), &raw_value.to_string());
        object.insert(key, serde_json::Value::String(value));
    }

    if fields_truncated {
        object.insert(
            "fields_truncated".to_string(),
            serde_json::Value::Bool(true),
        );
    }

    serde_json::to_string(&serde_json::Value::Object(object))
}

fn safe_record_field_key(key: &str) -> String {
    if matches!(
        key,
        "schema_version"
            | "timestamp"
            | "timestamp_ms"
            | "level"
            | "pid"
            | "server"
            | "session"
            | "provider"
            | "model"
            | "event"
            | "message"
            | "fields_truncated"
    ) {
        format!("field.{key}")
    } else {
        key.to_string()
    }
}

fn sanitize_log_key(key: &str) -> String {
    let sanitized: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "field".to_string()
    } else {
        truncate(&sanitized, 80)
    }
}

/// Log a structured auth event with conservative redaction.
///
/// Callers should pass only non-secret metadata. This function still redacts any
/// field whose key looks credential-like so accidental tokens/keys do not land in
/// logs.
pub fn auth_event(event: &str, provider: &str, fields: &[(&str, &str)]) {
    let mut record_fields = Vec::with_capacity(fields.len() + 1);
    record_fields.push(("provider".to_string(), provider.to_string()));
    for (key, value) in fields {
        record_fields.push(((*key).to_string(), (*value).to_string()));
    }
    write_event(LogLevel::Info, event, record_fields);
}

/// Log a tool call
pub fn tool_call(name: &str, input: &str, output: &str) {
    let fields = vec![
        ("tool".to_string(), sanitize_log_value(name)),
        ("input".to_string(), sanitize_payload_for_log(input, 200)),
        ("output".to_string(), sanitize_payload_for_log(output, 500)),
    ];
    write_event(LogLevel::Info, "tool.call", fields);
}

/// Log a crash/panic for auto-debug
pub fn crash(error: &str, context: &str) {
    let fields = vec![
        ("error".to_string(), error.to_string()),
        ("context".to_string(), context.to_string()),
    ];
    write_event(LogLevel::Error, "crash", fields);
}

/// Get the session ID from the current logging context (thread-local or task-local).
pub fn current_session() -> Option<String> {
    if let Some(ctx) = task_context_snapshot() {
        return ctx.session;
    }
    LOG_CONTEXT.with(|c| c.borrow().session.clone())
}

/// Get path to today's log file
pub fn log_path() -> Option<PathBuf> {
    let log_dir = log_dir()?;
    Some(daily_log_path(&log_dir, Local::now()))
}

/// Remove daily `jcode-*.log` / `jcode-desktop-*.log` files older than 7 days.
///
/// Scoped deliberately to the date-stamped log files this logger produces. The
/// log directory also holds non-log data (e.g. `memory/`, `memory-events-*.jsonl`)
/// that must NOT be touched here, so we never blanket-delete by mtime.
pub fn cleanup_old_logs() {
    if let Some(log_dir) = log_dir() {
        cleanup_old_logs_in(&log_dir, Local::now());
    }
}

/// Core of [`cleanup_old_logs`], parameterized on the directory and "now" so it
/// can be unit-tested without touching the real log directory or process env.
fn cleanup_old_logs_in(log_dir: &std::path::Path, now: chrono::DateTime<Local>) {
    let Ok(entries) = fs::read_dir(log_dir) else {
        return;
    };
    let cutoff = now - chrono::Duration::days(LOG_RETENTION_DAYS);
    for entry in entries.flatten() {
        // Only consider our own date-stamped log files.
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let is_jcode_log = (name.starts_with("jcode-") || name.starts_with("jcode-desktop-"))
            && name.ends_with(".log");
        if !is_jcode_log {
            continue;
        }

        if let Ok(metadata) = entry.metadata()
            && metadata.is_file()
            && let Ok(modified) = metadata.modified()
        {
            let modified: chrono::DateTime<Local> = modified.into();
            if modified < cutoff
                && let Err(err) = fs::remove_file(entry.path())
            {
                eprintln!("jcode logger cleanup failed: {err}");
            }
        }
    }
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() > max_len {
        format!("{}...", jcode_core::util::truncate_str(s, max_len))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests;
