//! Logging infrastructure for jcode
//!
//! Logs to ~/.jcode/logs/ with automatic rotation
//!
//! Supports thread-local context for server, session, provider, and model info.

pub mod watchdog;

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

fn redact_field_value(key: &str, value: &str) -> String {
    if is_sensitive_key(key) {
        return "<redacted>".to_string();
    }
    sanitize_log_value(value)
}

fn sanitize_log_value(value: &str) -> String {
    let value: String = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let value = redact_url_queries(&value);
    let value = redact_sensitive_assignments(&value);
    let value = redact_known_secret_tokens(&value);
    truncate(&value, 160)
}

fn redact_url_queries(value: &str) -> String {
    value
        .split_whitespace()
        .map(redact_url_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_url_token(token: &str) -> String {
    let Some(start) = ["https://", "http://"]
        .iter()
        .filter_map(|scheme| token.find(scheme))
        .min()
    else {
        return token.to_string();
    };
    let url = &token[start..];
    let Some(query_start) = url.find('?') else {
        return token.to_string();
    };
    format!("{}{}?<redacted>", &token[..start], &url[..query_start])
}

fn normalized_log_key(key: &str) -> String {
    key.trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .to_ascii_lowercase()
        .replace('-', "_")
}

fn is_safe_metadata_key(key: &str) -> bool {
    [
        "_id",
        "_name",
        "_count",
        "_len",
        "_length",
        "_present",
        "_configured",
        "_available",
        "_enabled",
        "_source",
        "_type",
    ]
    .iter()
    .any(|suffix| key.ends_with(suffix))
}

fn is_sensitive_key(key: &str) -> bool {
    let key = normalized_log_key(key);
    if key.is_empty() || is_safe_metadata_key(&key) {
        return false;
    }

    matches!(
        key.as_str(),
        "api_key"
            | "apikey"
            | "key"
            | "token"
            | "secret"
            | "credential"
            | "authorization"
            | "password"
            | "device_code"
            | "magic_link"
            | "callback_url"
            | "approval_url"
            | "checkout_url"
            | "portal_url"
            | "checkout_session_url"
            | "portal_session_url"
            | "client_secret"
            | "access_token"
            | "refresh_token"
            | "webhook_secret"
            | "webhook_signing_secret"
            | "private_key"
            | "signing_key"
            | "cookie"
            | "set_cookie"
            | "auth_header"
            | "proxy_authorization"
            | "auth_code"
            | "oauth_code"
            | "code_verifier"
            | "code_challenge"
            | "session_url"
    ) || key.starts_with("authorization_")
        || key.starts_with("api_key_")
        || key.ends_with("_api_key")
        || key.ends_with("_token")
        || key.ends_with("_secret")
        || key.ends_with("_credential")
        || key.ends_with("_device_code")
        || key.ends_with("_magic_link")
        || key.ends_with("_magic_link_url")
        || key.ends_with("_callback_url")
        || key.ends_with("_approval_url")
        || key.ends_with("_checkout_url")
        || key.ends_with("_portal_url")
        || key.ends_with("_session_url")
        || key.ends_with("_checkout_session_url")
        || key.ends_with("_portal_session_url")
        || key.ends_with("_client_secret")
        || key.ends_with("_access_token")
        || key.ends_with("_refresh_token")
        || key.ends_with("_secret_key")
        || key.ends_with("_private_key")
        || key.ends_with("_signing_key")
        || key.ends_with("_auth_code")
        || key.ends_with("_oauth_code")
        || key.ends_with("_code_verifier")
        || key.ends_with("_code_challenge")
}

fn redact_sensitive_assignments(value: &str) -> String {
    let mut output = Vec::new();
    let mut redact_next = 0u8;

    for token in value.split_whitespace() {
        if redact_next > 0 {
            if token.eq_ignore_ascii_case("bearer") || token.eq_ignore_ascii_case("token") {
                output.push(token.to_string());
            } else {
                output.push("<redacted>".to_string());
            }
            redact_next -= 1;
            continue;
        }

        if token.eq_ignore_ascii_case("bearer") || token.eq_ignore_ascii_case("token") {
            output.push(token.to_string());
            redact_next = 1;
            continue;
        }

        if let Some((key, separator, assigned_value)) = split_assignment(token)
            && is_sensitive_key(key)
        {
            output.push(format!("{key}{separator}<redacted>"));
            if assigned_value.is_empty() {
                redact_next = 2;
            }
            continue;
        }

        if let Some(key) = token.strip_suffix(':')
            && is_sensitive_key(key)
        {
            output.push(format!("{key}:"));
            redact_next = 2;
            continue;
        }

        output.push(token.to_string());
    }

    output.join(" ")
}

fn redact_known_secret_tokens(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;

    for (offset, _) in value.char_indices() {
        if offset < cursor {
            continue;
        }
        let Some(end) = known_secret_span(&value[offset..]) else {
            continue;
        };
        output.push_str(&value[cursor..offset]);
        output.push_str("<redacted>");
        cursor = offset + end;
    }

    output.push_str(&value[cursor..]);
    output
}

fn known_secret_span(value: &str) -> Option<usize> {
    if value.starts_with("-----BEGIN ") && value.contains("PRIVATE KEY-----") {
        let end_start = value.find("-----END ")?;
        let end_body_start = end_start + "-----END ".len();
        let end_body = value[end_body_start..].find("-----")?;
        return Some(end_body_start + end_body + 5);
    }

    for (prefix, minimum_suffix, allowed) in [
        ("sk-ant-oat01-", 8, is_token_char),
        ("sk-ant-ort01-", 8, is_token_char),
        ("sk-or-v1-", 8, is_token_char),
        ("sk-proj-", 8, is_token_char),
        ("sk_live_", 8, is_token_char),
        ("rk_live_", 8, is_token_char),
        ("sk_test_", 8, is_token_char),
        ("rk_test_", 8, is_token_char),
        ("whsec_", 8, is_token_char),
        ("jck_live_", 8, is_token_char),
        ("ghp_", 8, is_token_char),
        ("gho_", 8, is_token_char),
        ("ghu_", 8, is_token_char),
        ("ghs_", 8, is_token_char),
        ("github_pat_", 8, is_token_char),
        ("xoxb-", 8, is_token_char),
        ("xoxp-", 8, is_token_char),
        ("xoxa-", 8, is_token_char),
        ("xoxr-", 8, is_token_char),
        ("npm_", 8, is_token_char),
    ] {
        if let Some(suffix) = value.strip_prefix(prefix) {
            let suffix_len = suffix.bytes().take_while(|byte| allowed(*byte)).count();
            if suffix_len >= minimum_suffix {
                return Some(prefix.len() + suffix_len);
            }
        }
    }

    if let Some(suffix) = value.strip_prefix("ya29.") {
        let suffix_len = suffix
            .bytes()
            .take_while(|byte| is_token_char(*byte) || *byte == b'.')
            .count();
        if suffix_len >= 20 {
            return Some("ya29.".len() + suffix_len);
        }
    }

    if let Some(suffix) = value.strip_prefix("AIza") {
        let suffix_len = suffix
            .bytes()
            .take_while(|byte| is_token_char(*byte))
            .count();
        if suffix_len >= 20 {
            return Some("AIza".len() + suffix_len);
        }
    }

    if value.starts_with("AKIA")
        && value.as_bytes().get(4..20).is_some_and(|suffix| {
            suffix.len() == 16
                && suffix
                    .iter()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        })
        && value
            .as_bytes()
            .get(20)
            .is_none_or(|byte| !is_token_char(*byte))
    {
        return Some(20);
    }

    if value.starts_with("eyJ")
        && let Some(span) = jwt_like_span(value)
    {
        return Some(span);
    }

    None
}

fn is_token_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

fn jwt_like_span(value: &str) -> Option<usize> {
    let mut cursor = 0;
    for index in 0..3 {
        let start = cursor;
        while value
            .as_bytes()
            .get(cursor)
            .is_some_and(|byte| is_jwt_char(*byte))
        {
            cursor += 1;
        }
        if cursor - start < 10 {
            return None;
        }
        if index < 2 {
            if value.as_bytes().get(cursor) != Some(&b'.') {
                return None;
            }
            cursor += 1;
        }
    }
    Some(cursor)
}

fn is_jwt_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

fn split_assignment(token: &str) -> Option<(&str, &str, &str)> {
    let (index, separator) = token
        .char_indices()
        .find(|(_, character)| *character == '=' || *character == ':')?;
    let separator_end = index + separator.len_utf8();
    Some((
        &token[..index],
        &token[index..separator_end],
        &token[separator_end..],
    ))
}

fn sanitize_payload_for_log(value: &str, max_chars: usize) -> String {
    let sanitized = match serde_json::from_str::<serde_json::Value>(value) {
        Ok(mut payload) => {
            redact_json_value(&mut payload);
            serde_json::to_string(&payload).unwrap_or_else(|_| "<unserializable>".to_string())
        }
        Err(_) => sanitize_log_value(value),
    };
    truncate(&sanitized, max_chars)
}

fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                if is_sensitive_key(key) {
                    *value = serde_json::Value::String("<redacted>".to_string());
                } else {
                    redact_json_value(value);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json_value(value);
            }
        }
        serde_json::Value::String(value) => {
            *value = sanitize_log_value(value);
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_log_redacts_secret_like_fields() {
        assert_eq!(redact_field_value("api_key", "sk-secret"), "<redacted>");
        assert_eq!(
            redact_field_value("callback_url", "https://example.com/?code=secret"),
            "<redacted>"
        );
    }

    #[test]
    fn auth_log_sanitizes_urls_and_control_characters() {
        assert_eq!(
            sanitize_log_value("failed\nhttps://login.example.com/cb?code=secret&state=abc"),
            "failed https://login.example.com/cb?<redacted>"
        );
    }

    #[test]
    fn minimum_log_level_from_env_is_deterministic() {
        assert_eq!(
            minimum_log_level_from_env(false, None),
            Some(LogLevel::Info)
        );
        assert_eq!(
            minimum_log_level_from_env(false, Some("warn")),
            Some(LogLevel::Warn)
        );
        assert_eq!(
            minimum_log_level_from_env(false, Some("ERROR")),
            Some(LogLevel::Error)
        );
        assert_eq!(
            minimum_log_level_from_env(false, Some("trace")),
            Some(LogLevel::Debug)
        );
        assert_eq!(minimum_log_level_from_env(false, Some("off")), None);
        assert_eq!(
            minimum_log_level_from_env(true, Some("error")),
            Some(LogLevel::Debug)
        );
    }

    #[test]
    fn record_is_parseable_jsonl_and_has_context() {
        let context = LogContext {
            server: Some("shared-server".to_string()),
            session: Some("session-123".to_string()),
            provider: Some("provider-a".to_string()),
            model: Some("model-a".to_string()),
        };
        let record = serialize_record(
            Local::now(),
            LogLevel::Warn,
            Some("server_request"),
            Some("server_request"),
            &context,
            vec![
                ("api_key", "sk-secret"),
                ("exit_code", "127"),
                ("message", "field collision"),
            ],
        )
        .expect("serialize log record");
        let line = format!("{record}\n");
        assert!(line.ends_with('\n'));

        let parsed: serde_json::Value = serde_json::from_str(record.as_str()).expect("valid JSONL");
        assert_eq!(parsed["schema_version"], serde_json::json!(1));
        assert_eq!(parsed["level"], serde_json::json!("WARN"));
        assert_eq!(parsed["event"], serde_json::json!("server_request"));
        assert_eq!(parsed["message"], serde_json::json!("server_request"));
        assert_eq!(parsed["server"], serde_json::json!("shared-server"));
        assert_eq!(parsed["session"], serde_json::json!("session-123"));
        assert_eq!(parsed["provider"], serde_json::json!("provider-a"));
        assert_eq!(parsed["model"], serde_json::json!("model-a"));
        assert_eq!(parsed["api_key"], serde_json::json!("<redacted>"));
        assert_eq!(parsed["exit_code"], serde_json::json!("127"));
        assert_eq!(
            parsed["field.message"],
            serde_json::json!("field collision")
        );
    }

    #[test]
    fn plain_messages_redact_sensitive_assignments_and_urls() {
        let message = sanitize_log_value(
            "Authorization: Bearer super-secret api_key=sk-secret target_url=https://example.test/cb?code=secret",
        );
        assert!(!message.contains("super-secret"));
        assert!(!message.contains("sk-secret"));
        assert!(!message.contains("code=secret"));
        assert!(message.contains("https://example.test/cb?<redacted>"));
    }

    #[test]
    fn json_payload_redacts_sensitive_keys_and_keeps_safe_values() {
        let payload = r#"{"api_key":"sk-secret","nested":{"device_code":"device-secret","safe":"ok\n"},"exit_code":127,"authorization":"Bearer secret"}"#;
        let sanitized = sanitize_payload_for_log(payload, 1_000);
        let parsed: serde_json::Value =
            serde_json::from_str(&sanitized).expect("sanitized JSON payload");
        assert_eq!(parsed["api_key"], serde_json::json!("<redacted>"));
        assert_eq!(
            parsed["nested"]["device_code"],
            serde_json::json!("<redacted>")
        );
        assert_eq!(parsed["nested"]["safe"], serde_json::json!("ok"));
        assert_eq!(parsed["exit_code"], serde_json::json!(127));
        assert_eq!(parsed["authorization"], serde_json::json!("<redacted>"));
    }

    #[test]
    fn plain_messages_redact_known_direct_secret_formats() {
        let direct_tokens = vec![
            format!("sk-ant-oat01-{}", "A".repeat(24)),
            format!("sk-ant-ort01-{}", "B".repeat(24)),
            format!("sk-or-v1-{}", "C".repeat(24)),
            format!("sk_live_{}", "D".repeat(24)),
            format!("whsec_{}", "E".repeat(24)),
            format!("ghp_{}", "F".repeat(24)),
            format!("github_pat_{}", "G".repeat(24)),
            format!("xoxb-{}", "H".repeat(24)),
            format!("ya29.{}", "I".repeat(24)),
            format!("AIza{}", "J".repeat(24)),
            "AKIAABCDEFGHIJKLMNOP".to_string(),
            format!(
                "eyJ{}.eyJ{}.eyJ{}",
                "a".repeat(12),
                "b".repeat(12),
                "c".repeat(12)
            ),
        ];

        for token in direct_tokens {
            let sanitized = sanitize_log_value(&format!("diagnostic {token}"));
            assert!(
                !sanitized.contains(&token),
                "direct token leaked through sanitizer: {token}"
            );
            assert!(sanitized.contains("<redacted>"));
        }

        let private_key =
            "-----BEGIN RSA PRIVATE KEY----- private-material -----END RSA PRIVATE KEY-----";
        let sanitized = sanitize_log_value(private_key);
        assert!(!sanitized.contains("private-material"));
        assert!(!sanitized.contains("PRIVATE KEY-----"));
    }

    #[test]
    fn record_caps_event_fields_and_marks_truncation() {
        let context = LogContext::default();
        let fields = (0..(MAX_EVENT_FIELDS + 2))
            .map(|index| (format!("field_{index}"), index.to_string()))
            .collect::<Vec<_>>();
        let record = serialize_record(
            Local::now(),
            LogLevel::Info,
            Some("bounded"),
            None,
            &context,
            fields,
        )
        .expect("serialize bounded log record");
        let parsed: serde_json::Value = serde_json::from_str(&record).expect("valid JSONL");

        assert_eq!(parsed["fields_truncated"], serde_json::json!(true));
        assert_eq!(parsed["field_0"], serde_json::json!("0"));
        assert_eq!(
            parsed[&format!("field_{}", MAX_EVENT_FIELDS - 1)],
            serde_json::json!((MAX_EVENT_FIELDS - 1).to_string())
        );
        assert!(parsed.get(format!("field_{MAX_EVENT_FIELDS}")).is_none());
    }

    #[test]
    fn size_rotation_uses_next_available_path_without_overwrite() {
        use std::time::SystemTime;

        let dir = std::env::temp_dir().join(format!(
            "jcode-log-rotation-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("create rotation directory");

        let now = Local::now();
        let path = daily_log_path(&dir, now);
        let mut logger = Logger::open(path.clone(), 16).expect("open test logger");
        logger
            .append_line("first line\n", now)
            .expect("write first line");
        logger
            .append_line("second line\n", now)
            .expect("rotate and write second line");

        let rotated_one = rotated_log_path(&path, 1);
        let rotated_two = rotated_log_path(&path, 2);
        let rotated_three = rotated_log_path(&path, 3);
        assert_eq!(
            fs::read_to_string(&rotated_one).expect("read first rotation"),
            "first line\n"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("read active log"),
            "second line\n"
        );

        fs::write(&rotated_two, "keep\n").expect("reserve second rotation path");
        logger
            .append_line("third line\n", now)
            .expect("skip occupied paths and rotate");

        assert_eq!(
            fs::read_to_string(&rotated_two).expect("read reserved path"),
            "keep\n"
        );
        assert_eq!(
            fs::read_to_string(&rotated_three).expect("read third rotation"),
            "second line\n"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("read active log after rotation"),
            "third line\n"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_active_log_is_rotated_before_jsonl_append() {
        use std::time::SystemTime;

        let dir = std::env::temp_dir().join(format!(
            "jcode-log-legacy-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("create legacy log directory");

        let now = Local::now();
        let path = daily_log_path(&dir, now);
        fs::write(&path, "[2026-09-15 00:00:00.000] [INFO] legacy\n").expect("write legacy log");
        let mut logger = Logger::open(path.clone(), 1_000).expect("open migrated logger");
        logger
            .append_line("{\"schema_version\":1}\n", now)
            .expect("write JSONL record");

        let rotated = rotated_log_path(&path, 1);
        assert_eq!(
            fs::read_to_string(&rotated).expect("read legacy rotation"),
            "[2026-09-15 00:00:00.000] [INFO] legacy\n"
        );
        let current = fs::read_to_string(&path).expect("read migrated active log");
        let parsed: serde_json::Value =
            serde_json::from_str(current.trim_end()).expect("migrated active line is JSONL");
        assert_eq!(parsed["schema_version"], serde_json::json!(1));

        drop(logger);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn daily_rotation_switches_to_the_new_date_without_touching_old_log() {
        use std::time::SystemTime;

        let dir = std::env::temp_dir().join(format!(
            "jcode-log-daily-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("create daily rotation directory");

        let first = Local::now();
        let second = first + chrono::Duration::days(1);
        let first_path = daily_log_path(&dir, first);
        let second_path = daily_log_path(&dir, second);
        let mut logger = Logger::open(first_path.clone(), 1_000).expect("open daily logger");
        logger
            .append_line("first day\n", first)
            .expect("write first day");
        logger
            .append_line("second day\n", second)
            .expect("switch daily log");

        assert_eq!(
            fs::read_to_string(&first_path).expect("read first day log"),
            "first day\n"
        );
        assert_eq!(
            fs::read_to_string(&second_path).expect("read second day log"),
            "second day\n"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn diagnostic_field_names_are_not_redacted() {
        // The model-picker / login diagnostics intentionally avoid field names
        // that match credential-like keys. If any of these regress, the uploaded
        // logs we ask users for would show `<redacted>` instead of the
        // boolean/count/env-var-name we need.
        for name in [
            "env_var",
            "input_len",
            "optional",
            "anthropic_api",
            "openai_api",
            "azure_api",
            "copilot_cred",
            "session_provider",
            "routes_in",
            "by_provider",
            "requested_model",
            "route_provider",
        ] {
            assert_eq!(
                redact_field_value(name, "value"),
                "value",
                "diagnostic field `{name}` should not be redacted",
            );
        }
    }

    #[test]
    fn cleanup_removes_only_old_jcode_logs() {
        use std::time::{Duration, SystemTime};

        let dir = std::env::temp_dir().join(format!(
            "jcode-log-cleanup-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("create temp log dir");

        let old_mtime = SystemTime::now() - Duration::from_secs(60 * 60 * 24 * 30); // 30 days

        let write = |name: &str, age_old: bool| {
            let path = dir.join(name);
            let mut f = File::create(&path).expect("create file");
            f.write_all(b"x").ok();
            if age_old {
                f.set_modified(old_mtime).expect("set mtime");
            }
            path
        };

        // Old log files that SHOULD be deleted.
        let old_log = write("jcode-2000-01-01.log", true);
        let old_desktop = write("jcode-desktop-2000-01-01.log", true);
        // Recent log file that SHOULD survive.
        let new_log = write("jcode-2099-01-01.log", false);
        // Non-log data that SHOULD survive even though it is old.
        let old_memory = write("memory-events-2000-01-01.jsonl", true);
        let old_other = write("notes-2000-01-01.txt", true);
        // A subdirectory (e.g. `memory/`) must never be removed.
        let subdir = dir.join("memory");
        fs::create_dir_all(&subdir).expect("create subdir");

        cleanup_old_logs_in(&dir, Local::now());

        assert!(!old_log.exists(), "old jcode log should be deleted");
        assert!(!old_desktop.exists(), "old desktop log should be deleted");
        assert!(new_log.exists(), "recent jcode log must survive");
        assert!(old_memory.exists(), "memory-events jsonl must survive");
        assert!(old_other.exists(), "unrelated files must survive");
        assert!(subdir.is_dir(), "subdirectories must survive");

        fs::remove_dir_all(&dir).ok();
    }
}
