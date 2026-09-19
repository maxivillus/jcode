use super::*;
use crate::execution_state::{PatchValue, WorkflowRunRevision, WorkflowStatePatch};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

fn tool_context(session_id: &str) -> ToolContext {
    ToolContext {
        session_id: session_id.to_string(),
        message_id: "message".to_string(),
        tool_call_id: "tool".to_string(),
        working_dir: Some(std::env::temp_dir()),
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: crate::tool::ToolExecutionMode::Direct,
    }
}

fn bound_tool(
    session_id: &str,
) -> (
    WorkflowStateTool,
    Arc<Mutex<crate::context_controller::ContextController>>,
    ToolContext,
) {
    let controller = Arc::new(Mutex::new(
        crate::context_controller::ContextController::default(),
    ));
    let bindings: ContextControllerBindings = Arc::new(RwLock::new(HashMap::new()));
    bindings
        .write()
        .expect("bindings lock")
        .insert(session_id.to_string(), Arc::downgrade(&controller));
    (
        WorkflowStateTool::new(bindings),
        controller,
        tool_context(session_id),
    )
}

fn goal_patch(controller: &Arc<Mutex<crate::context_controller::ContextController>>) -> Value {
    let (schema, revision) = {
        let controller = controller.lock().expect("controller lock");
        (
            controller.workflow_run_state().state_schema.clone(),
            controller.workflow_run_state().revision,
        )
    };
    let mut patch = WorkflowStatePatch::new(schema, revision);
    patch.goal = Some(PatchValue::Set("bounded goal".to_string()));
    serde_json::to_value(patch).expect("patch serialization")
}

#[test]
fn schema_describes_revision_checked_patch_and_null_clear() {
    let bindings: ContextControllerBindings = Arc::new(RwLock::new(HashMap::new()));
    let schema = WorkflowStateTool::new(bindings).parameters_schema();

    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(
        schema["properties"]["action"]["enum"],
        json!([
            "get_state",
            "propose_patch",
            "record_observation",
            "retrieve_evidence",
            "reconcile",
            "commit_round"
        ])
    );
    assert_eq!(
        schema["properties"]["observation"]["required"],
        json!(["text"])
    );
    assert_eq!(
        schema["properties"]["patch"]["required"],
        json!(["patch_schema_version", "state_schema", "expected_revision"])
    );
    assert_eq!(
        schema["properties"]["patch"]["properties"]["goal"]["type"],
        json!(["string", "null"])
    );
}

#[tokio::test]
async fn get_state_is_read_only() {
    let (tool, controller, ctx) = bound_tool("state-read");
    let before = controller
        .lock()
        .expect("controller lock")
        .workflow_run_state()
        .clone();

    let result = tool
        .execute(json!({"action": "get_state"}), ctx)
        .await
        .expect("get_state should succeed");
    let metadata = result.metadata.expect("state metadata");

    assert_eq!(metadata["mutated"], json!(false));
    assert_eq!(metadata["plane"], json!("execution"));
    assert_eq!(metadata["revision"], json!(0));
    assert_eq!(metadata["state"]["revision"], json!(0));
    assert_eq!(metadata["contract"]["state_schema"], json!("default"));
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
            metadata["contract"].get(field).is_some(),
            "missing contract field {field}"
        );
    }
    assert_eq!(
        controller
            .lock()
            .expect("controller lock")
            .workflow_run_state(),
        &before
    );
}

#[tokio::test]
async fn propose_patch_updates_state_and_revision() {
    let (tool, controller, ctx) = bound_tool("state-apply");
    let result = tool
        .execute(
            json!({
                "action": "propose_patch",
                "patch": goal_patch(&controller),
            }),
            ctx,
        )
        .await
        .expect("fresh patch should apply");
    let metadata = result.metadata.expect("patch metadata");

    assert_eq!(metadata["applied"], json!(true));
    assert_eq!(metadata["previous_revision"], json!(0));
    assert_eq!(metadata["revision"], json!(1));
    assert_eq!(metadata["plane"], json!("execution"));
    assert_eq!(metadata["contract"]["state_schema"], json!("default"));
    assert_eq!(metadata["state"]["goal"], json!("bounded goal"));
    assert_eq!(
        controller
            .lock()
            .expect("controller lock")
            .workflow_run_state()
            .revision,
        WorkflowRunRevision::new(1)
    );
}

#[tokio::test]
async fn commit_round_applies_action_state_and_summary_once() {
    let (tool, controller, ctx) = bound_tool("round-commit");
    let mut state_patch = WorkflowStatePatch::new("default", WorkflowRunRevision::INITIAL);
    state_patch.phase = Some(PatchValue::Set("build".to_string()));
    state_patch.source_revision = Some(PatchValue::Set("source:v1".to_string()));

    let result = tool
        .execute(
            json!({
                "action": "commit_round",
                "round": {
                    "expected_revision": 0,
                    "action": {
                        "name": "run-checks",
                        "status": "completed",
                        "result": "checks passed"
                    },
                    "state_patch": serde_json::to_value(state_patch).expect("state patch"),
                    "summary_patch": {
                        "text": "Build phase completed.",
                        "source_revision": "source:v1"
                    }
                }
            }),
            ctx,
        )
        .await
        .expect("round should apply");
    let metadata = result.metadata.expect("round metadata");

    assert_eq!(metadata["applied"], json!(true));
    assert_eq!(metadata["revision_increment"], json!(1));
    assert_eq!(metadata["previous_revision"], json!(0));
    assert_eq!(metadata["revision"], json!(1));
    assert_eq!(metadata["summary_applied"], json!(true));
    assert_eq!(metadata["workflow_action"]["status"], json!("completed"));

    let state = controller.lock().expect("controller lock");
    let workflow = state.workflow_run_state();
    assert_eq!(workflow.phase.as_deref(), Some("build"));
    assert_eq!(
        workflow.context_summary.as_deref(),
        Some("Build phase completed.")
    );
    assert_eq!(
        workflow.summary_source_revision.as_deref(),
        Some("source:v1")
    );
    assert_eq!(workflow.last_action.as_deref(), Some("run-checks"));
    assert_eq!(workflow.last_action_status.as_deref(), Some("completed"));
    assert_eq!(
        workflow.last_action_result.as_deref(),
        Some("checks passed")
    );
    assert!(workflow.prompt_summary().contains("Build phase completed."));
    assert_eq!(workflow.revision, WorkflowRunRevision::new(1));
}

#[tokio::test]
async fn commit_round_refuses_stale_or_owned_patch_without_mutation() {
    let (tool, controller, ctx) = bound_tool("round-refusal");
    let mut initial_patch = WorkflowStatePatch::new("default", WorkflowRunRevision::INITIAL);
    initial_patch.goal = Some(PatchValue::Set("initial goal".to_string()));
    initial_patch.source_revision = Some(PatchValue::Set("source:v1".to_string()));
    tool.execute(
        json!({
            "action": "commit_round",
            "round": {
                "expected_revision": 0,
                "action": {"name": "initialize", "status": "completed"},
                "state_patch": serde_json::to_value(initial_patch).expect("state patch")
            }
        }),
        ctx.clone(),
    )
    .await
    .expect("initial round should apply");

    let before = controller
        .lock()
        .expect("controller lock")
        .workflow_run_state()
        .clone();
    let mut stale_patch = WorkflowStatePatch::new("default", WorkflowRunRevision::INITIAL);
    stale_patch.phase = Some(PatchValue::Set("stale phase".to_string()));
    let stale = tool
        .execute(
            json!({
                "action": "commit_round",
                "round": {
                    "expected_revision": 0,
                    "action": {"name": "stale", "status": "completed"},
                    "state_patch": serde_json::to_value(stale_patch).expect("state patch")
                }
            }),
            ctx.clone(),
        )
        .await
        .expect("stale round should return refusal metadata");
    let stale_metadata = stale.metadata.expect("stale refusal metadata");
    assert_eq!(stale_metadata["refused"], json!(true));
    assert_eq!(stale_metadata["mutated"], json!(false));
    assert_eq!(
        controller
            .lock()
            .expect("controller lock")
            .workflow_run_state(),
        &before
    );

    let no_revision_summary = tool
        .execute(
            json!({
                "action": "commit_round",
                "round": {
                    "expected_revision": 1,
                    "action": {"name": "unversioned-summary", "status": "completed"},
                    "summary_patch": {"text": "This summary lacks source provenance."}
                }
            }),
            ctx.clone(),
        )
        .await
        .expect("unversioned summary should return refusal metadata");
    let no_revision_metadata = no_revision_summary
        .metadata
        .expect("summary refusal metadata");
    assert_eq!(no_revision_metadata["refused"], json!(true));
    assert_eq!(no_revision_metadata["mutated"], json!(false));

    let mut owned_patch = WorkflowStatePatch::new("default", before.revision);
    owned_patch.context_summary = Some(PatchValue::Set("not allowed here".to_string()));
    let owned = tool
        .execute(
            json!({
                "action": "commit_round",
                "round": {
                    "expected_revision": 1,
                    "action": {"name": "owned", "status": "completed"},
                    "state_patch": serde_json::to_value(owned_patch).expect("state patch")
                }
            }),
            ctx,
        )
        .await
        .expect("owned-field round should return refusal metadata");
    let owned_metadata = owned.metadata.expect("owned refusal metadata");
    assert_eq!(owned_metadata["refused"], json!(true));
    assert_eq!(owned_metadata["mutated"], json!(false));
    assert_eq!(
        controller
            .lock()
            .expect("controller lock")
            .workflow_run_state(),
        &before
    );
}

#[tokio::test]
async fn commit_round_records_failed_action_with_safe_state_patch() {
    let (tool, controller, ctx) = bound_tool("round-failed-action");
    let mut state_patch = WorkflowStatePatch::new("default", WorkflowRunRevision::INITIAL);
    state_patch.blockers = Some(PatchValue::Set(vec!["check failed".to_string()]));

    tool.execute(
        json!({
            "action": "commit_round",
            "round": {
                "expected_revision": 0,
                "action": {
                    "name": "run-checks",
                    "status": "failed",
                    "result": "compiler rejected the change"
                },
                "state_patch": serde_json::to_value(state_patch).expect("state patch"),
                "summary_patch": {"text": "Checks failed; retain the safe fallback."}
            }
        }),
        ctx,
    )
    .await
    .expect("failed action round should still commit explicit safe state");

    let state = controller.lock().expect("controller lock");
    let workflow = state.workflow_run_state();
    assert_eq!(workflow.last_action_status.as_deref(), Some("failed"));
    assert_eq!(
        workflow.last_action_result.as_deref(),
        Some("compiler rejected the change")
    );
    assert_eq!(
        workflow.blockers.as_deref(),
        Some(["check failed".to_string()].as_slice())
    );
    assert!(
        workflow
            .prompt_summary()
            .contains("Checks failed; retain the safe fallback.")
    );
    assert_eq!(workflow.revision, WorkflowRunRevision::new(1));
}

#[tokio::test]
async fn concurrent_commit_rounds_allow_one_writer_and_refuse_the_other() {
    let (tool, controller, ctx) = bound_tool("round-concurrent");
    let mut first_patch = WorkflowStatePatch::new("default", WorkflowRunRevision::INITIAL);
    first_patch.phase = Some(PatchValue::Set("writer-a".to_string()));
    let mut second_patch = WorkflowStatePatch::new("default", WorkflowRunRevision::INITIAL);
    second_patch.phase = Some(PatchValue::Set("writer-b".to_string()));

    let first = tool.execute(
        json!({
            "action": "commit_round",
            "round": {
                "expected_revision": 0,
                "action": {"name": "writer-a", "status": "completed"},
                "state_patch": serde_json::to_value(first_patch).expect("state patch")
            }
        }),
        ctx.clone(),
    );
    let second = tool.execute(
        json!({
            "action": "commit_round",
            "round": {
                "expected_revision": 0,
                "action": {"name": "writer-b", "status": "completed"},
                "state_patch": serde_json::to_value(second_patch).expect("state patch")
            }
        }),
        ctx,
    );
    let (first, second) = tokio::join!(first, second);
    let first = first.expect("first concurrent result");
    let second = second.expect("second concurrent result");
    let first_applied = first.metadata.as_ref().is_some_and(|metadata| {
        metadata["applied"] == json!(true) && metadata["mutated"] == json!(true)
    });
    let second_applied = second.metadata.as_ref().is_some_and(|metadata| {
        metadata["applied"] == json!(true) && metadata["mutated"] == json!(true)
    });
    let first_refused = first.metadata.as_ref().is_some_and(|metadata| {
        metadata["refused"] == json!(true) && metadata["mutated"] == json!(false)
    });
    let second_refused = second.metadata.as_ref().is_some_and(|metadata| {
        metadata["refused"] == json!(true) && metadata["mutated"] == json!(false)
    });

    assert_eq!(first_applied as u8 + second_applied as u8, 1);
    assert_eq!(first_refused as u8 + second_refused as u8, 1);

    let state = controller.lock().expect("controller lock");
    let workflow = state.workflow_run_state();
    assert_eq!(workflow.revision, WorkflowRunRevision::new(1));
    assert!(matches!(
        workflow.phase.as_deref(),
        Some("writer-a") | Some("writer-b")
    ));
    assert_eq!(workflow.last_action_status.as_deref(), Some("completed"));
}

#[tokio::test]
async fn stale_patch_is_rejected_without_additional_mutation() {
    let (tool, controller, ctx) = bound_tool("state-stale");
    let patch = goal_patch(&controller);
    let input = json!({"action": "propose_patch", "patch": patch});

    tool.execute(input.clone(), ctx.clone())
        .await
        .expect("first patch should apply");
    let before = controller
        .lock()
        .expect("controller lock")
        .workflow_run_state()
        .clone();

    let error = tool
        .execute(input, ctx)
        .await
        .expect_err("stale patch should be rejected");

    assert!(error.to_string().contains("revision mismatch"));
    assert_eq!(
        controller
            .lock()
            .expect("controller lock")
            .workflow_run_state(),
        &before
    );
}

#[tokio::test]
async fn bindings_keep_sessions_isolated() {
    let session_a = "state-a";
    let session_b = "state-b";
    let controller_a = Arc::new(Mutex::new(
        crate::context_controller::ContextController::default(),
    ));
    let controller_b = Arc::new(Mutex::new(
        crate::context_controller::ContextController::default(),
    ));
    let bindings: ContextControllerBindings = Arc::new(RwLock::new(HashMap::new()));
    {
        let mut bindings_guard = bindings.write().expect("bindings lock");
        bindings_guard.insert(session_a.to_string(), Arc::downgrade(&controller_a));
        bindings_guard.insert(session_b.to_string(), Arc::downgrade(&controller_b));
    }
    let tool = WorkflowStateTool::new(bindings);

    tool.execute(
        json!({
            "action": "propose_patch",
            "patch": goal_patch(&controller_a),
        }),
        tool_context(session_a),
    )
    .await
    .expect("session A patch should apply");

    let result = tool
        .execute(json!({"action": "get_state"}), tool_context(session_b))
        .await
        .expect("session B state should be readable");
    let metadata = result.metadata.expect("state metadata");

    assert!(metadata["state"]["goal"].is_null());
    assert_eq!(metadata["revision"], json!(0));
    assert_eq!(
        controller_b
            .lock()
            .expect("controller lock")
            .workflow_run_state()
            .revision,
        WorkflowRunRevision::INITIAL
    );
}

#[tokio::test]
async fn record_observation_appends_evidence_and_advances_revision() {
    let (tool, controller, ctx) = bound_tool("workflow-observe");

    let result = tool
        .execute(
            json!({
                "action": "record_observation",
                "observation": {"text": "tests pass", "source": "cargo test"}
            }),
            ctx,
        )
        .await
        .expect("observation should be recorded");
    let metadata = result.metadata.expect("metadata");

    assert_eq!(metadata["mutated"], json!(true));
    assert_eq!(metadata["plane"], json!("evidence"));
    assert_eq!(metadata["revision"], json!(1));
    assert_eq!(metadata["evidence_count"], json!(1));
    let controller = controller.lock().expect("controller lock");
    let evidence = controller
        .workflow_run_state()
        .evidence_refs
        .clone()
        .unwrap_or_default();
    assert_eq!(evidence, vec!["cargo test: tests pass".to_string()]);
    assert_eq!(
        controller.workflow_run_state().last_observation.as_deref(),
        Some("tests pass")
    );
    assert_eq!(
        controller
            .workflow_run_state()
            .observation_status
            .as_deref(),
        Some(OBSERVATION_STATUS_CURRENT)
    );
}

#[tokio::test]
async fn record_observation_replaces_newer_source_and_rejects_old_or_duplicate_values() {
    let (tool, controller, _ctx) = bound_tool("workflow-freshness");

    tool.execute(
        json!({
            "action": "record_observation",
            "observation": {
                "text": "value: old",
                "source": "source",
                "source_revision": "source:v1",
                "observed_at": "2026-09-18T00:00:00Z"
            }
        }),
        tool_context("workflow-freshness"),
    )
    .await
    .expect("initial observation should apply");

    tool.execute(
        json!({
            "action": "record_observation",
            "observation": {
                "text": "value: new",
                "source": "source",
                "source_revision": "source:v2",
                "observed_at": "2026-09-18T00:01:00Z"
            }
        }),
        tool_context("workflow-freshness"),
    )
    .await
    .expect("newer observation should replace the old value");

    {
        let state = controller.lock().expect("controller lock");
        assert_eq!(
            state.workflow_run_state().source_revision.as_deref(),
            Some("source:v2")
        );
        assert_eq!(
            state.workflow_run_state().last_observation.as_deref(),
            Some("value: new")
        );
        let summary = state.workflow_run_state().prompt_summary();
        assert!(summary.contains("value: new"));
        assert!(!summary.contains("value: old"));
    }

    let duplicate = tool
        .execute(
            json!({
                "action": "record_observation",
                "observation": {
                    "text": "value: new",
                    "source": "source",
                    "source_revision": "source:v2"
                }
            }),
            tool_context("workflow-freshness"),
        )
        .await
        .expect("duplicate observation should be accepted as a no-op");
    let duplicate_metadata = duplicate.metadata.expect("duplicate metadata");
    assert_eq!(duplicate_metadata["duplicate"], json!(true));
    assert_eq!(duplicate_metadata["mutated"], json!(false));
    assert_eq!(duplicate_metadata["revision"], json!(2));

    let stale = tool
        .execute(
            json!({
                "action": "record_observation",
                "observation": {
                    "text": "value: old again",
                    "source": "source",
                    "source_revision": "source:v1"
                }
            }),
            tool_context("workflow-freshness"),
        )
        .await
        .expect("stale observation should be recorded outside current state");
    let stale_metadata = stale.metadata.expect("stale metadata");
    assert_eq!(
        stale_metadata["freshness_status"],
        json!(OBSERVATION_STATUS_STALE)
    );

    let contradiction = tool
        .execute(
            json!({
                "action": "record_observation",
                "observation": {
                    "text": "value: contradictory",
                    "source": "source",
                    "source_revision": "source:v2"
                }
            }),
            tool_context("workflow-freshness"),
        )
        .await
        .expect("contradictory observation should be recorded outside current state");
    let contradiction_metadata = contradiction.metadata.expect("contradiction metadata");
    assert_eq!(
        contradiction_metadata["freshness_status"],
        json!(OBSERVATION_STATUS_CONTRADICTED)
    );

    let state = controller.lock().expect("controller lock");
    assert_eq!(
        state.workflow_run_state().last_observation.as_deref(),
        Some("value: new")
    );
    assert_eq!(
        state.workflow_run_state().revision,
        WorkflowRunRevision::new(4)
    );
    let evidence = state
        .workflow_run_state()
        .evidence_refs
        .clone()
        .unwrap_or_default();
    assert!(evidence.iter().any(|entry| entry.contains("[stale]")));
    assert!(
        evidence
            .iter()
            .any(|entry| entry.contains("[contradicted]"))
    );
}

#[tokio::test]
async fn record_observation_rejects_empty_and_oversized_text() {
    let (tool, _controller, ctx) = bound_tool("workflow-observe-invalid");

    let error = tool
        .execute(
            json!({"action": "record_observation", "observation": {"text": "   "}}),
            ctx.clone(),
        )
        .await
        .expect_err("empty text must be rejected");
    assert!(
        error.to_string().contains("must not be empty"),
        "got: {error}"
    );

    let error = tool
        .execute(
            json!({
                "action": "record_observation",
                "observation": {"text": "x".repeat(MAX_OBSERVATION_CHARS + 1)}
            }),
            ctx,
        )
        .await
        .expect_err("oversized text must be rejected");
    assert!(error.to_string().contains("exceeds"), "got: {error}");
}

#[tokio::test]
async fn retrieve_evidence_is_read_only() {
    let (tool, controller, ctx) = bound_tool("workflow-evidence");
    {
        let mut controller = controller.lock().expect("controller lock");
        let state = controller.workflow_run_state();
        let mut patch = WorkflowStatePatch::new(state.state_schema.clone(), state.revision);
        patch.evidence_refs = Some(PatchValue::Set(vec!["file:line".to_string()]));
        patch.source_revision = Some(PatchValue::Set("abc123".to_string()));
        controller
            .apply_workflow_state_patch(&patch)
            .expect("patch should apply");
    }
    let before = controller
        .lock()
        .expect("controller lock")
        .workflow_run_state()
        .clone();

    let result = tool
        .execute(json!({"action": "retrieve_evidence"}), ctx)
        .await
        .expect("retrieve should succeed");
    let metadata = result.metadata.expect("metadata");

    assert_eq!(metadata["mutated"], json!(false));
    assert_eq!(metadata["plane"], json!("evidence"));
    assert_eq!(metadata["source_revision"], json!("abc123"));
    assert_eq!(metadata["evidence_refs"], json!(["file:line"]));
    assert_eq!(
        controller
            .lock()
            .expect("controller lock")
            .workflow_run_state(),
        &before
    );
}

#[tokio::test]
async fn reconcile_reports_differences_without_mutation() {
    let (tool, controller, ctx) = bound_tool("workflow-reconcile");
    {
        let mut controller = controller.lock().expect("controller lock");
        let state = controller.workflow_run_state();
        let mut patch = WorkflowStatePatch::new(state.state_schema.clone(), state.revision);
        patch.phase = Some(PatchValue::Set("build".to_string()));
        controller
            .apply_workflow_state_patch(&patch)
            .expect("patch should apply");
    }
    let before = controller
        .lock()
        .expect("controller lock")
        .workflow_run_state()
        .clone();

    let result = tool
        .execute(
            json!({"action": "reconcile", "expected": {"phase": "test"}}),
            ctx.clone(),
        )
        .await
        .expect("reconcile should succeed");
    let metadata = result.metadata.expect("metadata");
    assert_eq!(metadata["plane"], json!("execution"));
    assert_eq!(metadata["matches"], json!(false));
    assert_eq!(metadata["mutated"], json!(false));
    let differences = metadata["differences"].as_array().expect("differences");
    assert_eq!(differences.len(), 1);
    assert!(
        differences[0]
            .as_str()
            .unwrap_or_default()
            .contains("phase"),
        "got: {differences:?}"
    );

    let result = tool
        .execute(
            json!({"action": "reconcile", "expected": {"phase": "build"}}),
            ctx,
        )
        .await
        .expect("reconcile should succeed");
    assert_eq!(result.metadata.expect("metadata")["matches"], json!(true));
    assert_eq!(
        controller
            .lock()
            .expect("controller lock")
            .workflow_run_state(),
        &before
    );
}
