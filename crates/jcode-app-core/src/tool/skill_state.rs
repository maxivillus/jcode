use super::{
    Tool, ToolContext, ToolOutput,
    context_control::{ContextControllerBindings, context_controller_for_session},
};
use crate::execution_state::{ExecutionStatePatch, PatchValue};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

/// Model surface for bounded, revision-checked execution state.
pub(crate) struct SkillStateTool {
    bindings: ContextControllerBindings,
}

impl SkillStateTool {
    pub(crate) fn new(bindings: ContextControllerBindings) -> Self {
        Self { bindings }
    }
}

#[derive(Debug, Deserialize)]
struct SkillStateInput {
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
impl Tool for SkillStateTool {
    fn name(&self) -> &str {
        "skill_state"
    }

    fn description(&self) -> &str {
        "Read, patch, observe and reconcile bounded execution state."
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
        let params: SkillStateInput = serde_json::from_value(input)?;
        let controller = context_controller_for_session(&self.bindings, &ctx.session_id)?;
        let mut controller = controller
            .lock()
            .map_err(|_| anyhow::anyhow!("context controller lock poisoned"))?;

        let metadata = match params.action.as_str() {
            "get_state" => {
                let state = controller.execution_state().clone();
                json!({
                    "action": "get_state",
                    "mutated": false,
                    "revision": state.revision,
                    "fingerprint": state.fingerprint(),
                    "state": state,
                })
            }
            "propose_patch" => {
                let patch = params
                    .patch
                    .ok_or_else(|| anyhow::anyhow!("patch is required for propose_patch"))?;
                let patch: ExecutionStatePatch = serde_json::from_value(patch)?;
                let previous_revision = controller.execution_state().revision;
                let revision = controller.apply_execution_state_patch(&patch)?;
                let state = controller.execution_state().clone();
                json!({
                    "action": "propose_patch",
                    "applied": true,
                    "mutated": true,
                    "previous_revision": previous_revision,
                    "revision": revision,
                    "fingerprint": state.fingerprint(),
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
                let entry = match observation
                    .source
                    .as_deref()
                    .map(str::trim)
                    .filter(|source| !source.is_empty())
                {
                    Some(source) => format!("{source}: {text}"),
                    None => text.to_string(),
                };
                let (schema, revision, mut evidence) = {
                    let state = controller.execution_state();
                    let evidence: Vec<String> =
                        state.evidence_refs.iter().flatten().cloned().collect();
                    (state.state_schema.clone(), state.revision, evidence)
                };
                if evidence.len() >= MAX_EVIDENCE_REFS {
                    anyhow::bail!(
                        "evidence list is full ({MAX_EVIDENCE_REFS}); reconcile it before adding more"
                    );
                }
                evidence.push(entry);
                let mut patch = ExecutionStatePatch::new(schema, revision);
                patch.evidence_refs = Some(PatchValue::Set(evidence));
                let revision = controller.apply_execution_state_patch(&patch)?;
                let state = controller.execution_state();
                json!({
                    "action": "record_observation",
                    "applied": true,
                    "mutated": true,
                    "revision": revision,
                    "evidence_count": state.evidence_refs.as_ref().map_or(0, Vec::len),
                })
            }
            "retrieve_evidence" => {
                let state = controller.execution_state();
                let evidence: Vec<String> = state.evidence_refs.iter().flatten().cloned().collect();
                json!({
                    "action": "retrieve_evidence",
                    "mutated": false,
                    "revision": state.revision,
                    "source_revision": state.source_revision,
                    "owner": state.owner,
                    "lease": state.lease,
                    "evidence_refs": evidence,
                })
            }
            "reconcile" => {
                let expected = params.expected;
                let state = controller.execution_state();
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
                    "mutated": false,
                    "revision": state.revision,
                    "matches": differences.is_empty(),
                    "differences": differences,
                })
            }
            action => anyhow::bail!("unsupported skill_state action: {action}"),
        };

        Ok(output("skill_state", metadata))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_state::{ExecutionStatePatch, ExecutionStateRevision, PatchValue};
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
        SkillStateTool,
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
            SkillStateTool::new(bindings),
            controller,
            tool_context(session_id),
        )
    }

    fn goal_patch(controller: &Arc<Mutex<crate::context_controller::ContextController>>) -> Value {
        let (schema, revision) = {
            let controller = controller.lock().expect("controller lock");
            (
                controller.execution_state().state_schema.clone(),
                controller.execution_state().revision,
            )
        };
        let mut patch = ExecutionStatePatch::new(schema, revision);
        patch.goal = Some(PatchValue::Set("bounded goal".to_string()));
        serde_json::to_value(patch).expect("patch serialization")
    }

    #[test]
    fn schema_describes_revision_checked_patch_and_null_clear() {
        let bindings: ContextControllerBindings = Arc::new(RwLock::new(HashMap::new()));
        let schema = SkillStateTool::new(bindings).parameters_schema();

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
            .execution_state()
            .clone();

        let result = tool
            .execute(json!({"action": "get_state"}), ctx)
            .await
            .expect("get_state should succeed");
        let metadata = result.metadata.expect("state metadata");

        assert_eq!(metadata["mutated"], json!(false));
        assert_eq!(metadata["revision"], json!(0));
        assert_eq!(metadata["state"]["revision"], json!(0));
        assert_eq!(
            controller
                .lock()
                .expect("controller lock")
                .execution_state(),
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
        assert_eq!(metadata["state"]["goal"], json!("bounded goal"));
        assert_eq!(
            controller
                .lock()
                .expect("controller lock")
                .execution_state()
                .revision,
            ExecutionStateRevision(1)
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
            .execution_state()
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
                .execution_state(),
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
        let tool = SkillStateTool::new(bindings);

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
                .execution_state()
                .revision,
            ExecutionStateRevision::INITIAL
        );
    }

    #[tokio::test]
    async fn record_observation_appends_evidence_and_advances_revision() {
        let (tool, controller, ctx) = bound_tool("skill-observe");

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
        assert_eq!(metadata["revision"], json!(1));
        assert_eq!(metadata["evidence_count"], json!(1));
        let controller = controller.lock().expect("controller lock");
        let evidence = controller
            .execution_state()
            .evidence_refs
            .clone()
            .unwrap_or_default();
        assert_eq!(evidence, vec!["cargo test: tests pass".to_string()]);
    }

    #[tokio::test]
    async fn record_observation_rejects_empty_and_oversized_text() {
        let (tool, _controller, ctx) = bound_tool("skill-observe-invalid");

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
        let (tool, controller, ctx) = bound_tool("skill-evidence");
        {
            let mut controller = controller.lock().expect("controller lock");
            let state = controller.execution_state();
            let mut patch = ExecutionStatePatch::new(state.state_schema.clone(), state.revision);
            patch.evidence_refs = Some(PatchValue::Set(vec!["file:line".to_string()]));
            patch.source_revision = Some(PatchValue::Set("abc123".to_string()));
            controller
                .apply_execution_state_patch(&patch)
                .expect("patch should apply");
        }
        let before = controller
            .lock()
            .expect("controller lock")
            .execution_state()
            .clone();

        let result = tool
            .execute(json!({"action": "retrieve_evidence"}), ctx)
            .await
            .expect("retrieve should succeed");
        let metadata = result.metadata.expect("metadata");

        assert_eq!(metadata["mutated"], json!(false));
        assert_eq!(metadata["source_revision"], json!("abc123"));
        assert_eq!(metadata["evidence_refs"], json!(["file:line"]));
        assert_eq!(
            controller
                .lock()
                .expect("controller lock")
                .execution_state(),
            &before
        );
    }

    #[tokio::test]
    async fn reconcile_reports_differences_without_mutation() {
        let (tool, controller, ctx) = bound_tool("skill-reconcile");
        {
            let mut controller = controller.lock().expect("controller lock");
            let state = controller.execution_state();
            let mut patch = ExecutionStatePatch::new(state.state_schema.clone(), state.revision);
            patch.phase = Some(PatchValue::Set("build".to_string()));
            controller
                .apply_execution_state_patch(&patch)
                .expect("patch should apply");
        }
        let before = controller
            .lock()
            .expect("controller lock")
            .execution_state()
            .clone();

        let result = tool
            .execute(
                json!({"action": "reconcile", "expected": {"phase": "test"}}),
                ctx.clone(),
            )
            .await
            .expect("reconcile should succeed");
        let metadata = result.metadata.expect("metadata");
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
                .execution_state(),
            &before
        );
    }
}
