use super::*;
use jcode_harness_api::ContextStatusSnapshot;

#[test]
fn context_prune_forwards_structural_kind_and_maps_success() {
    for (kind, keep_recent, after) in [
        (ContextPruneKind::Turns, Some(3), None),
        (ContextPruneKind::Tail, None, Some(" m4 ")),
        (ContextPruneKind::Undo, None, None),
    ] {
        let mut state = state_with_session();
        let out = state.api_request_to_legacy(&json!({
            "id": 20,
            "req": "context_prune",
            "kind": kind,
            "keep_recent": keep_recent,
            "after": after,
        }));
        let legacy_id = match &out[..] {
            [Outbound::Legacy(value)] => {
                assert_eq!(value["type"], "context_prune");
                assert_eq!(value["kind"], context_prune_kind_name(kind));
                if let Some(keep_recent) = keep_recent {
                    assert_eq!(value["keep_recent"], keep_recent);
                } else {
                    assert!(value.get("keep_recent").is_none());
                }
                if let Some(after) = after {
                    assert_eq!(value["after"], "m4");
                    assert_ne!(after, value["after"].as_str().unwrap());
                } else {
                    assert!(value.get("after").is_none());
                }
                value["id"].as_u64().unwrap()
            }
            other => panic!("expected context prune legacy request, got {other:?}"),
        };

        let frames = state.legacy_event_to_api(&json!({
            "type": "context_prune_result",
            "id": legacy_id,
            "message": "context prune applied",
            "success": true,
        }));
        match &frames[..] {
            [frame] => {
                assert_eq!(frame.reply_to, Some(20));
                assert_eq!(
                    frame.event,
                    ApiEvent::ContextPruned {
                        session_id: "s1".into(),
                        kind,
                        message: "context prune applied".into(),
                    }
                );
            }
            other => panic!("expected context prune reply, got {other:?}"),
        }
    }
}

#[test]
fn context_prune_rejects_kind_specific_options_locally() {
    for request in [
        json!({"id": 21, "req": "context_prune", "kind": "turns", "after": "m4"}),
        json!({"id": 22, "req": "context_prune", "kind": "tail", "keep_recent": 2, "after": "m4"}),
        json!({"id": 23, "req": "context_prune", "kind": "tail"}),
        json!({"id": 24, "req": "context_prune", "kind": "undo", "after": "m4"}),
    ] {
        let mut state = state_with_session();
        let out = state.api_request_to_legacy(&request);
        match &out[..] {
            [Outbound::Reply(frame)] => match &frame.event {
                ApiEvent::Error { code, .. } => assert_eq!(*code, ErrorCode::InvalidRequest),
                other => panic!("expected invalid_request, got {other:?}"),
            },
            other => panic!("invalid context prune reached daemon: {other:?}"),
        }
    }
}

#[test]
fn provider_reset_maps_success_and_failure() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({
        "id": 25,
        "req": "reset_provider",
    }));
    let legacy_id = match &out[..] {
        [Outbound::Legacy(value)] => {
            assert_eq!(value["type"], "reset_provider");
            value["id"].as_u64().unwrap()
        }
        other => panic!("expected provider reset legacy request, got {other:?}"),
    };

    let success = state.legacy_event_to_api(&json!({
        "type": "provider_reset_result",
        "id": legacy_id,
        "message": "provider reset",
        "success": true,
    }));
    assert_eq!(
        success[0].event,
        ApiEvent::ProviderReset {
            session_id: "s1".into(),
            message: "provider reset".into(),
        }
    );

    let out = state.api_request_to_legacy(&json!({
        "id": 26,
        "req": "reset_provider",
    }));
    let legacy_id = match &out[..] {
        [Outbound::Legacy(value)] => value["id"].as_u64().unwrap(),
        other => panic!("expected second provider reset request, got {other:?}"),
    };
    let failure = state.legacy_event_to_api(&json!({
        "type": "provider_reset_result",
        "id": legacy_id,
        "message": "provider unavailable",
        "success": false,
    }));
    assert!(matches!(
        &failure[0].event,
        ApiEvent::Error {
            code: ErrorCode::InvalidRequest,
            message,
        } if message == "provider unavailable"
    ));
}

#[test]
fn context_status_maps_only_the_requested_snapshot() {
    let mut state = state_with_session();
    let out = state.api_request_to_legacy(&json!({
        "id": 27,
        "req": "get_context_status",
    }));
    let legacy_id = match &out[..] {
        [Outbound::Legacy(value)] => {
            assert_eq!(value["type"], "state");
            value["id"].as_u64().unwrap()
        }
        other => panic!("expected state legacy request, got {other:?}"),
    };

    let frames = state.legacy_event_to_api(&json!({
        "type": "state",
        "id": legacy_id,
        "session_id": "s1",
        "context_status": {
            "schema_version": 1,
            "revision": 9,
            "provider_generation": 4,
            "estimated_input_tokens": 512,
            "observed_input_tokens": 480,
            "fingerprint": "fp-9"
        }
    }));
    match &frames[..] {
        [frame] => {
            assert_eq!(frame.reply_to, Some(27));
            assert_eq!(
                frame.event,
                ApiEvent::ContextStatus {
                    session_id: "s1".into(),
                    status: ContextStatusSnapshot {
                        schema_version: 1,
                        revision: 9,
                        provider_generation: 4,
                        estimated_input_tokens: 512,
                        observed_input_tokens: Some(480),
                        fingerprint: "fp-9".into(),
                    },
                }
            );
        }
        other => panic!("expected context status reply, got {other:?}"),
    }
}

#[test]
fn context_control_requests_need_an_attached_session() {
    for (req, extra) in [
        ("context_prune", json!({"kind": "turns", "keep_recent": 2})),
        ("reset_provider", json!({})),
        ("get_context_status", json!({})),
    ] {
        let mut state = BridgeState::default();
        let mut request = json!({"id": 1, "req": req});
        for (key, value) in extra.as_object().unwrap() {
            request[key] = value.clone();
        }
        let out = state.api_request_to_legacy(&request);
        match &out[..] {
            [Outbound::Reply(frame)] => match &frame.event {
                ApiEvent::Error { code, .. } => assert_eq!(
                    *code,
                    ErrorCode::UnknownSession,
                    "{req} should report an unattached session"
                ),
                other => panic!("{req}: unexpected {other:?}"),
            },
            other => panic!("{req} reached the daemon unattached: {other:?}"),
        }
    }
}
