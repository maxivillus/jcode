use super::*;

#[test]
fn new_state_has_default_schema_and_revision() {
    let state = ExecutionState::new("code-review").unwrap();

    assert_eq!(state.schema_version, EXECUTION_STATE_SCHEMA_VERSION);
    assert_eq!(state.state_schema, "code-review");
    assert_eq!(state.revision, ExecutionStateRevision::INITIAL);
    assert_eq!(state.contract().state_schema, "code-review");
    assert!(state.validate().is_ok());
}

#[test]
fn contract_serializes_all_machine_readable_fields() {
    let state = ExecutionState::new("code-review").unwrap();
    let encoded = serde_json::to_value(state.contract()).unwrap();

    for field in [
        "schema_version",
        "state_schema",
        "required_fields",
        "field_limits",
        "observation_sources",
        "allowed_actions",
        "state_retention_policy",
        "conflict_policy",
    ] {
        assert!(
            encoded.get(field).is_some(),
            "missing contract field {field}"
        );
    }
    assert_eq!(
        encoded["field_limits"]["goal"]["max_chars"],
        serde_json::json!(MAX_EXECUTION_STATE_TEXT_CHARS)
    );
    assert_eq!(
        encoded["allowed_actions"],
        serde_json::json!([
            "get_state",
            "propose_patch",
            "record_observation",
            "retrieve_evidence",
            "reconcile"
        ])
    );
}

#[test]
fn legacy_state_json_uses_compatible_contract_defaults() {
    let legacy_json = r#"
        {
            "schema_version": 1,
            "state_schema": "code-review",
            "revision": 0,
            "goal": "Review the current diff"
        }
        "#;

    let state: ExecutionState = serde_json::from_str(legacy_json).unwrap();

    assert_eq!(state.goal.as_deref(), Some("Review the current diff"));
    assert_eq!(state.contract().state_schema, "code-review");
    assert!(state.validate().is_ok());
}

#[test]
fn contract_limits_are_enforced_for_state_payload() {
    let mut state = ExecutionState::new("code-review").unwrap();
    state.field_limits.get_mut("goal").unwrap().max_chars = Some(4);
    state.goal = Some("five!".to_string());

    assert_eq!(
        state.validate(),
        Err(ExecutionStateError::FieldTooLong {
            field: "goal",
            max_chars: 4,
            actual_chars: 5,
        })
    );
}

#[test]
fn invalid_contract_limits_are_rejected() {
    let mut state = ExecutionState::new("code-review").unwrap();
    state.field_limits.get_mut("goal").unwrap().max_chars =
        Some(MAX_EXECUTION_STATE_TEXT_CHARS + 1);

    assert_eq!(
        state.validate(),
        Err(ExecutionStateError::InvalidFieldLimit {
            field: "goal".to_string(),
        })
    );

    let mut state = ExecutionState::new("code-review").unwrap();
    state.field_limits.remove("goal");
    assert_eq!(
        state.validate(),
        Err(ExecutionStateError::MissingFieldLimit { field: "goal" })
    );
}

#[test]
fn required_contract_fields_must_be_known_and_present() {
    let mut state = ExecutionState::new("code-review").unwrap();
    state.required_fields.push("goal".to_string());
    assert_eq!(
        state.validate(),
        Err(ExecutionStateError::RequiredFieldMissing {
            field: "goal".to_string(),
        })
    );

    state.required_fields = vec!["unexpected".to_string()];
    assert_eq!(
        state.validate(),
        Err(ExecutionStateError::UnknownContractField {
            field: "unexpected".to_string(),
        })
    );
}

#[test]
fn patch_updates_fields_and_advances_revision() {
    let mut state = ExecutionState::new("code-review").unwrap();
    let mut patch = ExecutionStatePatch::new("code-review", state.revision);
    patch.goal = Some(PatchValue::Set("Review the current diff".to_string()));
    patch.pending = Some(PatchValue::Set(vec!["Inspect changed files".to_string()]));
    patch.next_action = Some(PatchValue::Set("Run focused tests".to_string()));

    state.apply_patch(&patch).unwrap();

    assert_eq!(state.revision, ExecutionStateRevision(1));
    assert_eq!(state.goal.as_deref(), Some("Review the current diff"));
    assert_eq!(
        state.pending,
        Some(vec!["Inspect changed files".to_string()])
    );
    assert_eq!(state.next_action.as_deref(), Some("Run focused tests"));
}

#[test]
fn json_null_clears_a_field() {
    let mut state = ExecutionState::new("code-review").unwrap();
    state.goal = Some("Old goal".to_string());
    let patch_json = r#"
        {
            "patch_schema_version": 1,
            "state_schema": "code-review",
            "expected_revision": 0,
            "goal": null
        }
        "#;
    let patch: ExecutionStatePatch = serde_json::from_str(patch_json).unwrap();

    assert_eq!(patch.goal, Some(PatchValue::Clear));

    state.apply_patch(&patch).unwrap();

    assert_eq!(state.goal, None);
    assert_eq!(state.revision, ExecutionStateRevision(1));
}

#[test]
fn patch_serialization_preserves_omitted_and_clear_fields() {
    let mut patch = ExecutionStatePatch::new("code-review", ExecutionStateRevision(3));
    patch.goal = Some(PatchValue::Clear);
    patch.phase = Some(PatchValue::Set("review".to_string()));

    let encoded = serde_json::to_value(&patch).unwrap();
    let object = encoded.as_object().unwrap();
    assert_eq!(object.get("goal"), Some(&serde_json::Value::Null));
    assert_eq!(
        object.get("phase").and_then(|value| value.as_str()),
        Some("review")
    );
    assert!(!object.contains_key("pending"));

    let decoded: ExecutionStatePatch = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded.goal, Some(PatchValue::Clear));
    assert_eq!(decoded.phase, Some(PatchValue::Set("review".to_string())));
    assert_eq!(decoded.pending, None);
}

#[test]
fn unknown_patch_fields_are_rejected() {
    let patch_json = r#"
        {
            "patch_schema_version": 1,
            "state_schema": "code-review",
            "expected_revision": 0,
            "goal": "Review",
            "unexpected": true
        }
        "#;

    let error = serde_json::from_str::<ExecutionStatePatch>(patch_json).unwrap_err();

    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn stale_patch_is_rejected_without_mutating_state() {
    let mut state = ExecutionState::new("code-review").unwrap();
    state.goal = Some("Current goal".to_string());
    let before = state.clone();
    let mut patch = ExecutionStatePatch::new("code-review", ExecutionStateRevision(4));
    patch.goal = Some(PatchValue::Set("Stale goal".to_string()));

    let error = state.apply_patch(&patch).unwrap_err();

    assert_eq!(
        error,
        ExecutionStateError::RevisionMismatch {
            expected: ExecutionStateRevision(4),
            actual: ExecutionStateRevision::INITIAL,
        }
    );
    assert_eq!(state, before);
}

#[test]
fn schema_mismatch_is_rejected() {
    let mut state = ExecutionState::new("code-review").unwrap();
    let mut patch = ExecutionStatePatch::new("release-plan", state.revision);
    patch.phase = Some(PatchValue::Set("review".to_string()));

    let error = state.apply_patch(&patch).unwrap_err();

    assert_eq!(
        error,
        ExecutionStateError::StateSchemaMismatch {
            expected: "code-review".to_string(),
            actual: "release-plan".to_string(),
        }
    );
    assert_eq!(state.revision, ExecutionStateRevision::INITIAL);
}

#[test]
fn invalid_bounds_are_rejected() {
    let mut state = ExecutionState::new("code-review").unwrap();
    let mut patch = ExecutionStatePatch::new("code-review", state.revision);
    patch.goal = Some(PatchValue::Set(
        "x".repeat(MAX_EXECUTION_STATE_TEXT_CHARS + 1),
    ));

    assert!(matches!(
        state.apply_patch(&patch),
        Err(ExecutionStateError::FieldTooLong { field: "goal", .. })
    ));
}

#[test]
fn empty_patch_is_rejected() {
    let mut state = ExecutionState::new("code-review").unwrap();
    let patch = ExecutionStatePatch::new("code-review", state.revision);

    assert_eq!(
        state.apply_patch(&patch),
        Err(ExecutionStateError::EmptyPatch)
    );
}

#[test]
fn revision_exhaustion_is_rejected() {
    let mut state = ExecutionState::new("code-review").unwrap();
    state.revision = ExecutionStateRevision(u64::MAX);
    let mut patch = ExecutionStatePatch::new("code-review", state.revision);
    patch.phase = Some(PatchValue::Set("done".to_string()));

    assert_eq!(
        state.apply_patch(&patch),
        Err(ExecutionStateError::RevisionExhausted)
    );
}

#[test]
fn fingerprint_is_deterministic_and_changes_with_state() {
    let mut first = ExecutionState::new("code-review").unwrap();
    let second = first.clone();
    assert_eq!(first.fingerprint(), second.fingerprint());

    first.goal = Some("changed".to_string());
    assert_ne!(first.fingerprint(), second.fingerprint());
}
