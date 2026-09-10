use super::{Tool, ToolContext, ToolOutput, skill_state};
use crate::context::ContextRevision;
use crate::context_controller::{
    ContextActionKind, ContextController, ContextPruneKind, ContextPruneSpec,
};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock, Weak};

pub(crate) type ContextControllerHandle = Arc<StdMutex<ContextController>>;
pub(crate) type ContextControllerWeak = Weak<StdMutex<ContextController>>;
pub(crate) type ContextControllerBindings = Arc<StdRwLock<HashMap<String, ContextControllerWeak>>>;

#[derive(Clone)]
pub(crate) struct ContextTools {
    bindings: ContextControllerBindings,
}

impl ContextTools {
    pub(crate) fn new() -> Self {
        Self {
            bindings: Arc::new(StdRwLock::new(HashMap::new())),
        }
    }

    pub(crate) fn register(&self, tools: &mut HashMap<String, Arc<dyn Tool>>) {
        tools.insert(
            "context_control".into(),
            Arc::new(ContextControlTool::new(self.bindings.clone())),
        );
        tools.insert(
            "skill_state".into(),
            Arc::new(skill_state::SkillStateTool::new(self.bindings.clone())),
        );
    }

    pub(crate) fn bind(&self, session_id: &str, controller: ContextControllerWeak) {
        let Ok(mut bindings) = self.bindings.write() else {
            crate::logging::warn("Context controller bindings lock poisoned");
            return;
        };
        bindings.retain(|_, current| current.strong_count() > 0);
        bindings.insert(session_id.to_string(), controller);
    }
}

pub(crate) fn context_controller_for_session(
    bindings: &ContextControllerBindings,
    session_id: &str,
) -> Result<ContextControllerHandle> {
    let mut bindings = bindings
        .write()
        .map_err(|_| anyhow::anyhow!("context controller bindings lock poisoned"))?;
    bindings.retain(|_, controller| controller.strong_count() > 0);
    bindings
        .get(session_id)
        .and_then(Weak::upgrade)
        .ok_or_else(|| anyhow::anyhow!("context controller is not bound for this session"))
}

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
    /// Revision, которую модель увидела в status/preview. Требуется для
    /// действий, которые меняют состояние на границе turn-а.
    #[serde(default)]
    expected_revision: Option<u64>,
    /// Параметры структурной обрезки для действия `prune`.
    #[serde(default)]
    prune: Option<ContextPruneInput>,
}

#[derive(Debug, Deserialize)]
struct ContextPruneInput {
    kind: String,
    #[serde(default)]
    keep_recent: Option<usize>,
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

fn execution_state_metadata(controller: &ContextController) -> Value {
    let state = controller.execution_state();
    json!({
        "state_schema": state.state_schema,
        "schema_version": state.schema_version,
        "revision": state.revision,
    })
}

fn actions_metadata(controller: &ContextController) -> Value {
    json!({
        "pending": controller.pending_actions(),
        "last": controller.last_action(),
    })
}

fn queued_action_metadata(
    action: &str,
    controller: &ContextController,
    request: crate::context_controller::ContextActionRequest,
) -> Value {
    json!({
        "action": action,
        "mutated": false,
        "queued": true,
        "request": request,
        "current_revision": controller.manifest().revision,
        "next_step": "runtime applies queued actions on the next safe turn boundary",
        "actions": actions_metadata(controller),
    })
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
        "Read-only context status, preflight metadata and queued context actions."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": ["status", "preview", "refresh", "compact", "reset-provider", "export", "prune", "undo-prune"],
                    "description": "Operation to perform; queued actions apply on the next turn.",
                },
                "expected_revision": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Revision from status/preview; required for queued actions.",
                },
                "prune": {
                    "type": "object",
                    "description": "Prune parameters for the prune action.",
                    "required": ["kind"],
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["images", "memory-injections", "tool-results", "turns"],
                            "description": "Structural data to prune.",
                        },
                        "keep_recent": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "How many recent items to keep.",
                        },
                    },
                    "additionalProperties": false,
                },
            },
            "additionalProperties": false,
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: ContextControlInput = serde_json::from_value(input)?;
        let controller = context_controller_for_session(&self.bindings, &ctx.session_id)?;
        let mut controller = controller
            .lock()
            .map_err(|_| anyhow::anyhow!("context controller lock poisoned"))?;

        let action = params.action.as_str();
        let metadata = match action {
            "status" | "preview" => json!({
                "action": action,
                "mutated": false,
                "context": context_metadata(&controller),
                "preflight": preflight_metadata(&controller),
                "actions": actions_metadata(&controller),
            }),
            "refresh" | "compact" | "reset-provider" => {
                let expected = params.expected_revision.ok_or_else(|| {
                    anyhow::anyhow!(
                        "expected_revision is required for `{action}`; read status first"
                    )
                })?;
                let kind = match action {
                    "refresh" => ContextActionKind::Refresh,
                    "compact" => ContextActionKind::Compact,
                    _ => ContextActionKind::ResetProvider,
                };
                let request = controller
                    .request_action(kind, ContextRevision(expected))
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                queued_action_metadata(action, &controller, request)
            }
            "export" => json!({
                "action": action,
                "mutated": false,
                "writes_file": false,
                "manifest": context_metadata(&controller),
                "execution_state": execution_state_metadata(&controller),
                "actions": actions_metadata(&controller),
            }),
            "prune" | "undo-prune" => {
                let expected = params.expected_revision.ok_or_else(|| {
                    anyhow::anyhow!(
                        "expected_revision is required for `{action}`; read status first"
                    )
                })?;
                let request = if action == "prune" {
                    let input = params.prune.ok_or_else(|| {
                        anyhow::anyhow!(
                            "prune spec is required for `prune`; pass kind images|memory-injections|tool-results|turns"
                        )
                    })?;
                    let kind = match input.kind.as_str() {
                        "images" => ContextPruneKind::Images,
                        "memory-injections" => ContextPruneKind::MemoryInjections,
                        "tool-results" => ContextPruneKind::ToolResults,
                        "turns" => ContextPruneKind::Turns,
                        other => anyhow::bail!("unsupported prune kind: {other}"),
                    };
                    let mut spec = ContextPruneSpec::new(kind);
                    if let Some(keep_recent) = input.keep_recent {
                        spec = spec.keep_recent(keep_recent);
                    }
                    controller
                        .request_prune(spec, ContextRevision(expected))
                        .map_err(|error| anyhow::anyhow!("{error}"))?
                } else {
                    controller
                        .request_action(ContextActionKind::UndoPrune, ContextRevision(expected))
                        .map_err(|error| anyhow::anyhow!("{error}"))?
                };
                queued_action_metadata(action, &controller, request)
            }
            _ => anyhow::bail!("unsupported context_control action: {action}"),
        };

        Ok(output("context_control", metadata))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ContextBudget, ContextComponentHashes};
    use crate::tool::{Registry, tests::MockProvider};
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

    #[tokio::test]
    async fn registry_registers_context_tools_and_schemas() {
        let registry = Registry::new(Arc::new(MockProvider)).await;
        let definitions = registry.definitions(None).await;
        for (name, actions) in [
            (
                "context_control",
                json!([
                    "status",
                    "preview",
                    "refresh",
                    "compact",
                    "reset-provider",
                    "export",
                    "prune",
                    "undo-prune"
                ]),
            ),
            (
                "skill_state",
                json!([
                    "get_state",
                    "propose_patch",
                    "record_observation",
                    "retrieve_evidence",
                    "reconcile"
                ]),
            ),
        ] {
            let schema = &definitions
                .iter()
                .find(|definition| definition.name == name)
                .expect("tool definition")
                .input_schema;
            assert_eq!(schema["additionalProperties"], false);
            assert_eq!(schema["properties"]["action"]["enum"], actions);
            if name == "skill_state" {
                assert_eq!(
                    schema["properties"]["patch"]["properties"]["expected_revision"]["type"],
                    "integer"
                );
            }
        }
    }

    #[test]
    fn schema_exposes_read_only_and_queued_actions() {
        let bindings: ContextControllerBindings = Arc::new(RwLock::new(HashMap::new()));
        let schema = ContextControlTool::new(bindings).parameters_schema();

        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(
            schema["properties"]["action"]["enum"],
            json!([
                "status",
                "preview",
                "refresh",
                "compact",
                "reset-provider",
                "export",
                "prune",
                "undo-prune"
            ])
        );
        assert_eq!(
            schema["properties"]["expected_revision"]["type"],
            json!("integer")
        );
        assert_eq!(
            schema["properties"]["prune"]["properties"]["kind"]["enum"],
            json!(["images", "memory-injections", "tool-results", "turns"])
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
            .execute(json!({"action": "clear"}), ctx)
            .await
            .expect_err("unknown action must be rejected");

        assert!(
            error
                .to_string()
                .contains("unsupported context_control action")
        );
    }

    #[tokio::test]
    async fn queued_action_requires_expected_revision() {
        let (tool, _controller, ctx) = bound_tool("context-queued-missing");

        let error = tool
            .execute(json!({"action": "compact"}), ctx)
            .await
            .expect_err("missing revision must be rejected");

        assert!(
            error.to_string().contains("expected_revision is required"),
            "got: {error}"
        );
    }

    #[tokio::test]
    async fn stale_revision_is_rejected_for_queued_action() {
        let (tool, controller, ctx) = bound_tool("context-queued-stale");
        {
            let mut controller = controller.lock().expect("controller lock");
            controller.update_sources(ContextComponentHashes::default(), 7);
        }

        let error = tool
            .execute(json!({"action": "compact", "expected_revision": 0}), ctx)
            .await
            .expect_err("stale revision must be rejected");

        assert!(
            error.to_string().contains("context revision changed"),
            "got: {error}"
        );
        assert!(
            controller
                .lock()
                .expect("controller lock")
                .pending_actions()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn queued_action_is_idempotent_and_not_mutating() {
        let (tool, controller, ctx) = bound_tool("context-queued-ok");
        let revision = controller
            .lock()
            .expect("controller lock")
            .manifest()
            .revision;
        let before_state = controller
            .lock()
            .expect("controller lock")
            .execution_state()
            .clone();

        let input = json!({"action": "reset-provider", "expected_revision": revision.0});
        let first = tool
            .execute(input.clone(), ctx.clone())
            .await
            .expect("fresh revision is accepted");
        let second = tool
            .execute(input, ctx)
            .await
            .expect("repeat stays accepted");

        let first = first.metadata.expect("metadata");
        let second = second.metadata.expect("metadata");
        assert_eq!(first["queued"], json!(true));
        assert_eq!(first["mutated"], json!(false));
        assert_eq!(first["request"]["sequence"], json!(1));
        assert_eq!(second["request"], first["request"]);

        let controller = controller.lock().expect("controller lock");
        assert_eq!(controller.pending_actions().len(), 1);
        assert_eq!(controller.execution_state(), &before_state);
    }

    #[tokio::test]
    async fn export_returns_redacted_manifest_without_file() {
        let (tool, controller, ctx) = bound_tool("context-export");
        {
            let mut controller = controller.lock().expect("controller lock");
            controller.update_sources(ContextComponentHashes::default(), 11);
            let revision = controller.manifest().revision;
            let request = controller
                .request_action(ContextActionKind::Compact, revision)
                .expect("request accepted");
            controller.take_pending_actions();
            controller.record_action_outcome(
                request,
                crate::context_controller::ContextActionOutcome::Skipped {
                    reason: "nothing to compact".to_string(),
                },
            );
        }

        let result = tool
            .execute(json!({"action": "export"}), ctx)
            .await
            .expect("export should succeed");
        let metadata = result.metadata.expect("export metadata");

        assert_eq!(metadata["writes_file"], json!(false));
        assert_eq!(metadata["manifest"]["revision"], json!(1));
        assert_eq!(metadata["manifest"]["provider_generation"], json!(11));
        assert!(metadata["manifest"]["fingerprint"].is_string());
        assert_eq!(
            metadata["actions"]["last"]["outcome"]["status"],
            json!("skipped")
        );
        assert_eq!(
            metadata["actions"]["last"]["request"]["action"],
            json!("compact")
        );
        assert!(metadata["manifest"]["messages"].is_null());
        assert!(metadata["manifest"]["transcript"].is_null());
    }

    #[tokio::test]
    async fn status_reports_pending_actions() {
        let (tool, controller, ctx) = bound_tool("context-status-actions");
        let revision = controller
            .lock()
            .expect("controller lock")
            .manifest()
            .revision;
        controller
            .lock()
            .expect("controller lock")
            .request_action(ContextActionKind::Refresh, revision)
            .expect("request accepted");

        let result = tool
            .execute(json!({"action": "status"}), ctx)
            .await
            .expect("status should succeed");
        let metadata = result.metadata.expect("status metadata");

        assert_eq!(
            metadata["actions"]["pending"][0]["action"],
            json!("refresh")
        );
        assert_eq!(metadata["actions"]["pending"][0]["sequence"], json!(1));
        assert!(metadata["actions"]["last"].is_null());
    }

    #[tokio::test]
    async fn prune_requires_spec_and_known_kind() {
        let (tool, controller, ctx) = bound_tool("context-prune-validation");
        let revision = controller
            .lock()
            .expect("controller lock")
            .manifest()
            .revision
            .0;

        let error = tool
            .execute(
                json!({"action": "prune", "expected_revision": revision}),
                ctx.clone(),
            )
            .await
            .expect_err("missing prune spec must be rejected");
        assert!(
            error.to_string().contains("prune spec is required"),
            "got: {error}"
        );

        let error = tool
            .execute(
                json!({
                    "action": "prune",
                    "expected_revision": revision,
                    "prune": {"kind": "threads"}
                }),
                ctx,
            )
            .await
            .expect_err("unknown prune kind must be rejected");
        assert!(
            error.to_string().contains("unsupported prune kind"),
            "got: {error}"
        );
    }

    #[tokio::test]
    async fn prune_spec_is_queued_with_keep_recent() {
        let (tool, controller, ctx) = bound_tool("context-prune-queued");
        let revision = controller
            .lock()
            .expect("controller lock")
            .manifest()
            .revision
            .0;

        let result = tool
            .execute(
                json!({
                    "action": "prune",
                    "expected_revision": revision,
                    "prune": {"kind": "tool-results", "keep_recent": 3}
                }),
                ctx,
            )
            .await
            .expect("fresh prune request is accepted");
        let metadata = result.metadata.expect("metadata");

        assert_eq!(metadata["queued"], json!(true));
        assert_eq!(metadata["mutated"], json!(false));
        assert_eq!(metadata["request"]["action"], json!("prune"));
        assert_eq!(metadata["request"]["prune"]["kind"], json!("tool-results"));
        assert_eq!(metadata["request"]["prune"]["keep_recent"], json!(3));
    }

    #[tokio::test]
    async fn undo_prune_is_queued() {
        let (tool, controller, ctx) = bound_tool("context-undo-prune");
        let revision = controller
            .lock()
            .expect("controller lock")
            .manifest()
            .revision
            .0;

        let result = tool
            .execute(
                json!({"action": "undo-prune", "expected_revision": revision}),
                ctx,
            )
            .await
            .expect("fresh undo request is accepted");
        let metadata = result.metadata.expect("metadata");

        assert_eq!(metadata["queued"], json!(true));
        assert_eq!(metadata["request"]["action"], json!("undo-prune"));
    }
}
