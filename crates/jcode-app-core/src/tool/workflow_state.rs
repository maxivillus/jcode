use super::{
    Tool, ToolContext, ToolOutput,
    context_control::{ContextControllerBindings, context_controller_for_session},
};
use crate::context::ContextPlane;
use crate::execution_state::{
    MAX_WORKFLOW_OBSERVATION_REVISION_CHARS, MAX_WORKFLOW_OBSERVATION_SOURCE_CHARS,
    MAX_WORKFLOW_OBSERVED_AT_CHARS, OBSERVATION_STATUS_CONTRADICTED, OBSERVATION_STATUS_CURRENT,
    OBSERVATION_STATUS_STALE, PatchValue, WorkflowRunState, WorkflowStatePatch,
};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

/// Model surface for bounded, revision-checked workflow run state.
pub(crate) struct WorkflowStateTool {
    bindings: ContextControllerBindings,
}

impl WorkflowStateTool {
    pub(crate) fn new(bindings: ContextControllerBindings) -> Self {
        Self { bindings }
    }
}

#[derive(Debug, Deserialize)]
struct WorkflowStateInput {
    #[serde(default = "default_action")]
    action: String,
    #[serde(default)]
    patch: Option<Value>,
    #[serde(default)]
    observation: Option<ObservationInput>,
    #[serde(default)]
    expected: ReconcileExpectation,
}

/// Наблюдение из актуального источника, добавляемое в evidence.
#[derive(Debug, Deserialize)]
struct ObservationInput {
    text: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    source_revision: Option<String>,
    #[serde(default)]
    observed_at: Option<String>,
}

/// Ожидаемые значения для сверки без мутации состояния.
#[derive(Debug, Default, Deserialize)]
struct ReconcileExpectation {
    #[serde(default)]
    phase: Option<String>,
    #[serde(default)]
    next_action: Option<String>,
    #[serde(default)]
    source_revision: Option<String>,
}

const MAX_OBSERVATION_CHARS: usize = 500;
const MAX_EVIDENCE_REFS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObservationDisposition {
    Current,
    Duplicate,
    Stale,
    Contradicted,
}

fn trailing_revision_number(value: &str) -> Option<u64> {
    let start = value
        .char_indices()
        .rev()
        .find(|(_, character)| !character.is_ascii_digit())
        .map_or(0, |(index, _)| index + 1);
    value.get(start..)?.parse().ok()
}

fn classify_observation(
    state: &WorkflowRunState,
    source: Option<&str>,
    source_revision: Option<&str>,
    text: &str,
) -> ObservationDisposition {
    let Some(previous_text) = state.last_observation.as_deref() else {
        return ObservationDisposition::Current;
    };

    if state.observation_source.as_deref() != source {
        return ObservationDisposition::Current;
    }

    if state.last_observation_revision.as_deref() == source_revision {
        return if previous_text == text
            && state.observation_status.as_deref() == Some(OBSERVATION_STATUS_CURRENT)
        {
            ObservationDisposition::Duplicate
        } else {
            ObservationDisposition::Contradicted
        };
    }

    match (
        state
            .last_observation_revision
            .as_deref()
            .and_then(trailing_revision_number),
        source_revision.and_then(trailing_revision_number),
    ) {
        (Some(previous), Some(incoming)) if incoming < previous => ObservationDisposition::Stale,
        (Some(previous), Some(incoming)) if incoming == previous => {
            ObservationDisposition::Contradicted
        }
        (Some(_), None) => ObservationDisposition::Stale,
        _ => ObservationDisposition::Current,
    }
}

fn format_observation_evidence(
    source: Option<&str>,
    source_revision: Option<&str>,
    status: &str,
    text: &str,
) -> String {
    if source_revision.is_none() && status == OBSERVATION_STATUS_CURRENT {
        return source.map_or_else(|| text.to_string(), |value| format!("{value}: {text}"));
    }

    let source = source.unwrap_or("observation");
    let revision = source_revision.map_or_else(String::new, |value| format!("#{value}"));
    format!("{source}{revision} [{status}]: {text}")
}

fn default_action() -> String {
    "get_state".to_string()
}

fn nullable_text_schema(description: &str) -> Value {
    json!({
        "type": ["string", "null"],
        "description": description,
    })
}

fn nullable_list_schema(description: &str) -> Value {
    json!({
        "type": ["array", "null"],
        "items": {"type": "string"},
        "description": description,
    })
}

fn patch_schema() -> Value {
    json!({
        "type": "object",
        "description": "Required for propose_patch. JSON null clears a patchable field.",
        "required": ["patch_schema_version", "state_schema", "expected_revision"],
        "properties": {
            "patch_schema_version": {"type": "integer", "minimum": 1},
            "state_schema": {"type": "string"},
            "expected_revision": {"type": "integer", "minimum": 0},
            "goal": nullable_text_schema("New goal, or null to clear it."),
            "acceptance_criteria": nullable_list_schema("Acceptance criteria, or null to clear them."),
            "phase": nullable_text_schema("Current phase, or null to clear it."),
            "completed": nullable_list_schema("Completed items, or null to clear them."),
            "pending": nullable_list_schema("Pending items, or null to clear them."),
            "decisions": nullable_list_schema("Decisions, or null to clear them."),
            "changed_files": nullable_list_schema("Changed file paths, or null to clear them."),
            "tests": nullable_list_schema("Checks and tests, or null to clear them."),
            "blockers": nullable_list_schema("Blockers, or null to clear them."),
            "next_action": nullable_text_schema("Next action, or null to clear it."),
            "source_revision": nullable_text_schema("Source revision, or null to clear it."),
            "last_observation": nullable_text_schema("Last observation value, or null to clear it."),
            "observation_source": nullable_text_schema("Observation source, or null to clear it."),
            "last_observation_revision": nullable_text_schema("Revision attached to the last observation, or null to clear it."),
            "observation_status": nullable_text_schema("Observation status: current, stale, or contradicted."),
            "observed_at": nullable_text_schema("Observation timestamp, or null to clear it."),
            "evidence_refs": nullable_list_schema("Bounded evidence references, or null to clear them."),
            "owner": nullable_text_schema("State owner, or null to clear it."),
            "lease": nullable_text_schema("Lease identifier, or null to clear it."),
        },
        "additionalProperties": false,
    })
}

fn output(title: &str, metadata: Value) -> ToolOutput {
    let body = serde_json::to_string_pretty(&metadata).unwrap_or_else(|_| metadata.to_string());
    ToolOutput::new(body)
        .with_title(title.to_string())
        .with_metadata(metadata)
}

#[async_trait]
impl Tool for WorkflowStateTool {
    fn name(&self) -> &str {
        "workflow_state"
    }

    fn description(&self) -> &str {
        "Read, patch, observe and reconcile bounded workflow run state."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": [
                        "get_state",
                        "propose_patch",
                        "record_observation",
                        "retrieve_evidence",
                        "reconcile"
                    ],
                    "description": "Read state, patch it, record an observation, list evidence or reconcile expectations.",
                },
                "patch": patch_schema(),
                "observation": {
                    "type": "object",
                    "required": ["text"],
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "Observed fact taken from a current source.",
                        },
                        "source": {
                            "type": "string",
                            "description": "Where the observation came from.",
                        },
                        "source_revision": nullable_text_schema("Opaque source revision used for freshness checks."),
                        "observed_at": nullable_text_schema("Observation timestamp when available."),
                    },
                    "additionalProperties": false,
                },
                "expected": {
                    "type": "object",
                    "properties": {
                        "phase": nullable_text_schema("Expected phase."),
                        "next_action": nullable_text_schema("Expected next action."),
                        "source_revision": nullable_text_schema("Expected source revision."),
                    },
                    "additionalProperties": false,
                },
            },
            "additionalProperties": false,
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: WorkflowStateInput = serde_json::from_value(input)?;
        let controller = context_controller_for_session(&self.bindings, &ctx.session_id)?;
        let mut controller = controller
            .lock()
            .map_err(|_| anyhow::anyhow!("context controller lock poisoned"))?;

        let metadata = match params.action.as_str() {
            "get_state" => {
                let state = controller.workflow_run_state().clone();
                json!({
                    "action": "get_state",
                    "plane": ContextPlane::Execution,
                    "mutated": false,
                    "revision": state.revision,
                    "fingerprint": state.fingerprint(),
                    "contract": state.contract(),
                    "state": state,
                })
            }
            "propose_patch" => {
                let patch = params
                    .patch
                    .ok_or_else(|| anyhow::anyhow!("patch is required for propose_patch"))?;
                let patch: WorkflowStatePatch = serde_json::from_value(patch)?;
                let previous_revision = controller.workflow_run_state().revision;
                let revision = controller.apply_workflow_state_patch(&patch)?;
                let state = controller.workflow_run_state().clone();
                json!({
                    "action": "propose_patch",
                    "plane": ContextPlane::Execution,
                    "applied": true,
                    "mutated": true,
                    "previous_revision": previous_revision,
                    "revision": revision,
                    "fingerprint": state.fingerprint(),
                    "contract": state.contract(),
                    "state": state,
                })
            }
            "record_observation" => {
                let observation = params.observation.ok_or_else(|| {
                    anyhow::anyhow!("observation is required for record_observation")
                })?;
                let text = observation.text.trim();
                if text.is_empty() {
                    anyhow::bail!("observation text must not be empty");
                }
                if text.chars().count() > MAX_OBSERVATION_CHARS {
                    anyhow::bail!("observation text exceeds {MAX_OBSERVATION_CHARS} characters");
                }
                let source = observation
                    .source
                    .as_deref()
                    .map(str::trim)
                    .filter(|source| !source.is_empty());
                if source.is_some_and(|value| {
                    value.chars().count() > MAX_WORKFLOW_OBSERVATION_SOURCE_CHARS
                }) {
                    anyhow::bail!(
                        "observation source exceeds {MAX_WORKFLOW_OBSERVATION_SOURCE_CHARS} characters"
                    );
                }
                let source_revision = observation
                    .source_revision
                    .as_deref()
                    .map(str::trim)
                    .filter(|revision| !revision.is_empty());
                if source_revision.is_some_and(|value| {
                    value.chars().count() > MAX_WORKFLOW_OBSERVATION_REVISION_CHARS
                }) {
                    anyhow::bail!(
                        "observation source revision exceeds {MAX_WORKFLOW_OBSERVATION_REVISION_CHARS} characters"
                    );
                }
                let observed_at = observation
                    .observed_at
                    .as_deref()
                    .map(str::trim)
                    .filter(|timestamp| !timestamp.is_empty());
                if observed_at
                    .is_some_and(|value| value.chars().count() > MAX_WORKFLOW_OBSERVED_AT_CHARS)
                {
                    anyhow::bail!(
                        "observation timestamp exceeds {MAX_WORKFLOW_OBSERVED_AT_CHARS} characters"
                    );
                }

                let disposition = classify_observation(
                    controller.workflow_run_state(),
                    source,
                    source_revision,
                    text,
                );
                let status = match disposition {
                    ObservationDisposition::Current | ObservationDisposition::Duplicate => {
                        OBSERVATION_STATUS_CURRENT
                    }
                    ObservationDisposition::Stale => OBSERVATION_STATUS_STALE,
                    ObservationDisposition::Contradicted => OBSERVATION_STATUS_CONTRADICTED,
                };
                let entry = format_observation_evidence(source, source_revision, status, text);
                let (schema, revision, mut evidence) = {
                    let state = controller.workflow_run_state();
                    let evidence: Vec<String> =
                        state.evidence_refs.iter().flatten().cloned().collect();
                    (state.state_schema.clone(), state.revision, evidence)
                };

                if disposition == ObservationDisposition::Duplicate {
                    let state = controller.workflow_run_state();
                    return Ok(output(
                        "workflow_state",
                        json!({
                            "action": "record_observation",
                            "plane": ContextPlane::Evidence,
                            "applied": false,
                            "mutated": false,
                            "duplicate": true,
                            "freshness_status": OBSERVATION_STATUS_CURRENT,
                            "revision": state.revision,
                            "evidence_count": state.evidence_refs.as_ref().map_or(0, Vec::len),
                        }),
                    ));
                }

                if evidence.len() >= MAX_EVIDENCE_REFS {
                    anyhow::bail!(
                        "evidence list is full ({MAX_EVIDENCE_REFS}); reconcile it before adding more"
                    );
                }
                evidence.push(entry);
                let mut patch = WorkflowStatePatch::new(schema, revision);
                patch.evidence_refs = Some(PatchValue::Set(evidence));
                if disposition == ObservationDisposition::Current {
                    patch.last_observation = Some(PatchValue::Set(text.to_string()));
                    patch.observation_source = Some(match source {
                        Some(value) => PatchValue::Set(value.to_string()),
                        None => PatchValue::Clear,
                    });
                    patch.last_observation_revision = Some(match source_revision {
                        Some(value) => PatchValue::Set(value.to_string()),
                        None => PatchValue::Clear,
                    });
                    patch.observation_status =
                        Some(PatchValue::Set(OBSERVATION_STATUS_CURRENT.to_string()));
                    patch.observed_at = Some(match observed_at {
                        Some(value) => PatchValue::Set(value.to_string()),
                        None => PatchValue::Clear,
                    });
                    if let Some(source_revision) = source_revision {
                        patch.source_revision = Some(PatchValue::Set(source_revision.to_string()));
                    }
                }
                let revision = controller.apply_workflow_state_patch(&patch)?;
                let state = controller.workflow_run_state();
                json!({
                    "action": "record_observation",
                    "plane": ContextPlane::Evidence,
                    "applied": true,
                    "mutated": true,
                    "freshness_status": status,
                    "duplicate": false,
                    "revision": revision,
                    "evidence_count": state.evidence_refs.as_ref().map_or(0, Vec::len),
                })
            }
            "retrieve_evidence" => {
                let state = controller.workflow_run_state();
                let evidence: Vec<String> = state.evidence_refs.iter().flatten().cloned().collect();
                json!({
                    "action": "retrieve_evidence",
                    "plane": ContextPlane::Evidence,
                    "mutated": false,
                    "revision": state.revision,
                    "source_revision": state.source_revision,
                    "last_observation": state.last_observation,
                    "observation_source": state.observation_source,
                    "last_observation_revision": state.last_observation_revision,
                    "observation_status": state.observation_status,
                    "observed_at": state.observed_at,
                    "owner": state.owner,
                    "lease": state.lease,
                    "evidence_refs": evidence,
                })
            }
            "reconcile" => {
                let expected = params.expected;
                let state = controller.workflow_run_state();
                let mut differences: Vec<String> = Vec::new();
                if let Some(phase) = expected.phase.as_deref()
                    && state.phase.as_deref() != Some(phase)
                {
                    differences.push(format!(
                        "phase: expected {phase:?}, actual {:?}",
                        state.phase
                    ));
                }
                if let Some(next_action) = expected.next_action.as_deref()
                    && state.next_action.as_deref() != Some(next_action)
                {
                    differences.push(format!(
                        "next_action: expected {next_action:?}, actual {:?}",
                        state.next_action
                    ));
                }
                if let Some(source_revision) = expected.source_revision.as_deref()
                    && state.source_revision.as_deref() != Some(source_revision)
                {
                    differences.push(format!(
                        "source_revision: expected {source_revision:?}, actual {:?}",
                        state.source_revision
                    ));
                }
                json!({
                    "action": "reconcile",
                    "plane": ContextPlane::Execution,
                    "mutated": false,
                    "revision": state.revision,
                    "matches": differences.is_empty(),
                    "differences": differences,
                })
            }
            action => anyhow::bail!("unsupported workflow_state action: {action}"),
        };

        Ok(output("workflow_state", metadata))
    }
}

#[cfg(test)]
mod tests {
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
                "reconcile"
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
}
