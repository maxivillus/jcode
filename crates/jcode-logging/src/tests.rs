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
fn context_event_record_is_jsonl_and_keeps_only_aggregate_usage() {
    let record = serialize_record(
        Local::now(),
        LogLevel::Debug,
        Some("CONTEXT_PROVIDER_USAGE"),
        Some("CONTEXT_PROVIDER_USAGE"),
        &LogContext::default(),
        vec![
            ("revision", "7"),
            ("input_tokens", "1200"),
            ("output_tokens", "18"),
            ("cache_read_input_tokens", "800"),
            ("context_revision_accepted", "true"),
        ],
    )
    .expect("serialize context event");
    let parsed: serde_json::Value = serde_json::from_str(&record).expect("valid JSONL");

    assert_eq!(parsed["event"], serde_json::json!("CONTEXT_PROVIDER_USAGE"));
    assert_eq!(
        parsed["message"],
        serde_json::json!("CONTEXT_PROVIDER_USAGE")
    );
    assert_eq!(parsed["revision"], serde_json::json!("7"));
    assert_eq!(parsed["input_tokens"], serde_json::json!("1200"));
    assert_eq!(
        parsed["context_revision_accepted"],
        serde_json::json!("true")
    );
    assert!(parsed.get("prompt").is_none());
    assert!(parsed.get("response").is_none());
    assert!(parsed.get("credentials").is_none());
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
