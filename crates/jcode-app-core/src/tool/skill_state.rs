use super::{
    ContextControllerBindings, Tool, ToolContext, ToolOutput, context_controller_for_session,
};
use crate::execution_state::ExecutionStatePatch;
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
        "Read or atomically patch bounded execution state."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": ["get_state", "propose_patch"],
                    "description": "Read state or propose one validated state patch.",
                },
                "patch": patch_schema(),
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
            action => anyhow::bail!("unsupported skill_state action: {action}"),
        };

        Ok(output("skill_state", metadata))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_state::{ExecutionStateRevision, PatchValue};
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
            json!(["get_state", "propose_patch"])
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
}
