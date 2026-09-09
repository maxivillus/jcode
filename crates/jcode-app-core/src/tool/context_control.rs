use super::{
    ContextControllerBindings, Tool, ToolContext, ToolOutput, context_controller_for_session,
};
use crate::context_controller::ContextController;
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

/// Read-only model surface for the provider-facing context manifest.
pub(crate) struct ContextControlTool {
    bindings: ContextControllerBindings,
}

impl ContextControlTool {
    pub(crate) fn new(bindings: ContextControllerBindings) -> Self {
        Self { bindings }
    }
}

#[derive(Debug, Deserialize)]
struct ContextControlInput {
    #[serde(default = "default_action")]
    action: String,
}

fn default_action() -> String {
    "status".to_string()
}

fn context_metadata(controller: &ContextController) -> Value {
    let manifest = controller.manifest();
    json!({
        "schema_version": manifest.schema_version,
        "revision": manifest.revision,
        "provider_generation": manifest.provider_generation,
        "estimated_input_tokens": manifest.estimated_input_tokens,
        "observed_input_tokens": manifest.observed_input_tokens,
        "components": &manifest.components,
        "fingerprint": manifest.fingerprint(),
    })
}

fn preflight_metadata(controller: &ContextController) -> Value {
    match (controller.last_budget(), controller.last_plan()) {
        (Some(budget), Some(plan)) => json!({
            "status": "available",
            "budget": budget,
            "plan": plan,
            "needs_refresh": plan.needs_refresh(),
            "needs_compaction": plan.needs_compaction(),
        }),
        _ => json!({
            "status": "not_available",
            "reason": "no provider preflight has run",
        }),
    }
}

fn output(title: &str, metadata: Value) -> ToolOutput {
    let body = serde_json::to_string_pretty(&metadata).unwrap_or_else(|_| metadata.to_string());
    ToolOutput::new(body)
        .with_title(title.to_string())
        .with_metadata(metadata)
}

#[async_trait]
impl Tool for ContextControlTool {
    fn name(&self) -> &str {
        "context_control"
    }

    fn description(&self) -> &str {
        "Read-only context status and preflight metadata."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": ["status", "preview"],
                    "description": "Read-only operation to perform.",
                },
            },
            "additionalProperties": false,
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: ContextControlInput = serde_json::from_value(input)?;
        let controller = context_controller_for_session(&self.bindings, &ctx.session_id)?;
        let controller = controller
            .lock()
            .map_err(|_| anyhow::anyhow!("context controller lock poisoned"))?;

        let action = params.action.as_str();
        let metadata = match action {
            "status" => json!({
                "action": action,
                "mutated": false,
                "context": context_metadata(&controller),
                "preflight": preflight_metadata(&controller),
            }),
            "preview" => json!({
                "action": action,
                "mutated": false,
                "context": context_metadata(&controller),
                "preflight": preflight_metadata(&controller),
            }),
            _ => anyhow::bail!("unsupported context_control action: {action}"),
        };

        Ok(output("context_control", metadata))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ContextBudget, ContextComponentHashes};
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
        ContextControlTool,
        Arc<Mutex<ContextController>>,
        ToolContext,
    ) {
        let controller = Arc::new(Mutex::new(ContextController::default()));
        let bindings: ContextControllerBindings = Arc::new(RwLock::new(HashMap::new()));
        bindings
            .write()
            .expect("bindings lock")
            .insert(session_id.to_string(), Arc::downgrade(&controller));
        (
            ContextControlTool::new(bindings),
            controller,
            tool_context(session_id),
        )
    }

    #[test]
    fn schema_exposes_only_read_only_actions() {
        let bindings: ContextControllerBindings = Arc::new(RwLock::new(HashMap::new()));
        let schema = ContextControlTool::new(bindings).parameters_schema();

        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(
            schema["properties"]["action"]["enum"],
            json!(["status", "preview"])
        );
    }

    #[tokio::test]
    async fn status_is_read_only_and_reports_bounded_metadata() {
        let (tool, controller, ctx) = bound_tool("context-status");
        let (before_manifest, before_state) = {
            let controller = controller.lock().expect("controller lock");
            (
                controller.manifest().clone(),
                controller.execution_state().clone(),
            )
        };

        let result = tool
            .execute(json!({"action": "status"}), ctx)
            .await
            .expect("status should succeed");
        let metadata = result.metadata.expect("status metadata");

        assert_eq!(metadata["mutated"], json!(false));
        assert_eq!(metadata["context"]["revision"], json!(0));
        assert_eq!(metadata["preflight"]["status"], json!("not_available"));

        let controller = controller.lock().expect("controller lock");
        assert_eq!(controller.manifest(), &before_manifest);
        assert_eq!(controller.execution_state(), &before_state);
    }

    #[tokio::test]
    async fn preview_reports_last_preflight_without_mutating_state() {
        let (tool, controller, ctx) = bound_tool("context-preview");
        {
            let mut controller = controller.lock().expect("controller lock");
            controller.prepare(
                &ContextBudget {
                    provider_context_limit: 100,
                    reserved_output_tokens: 20,
                    safety_margin_tokens: 10,
                    estimated_input_tokens: 20,
                },
                ContextComponentHashes::default(),
                7,
            );
        }
        let before_state = controller
            .lock()
            .expect("controller lock")
            .execution_state()
            .clone();

        let result = tool
            .execute(json!({"action": "preview"}), ctx)
            .await
            .expect("preview should succeed");
        let metadata = result.metadata.expect("preview metadata");

        assert_eq!(metadata["mutated"], json!(false));
        assert_eq!(metadata["preflight"]["status"], json!("available"));
        assert_eq!(metadata["preflight"]["plan"]["action"], json!("refresh"));
        assert_eq!(
            controller
                .lock()
                .expect("controller lock")
                .execution_state(),
            &before_state
        );
    }

    #[tokio::test]
    async fn unknown_action_is_rejected() {
        let (tool, _controller, ctx) = bound_tool("context-invalid");

        let error = tool
            .execute(json!({"action": "compact"}), ctx)
            .await
            .expect_err("mutating action must not be exposed by this MVP");

        assert!(
            error
                .to_string()
                .contains("unsupported context_control action")
        );
    }
}
