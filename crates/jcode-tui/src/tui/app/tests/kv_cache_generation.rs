#[test]
fn remote_cache_generation_change_clears_old_baseline_once() {
    let mut app = create_test_app();
    app.is_remote = true;
    app.remote_session_id = Some("session_generation".to_string());

    let history = vec![
        Message::user("first prompt"),
        Message::assistant_text("first answer"),
    ];
    let baseline_signature = App::kv_cache_request_signature(&history, &[], "system", "");
    app.kv_cache.kv_cache_baseline = Some(KvCacheBaseline {
        session_id: Some("session_generation".to_string()),
        cache_generation: app.kv_cache.cache_generation,
        input_tokens: 1_000,
        completed_at: Instant::now(),
        provider: "anthropic".to_string(),
        model: "claude-opus-4-6".to_string(),
        upstream_provider: None,
        signature: Some(baseline_signature),
    });

    let next_generation = app.kv_cache.cache_generation.wrapping_add(1);
    let changed = vec![
        Message::user("changed prompt"),
        Message::assistant_text("first answer"),
    ];
    let changed_signature = App::kv_cache_request_signature(&changed, &[], "system", "");
    app.begin_remote_kv_cache(changed_signature.clone(), Some(next_generation));

    assert_eq!(app.kv_cache.cache_generation, next_generation);
    let request = app
        .kv_cache
        .pending_kv_cache_request
        .as_ref()
        .expect("request should be pending");
    assert!(request.baseline.is_none());
    assert_eq!(request.baseline_messages_prefix_matches, None);

    app.begin_remote_kv_cache(changed_signature, Some(next_generation));
    assert_eq!(
        app.kv_cache.cache_generation, next_generation,
        "the same generation must not trigger another reset"
    );
}

#[test]
fn remote_token_usage_records_cache_stats_before_done_and_dedupes_snapshots() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();

    app.is_remote = true;
    app.remote_provider_name = Some("OpenAI".to_string());
    app.remote_provider_model = Some("gpt-5.5".to_string());
    app.display_messages
        .push(DisplayMessage::user("live prompt"));

    app.handle_server_event(
        crate::protocol::ServerEvent::KvCacheRequest {
            system_static_hash: 1,
            tools_hash: 2,
            messages_hash: 3,
            message_hashes: vec![11, 22],
            message_count: 2,
            tool_count: 33,
            system_static_chars: 11155,
            tools_json_chars: 35228,
            messages_json_chars: 198612,
            ephemeral_hash: None,
            ephemeral_chars: 2,
            ephemeral_message_count: 0,
            cache_generation: None,
        },
        &mut remote,
    );
    app.handle_server_event(
        crate::protocol::ServerEvent::TokenUsage {
            input: 63_762,
            output: 153,
            cache_read_input: Some(0),
            cache_creation_input: None,
        },
        &mut remote,
    );

    assert_eq!(
        app.token_accounting.total_cache_reported_input_tokens,
        63_762
    );
    assert_eq!(app.token_accounting.total_cache_read_tokens, 0);
    assert_eq!(
        app.token_accounting.last_cache_reported_input_tokens,
        Some(63_762)
    );
    assert_eq!(app.token_accounting.total_input_tokens, 63_762);
    assert!(app.last_api_completed.is_some());
    assert!(app.kv_cache.pending_kv_cache_request.is_none());

    app.handle_server_event(
        crate::protocol::ServerEvent::TokenUsage {
            input: 63_762,
            output: 153,
            cache_read_input: Some(0),
            cache_creation_input: None,
        },
        &mut remote,
    );

    assert_eq!(
        app.token_accounting.total_cache_reported_input_tokens,
        63_762
    );
    assert_eq!(app.token_accounting.total_input_tokens, 63_762);

    assert!(super::state_ui::handle_info_command(
        &mut app,
        "/cache stats"
    ));
    let stats = app.display_messages().last().unwrap().content.clone();
    assert!(
        stats.contains("- total_cache_reported_input_tokens: 63.8k (63,762)"),
        "{stats}"
    );
    assert!(
        stats.contains("- baseline.signature.messages_json_chars: 198.6k (198,612)"),
        "{stats}"
    );
    assert!(
        stats.contains("- current_api_usage_recorded: true"),
        "{stats}"
    );
}

#[test]
fn test_handle_server_event_kv_cache_request_resets_tps_output_watermark_for_next_api_call() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();

    app.streaming.streaming_tps_collect_output = true;

    app.handle_server_event(
        crate::protocol::ServerEvent::TokenUsage {
            input: 100,
            output: 40,
            cache_read_input: None,
            cache_creation_input: None,
        },
        &mut remote,
    );

    app.handle_server_event(
        crate::protocol::ServerEvent::KvCacheRequest {
            system_static_hash: 1,
            tools_hash: 2,
            messages_hash: 3,
            message_hashes: vec![11, 22],
            message_count: 2,
            tool_count: 1,
            system_static_chars: 10,
            tools_json_chars: 20,
            messages_json_chars: 30,
            ephemeral_hash: None,
            ephemeral_chars: 0,
            ephemeral_message_count: 0,
            cache_generation: None,
        },
        &mut remote,
    );

    assert!(!app.streaming.streaming_tps_collect_output);

    app.handle_server_event(
        crate::protocol::ServerEvent::ConnectionPhase {
            phase: "streaming".to_string(),
        },
        &mut remote,
    );

    assert!(app.streaming.streaming_tps_collect_output);

    app.handle_server_event(
        crate::protocol::ServerEvent::TokenUsage {
            input: 120,
            output: 15,
            cache_read_input: None,
            cache_creation_input: None,
        },
        &mut remote,
    );

    assert_eq!(app.streaming.streaming_total_output_tokens, 55);
    assert_eq!(app.streaming.streaming_tps_observed_output_tokens, 55);
}
