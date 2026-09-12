//! Тесты инструмента `context_control`: схема, проекции и очередь заявок.

use super::*;
use crate::context::{ContextBudget, ContextComponentHashes};
use crate::context_controller::{ContextPruneLevel, ContextPruneProjection};
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

/// Кладёт снимок проекции так же, как это делает preflight агента.
fn record_projection(
    controller: &Arc<Mutex<ContextController>>,
    kind: ContextPruneKind,
    levels: Vec<ContextPruneLevel>,
    total_tokens: usize,
) {
    let mut controller = controller.lock().expect("controller lock");
    let revision = controller.manifest().revision;
    controller.record_prune_projections(
        revision,
        vec![ContextPruneProjection {
            kind,
            levels,
            overflow_levels: 0,
            overflow_items: 0,
            overflow_tokens: 0,
            total_tokens,
            source_messages: 8,
        }],
    );
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
        json!([
            "images",
            "memory-injections",
            "system-reminders",
            "tool-results",
            "turns",
            "tail"
        ])
    );
    assert_eq!(
        schema["properties"]["prune"]["properties"]["after"]["type"],
        json!("string"),
        "tail needs the id of the last message to keep"
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
    assert!(
        metadata["projection"].is_null(),
        "status must stay a summary; the projection belongs to preview"
    );

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
async fn preview_projects_what_prune_would_drop() {
    let (tool, controller, ctx) = bound_tool("context-preview-projection");
    record_projection(
        &controller,
        ContextPruneKind::Images,
        vec![
            ContextPruneLevel {
                index: 5,
                tokens: 10,
                items: 1,
                message_id: None,
                checkpoint: None,
            },
            ContextPruneLevel {
                index: 2,
                tokens: 30,
                items: 1,
                message_id: None,
                checkpoint: None,
            },
        ],
        500,
    );
    let (before_manifest, before_state) = {
        let controller = controller.lock().expect("controller lock");
        (
            controller.manifest().clone(),
            controller.execution_state().clone(),
        )
    };

    let result = tool
        .execute(
            json!({"action": "preview", "prune": {"kind": "images", "keep_recent": 1}}),
            ctx,
        )
        .await
        .expect("preview should succeed");
    let metadata = result.metadata.expect("preview metadata");

    assert_eq!(metadata["mutated"], json!(false));
    assert_eq!(metadata["queues_nothing"], json!(true));
    assert_eq!(metadata["projection"]["status"], json!("available"));
    assert_eq!(metadata["projection"]["snapshot_revision"], json!(0));
    assert_eq!(metadata["projection"]["stale"], json!(false));
    assert_eq!(
        metadata["projection"]["estimated_context_tokens"],
        json!(500)
    );
    assert_eq!(metadata["projection"]["prune"][0]["kind"], json!("images"));
    assert_eq!(metadata["projection"]["prune"][0]["total_items"], json!(2));
    assert_eq!(
        metadata["projection"]["prune"][0]["forecast"]["removable_items"],
        json!(1),
        "the forecast must show what leaves by default"
    );
    assert_eq!(
        metadata["projection"]["prune"][0]["forecast"]["removable_tokens"],
        json!(30)
    );
    assert_eq!(
        metadata["projection"]["requested"]["status"],
        json!("available")
    );
    assert_eq!(
        metadata["projection"]["requested"]["queues_nothing"],
        json!(true)
    );
    assert_eq!(
        metadata["projection"]["requested"]["forecast"]["removable_items"],
        json!(1)
    );
    assert_eq!(
        metadata["projection"]["requested"]["forecast"]["removable_tokens"],
        json!(30)
    );
    assert_eq!(
        metadata["projection"]["requested"]["forecast"]["remaining_tokens"],
        json!(470)
    );
    assert_eq!(
        metadata["projection"]["requested"]["forecast"]["tokens_exact"],
        json!(true)
    );

    let controller = controller.lock().expect("controller lock");
    assert!(
        controller.pending_actions().is_empty(),
        "preview must not queue any work"
    );
    assert_eq!(controller.manifest(), &before_manifest);
    assert_eq!(controller.execution_state(), &before_state);
}

#[tokio::test]
async fn preview_explains_missing_and_stale_snapshots() {
    let (tool, controller, _) = bound_tool("context-preview-snapshot");

    let result = tool
        .execute(
            json!({"action": "preview"}),
            tool_context("context-preview-snapshot"),
        )
        .await
        .expect("preview without a snapshot should still answer");
    let metadata = result.metadata.expect("preview metadata");
    assert_eq!(metadata["projection"]["status"], json!("not_available"));
    assert!(
        metadata["projection"]["reason"]
            .as_str()
            .expect("reason")
            .contains("snapshot"),
        "the answer must say why the projection is missing: {metadata}"
    );

    record_projection(
        &controller,
        ContextPruneKind::Turns,
        vec![ContextPruneLevel {
            index: 1,
            tokens: 40,
            items: 2,
            message_id: None,
            checkpoint: None,
        }],
        300,
    );
    {
        let mut controller = controller.lock().expect("controller lock");
        controller.update_sources(ContextComponentHashes::default(), 7);
    }

    let result = tool
        .execute(
            json!({"action": "preview"}),
            tool_context("context-preview-snapshot"),
        )
        .await
        .expect("preview with a stale snapshot should still answer");
    let metadata = result.metadata.expect("preview metadata");
    assert_eq!(metadata["projection"]["status"], json!("available"));
    assert_eq!(metadata["projection"]["stale"], json!(true));
    assert_eq!(metadata["projection"]["snapshot_revision"], json!(0));
    assert_eq!(metadata["context"]["revision"], json!(1));
}

#[tokio::test]
async fn preview_dry_run_reports_no_op_for_kept_items() {
    let (tool, controller, ctx) = bound_tool("context-preview-no-op");
    record_projection(
        &controller,
        ContextPruneKind::Images,
        vec![ContextPruneLevel {
            index: 4,
            tokens: 12,
            items: 1,
            message_id: None,
            checkpoint: None,
        }],
        200,
    );

    let result = tool
        .execute(
            json!({"action": "preview", "prune": {"kind": "images", "keep_recent": 1}}),
            ctx,
        )
        .await
        .expect("preview should succeed");
    let metadata = result.metadata.expect("preview metadata");

    assert_eq!(
        metadata["projection"]["requested"]["status"],
        json!("no_op")
    );
    assert_eq!(
        metadata["projection"]["requested"]["forecast"]["removable_items"],
        json!(0)
    );
    assert_eq!(
        metadata["projection"]["requested"]["forecast"]["no_op"],
        json!(true)
    );
}

#[tokio::test]
async fn preview_rejects_unknown_prune_kind() {
    let (tool, _controller, ctx) = bound_tool("context-preview-kind");

    let error = tool
        .execute(
            json!({"action": "preview", "prune": {"kind": "threads"}}),
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
async fn tail_prune_requires_after_and_rejects_keep_recent() {
    let (tool, _controller, ctx) = bound_tool("context-tail-validation");

    let error = tool
        .execute(
            json!({"action": "preview", "prune": {"kind": "tail"}}),
            ctx.clone(),
        )
        .await
        .expect_err("tail without after must be rejected");
    assert!(
        error.to_string().contains("requires `after`"),
        "got: {error}"
    );

    let error = tool
        .execute(
            json!({
                "action": "preview",
                "prune": {"kind": "tail", "after": "message-4", "keep_recent": 1}
            }),
            ctx,
        )
        .await
        .expect_err("tail must not accept keep_recent");
    assert!(
        error.to_string().contains("is not supported by `tail`"),
        "got: {error}"
    );
}

#[tokio::test]
async fn preview_lists_tail_cut_candidates_and_projects_the_requested_cut() {
    let (tool, controller, ctx) = bound_tool("context-tail-preview");
    record_projection(
        &controller,
        ContextPruneKind::Tail,
        vec![
            ContextPruneLevel {
                index: 3,
                tokens: 40,
                items: 2,
                message_id: Some("message-3".to_string()),
                checkpoint: Some("compaction-boundary".to_string()),
            },
            ContextPruneLevel {
                index: 2,
                tokens: 60,
                items: 3,
                message_id: Some("message-2".to_string()),
                checkpoint: None,
            },
        ],
        300,
    );

    let result = tool
        .execute(
            json!({"action": "preview", "prune": {"kind": "tail", "after": "message-2"}}),
            ctx,
        )
        .await
        .expect("preview with a tail cut should succeed");
    let metadata = result.metadata.expect("preview metadata");
    let tail = &metadata["projection"]["prune"][0];

    assert_eq!(tail["kind"], json!("tail"));
    assert_eq!(tail["selector"], json!("after"));
    assert_eq!(tail["candidates"], json!(2));
    assert_eq!(
        tail["sample_after"],
        json!(["message-3", "message-2"]),
        "the model must see which message ids it can cut after"
    );
    assert_eq!(
        tail["checkpoints"],
        json!([{"label": "compaction-boundary", "after": "message-3"}]),
        "named cuts must stay visible even when they are older than the newest candidates"
    );

    let requested = &metadata["projection"]["requested"];
    assert_eq!(requested["status"], json!("available"));
    assert_eq!(requested["queues_nothing"], json!(true));
    assert_eq!(requested["forecast"]["removable_items"], json!(3));
    assert_eq!(requested["forecast"]["removable_tokens"], json!(60));
    assert_eq!(requested["forecast"]["kept_items"], json!(5));
}

#[tokio::test]
async fn tail_prune_is_queued_with_its_cut_message() {
    let (tool, controller, ctx) = bound_tool("context-tail-queued");
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
                "prune": {"kind": "tail", "after": "message-4"}
            }),
            ctx,
        )
        .await
        .expect("fresh tail prune request is accepted");
    let metadata = result.metadata.expect("metadata");

    assert_eq!(metadata["queued"], json!(true));
    assert_eq!(metadata["request"]["prune"]["kind"], json!("tail"));
    assert_eq!(metadata["request"]["prune"]["after"], json!("message-4"));
    assert_eq!(metadata["request"]["prune"]["keep_recent"], Value::Null);
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

/// Отводит `JCODE_HOME` в отдельный каталог и возвращает его при выходе.
struct JcodeHomeGuard {
    previous: Option<std::ffi::OsString>,
}

impl JcodeHomeGuard {
    fn set(path: &std::path::Path) -> Self {
        let previous = std::env::var_os("JCODE_HOME");
        jcode_base::env::set_var("JCODE_HOME", path);
        Self { previous }
    }
}

impl Drop for JcodeHomeGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => jcode_base::env::set_var("JCODE_HOME", value),
            None => jcode_base::env::remove_var("JCODE_HOME"),
        }
    }
}

/// Имена файлов в каталоге экспорта: по ним видно незакрытые временные файлы.
fn export_dir_entries(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("read exports directory")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

#[cfg(unix)]
#[tokio::test]
async fn export_writes_a_private_redacted_file() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new().expect("create temp dir");
    let _home = JcodeHomeGuard::set(temp.path());
    let (tool, _controller, ctx) = bound_tool("context-export-file");

    let result = tool
        .execute(
            json!({"action": "export", "export": {"path": "context.json"}}),
            ctx,
        )
        .await
        .expect("export writes the manifest");
    let metadata = result.metadata.expect("export metadata");

    assert_eq!(metadata["writes_file"], json!(true));
    assert_eq!(metadata["redacted"], json!(true));
    assert_eq!(metadata["file"]["format"], json!("json"));
    assert_eq!(metadata["file"]["overwritten"], json!(false));
    assert_eq!(metadata["file"]["permissions"], json!("0600"));
    assert_eq!(metadata["file"]["external_storage"], json!(true));

    let dir = temp.path().join("exports");
    let path = metadata["file"]["path"].as_str().expect("export path");
    assert_eq!(
        std::path::Path::new(path).parent(),
        Some(dir.as_path()),
        "the file must stay inside the exports directory"
    );

    let contents = std::fs::read_to_string(path).expect("read export");
    let document: Value = serde_json::from_str(&contents).expect("export is json");
    assert_eq!(document["session_id"], json!("context-export-file"));
    assert_eq!(document["redacted"], json!(true));
    assert!(
        document["context"]["components"].is_object(),
        "the manifest keeps component hashes: {document}"
    );
    assert!(document["prune"].is_array());

    let mode = std::fs::metadata(path)
        .expect("stat export")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the export must be owner-only");
    assert_eq!(
        export_dir_entries(&dir),
        vec!["context.json".to_string()],
        "the atomic write must leave no temporary file behind"
    );
}

#[tokio::test]
async fn export_refuses_to_replace_a_file_without_confirmation() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new().expect("create temp dir");
    let _home = JcodeHomeGuard::set(temp.path());
    let (tool, _controller, ctx) = bound_tool("context-export-overwrite");

    tool.execute(
        json!({"action": "export", "export": {"path": "context.md"}}),
        ctx.clone(),
    )
    .await
    .expect("first export writes the file");

    let error = tool
        .execute(
            json!({"action": "export", "export": {"path": "context.md"}}),
            ctx.clone(),
        )
        .await
        .expect_err("an existing export must not be replaced silently");
    assert!(
        error.to_string().contains("export.overwrite=true"),
        "the error must name the confirmation: {error}"
    );

    let result = tool
        .execute(
            json!({"action": "export", "export": {"path": "context.md", "overwrite": true}}),
            ctx,
        )
        .await
        .expect("explicit overwrite is accepted");
    let metadata = result.metadata.expect("export metadata");

    assert_eq!(metadata["file"]["format"], json!("markdown"));
    assert_eq!(metadata["file"]["overwritten"], json!(true));
    let contents = std::fs::read_to_string(metadata["file"]["path"].as_str().expect("path"))
        .expect("read markdown export");
    assert!(
        contents.starts_with("# Context export"),
        "markdown export must start with its title: {contents}"
    );
    assert!(contents.contains("- revision:"), "got: {contents}");
}

#[tokio::test]
async fn export_rejects_paths_outside_the_exports_directory() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new().expect("create temp dir");
    let _home = JcodeHomeGuard::set(temp.path());
    let (tool, _controller, ctx) = bound_tool("context-export-path");

    for path in [
        "../escape.json",
        "sub/dir.json",
        ".hidden.json",
        "/tmp/escape.json",
    ] {
        let error = tool
            .execute(
                json!({"action": "export", "export": {"path": path}}),
                ctx.clone(),
            )
            .await
            .expect_err("a path outside the exports directory must be refused");
        assert!(
            error.to_string().contains("export.path"),
            "{path} must be named in the error: {error}"
        );
    }

    let error = tool
        .execute(
            json!({"action": "export", "export": {"path": "notes.txt"}}),
            ctx,
        )
        .await
        .expect_err("an unknown extension must be refused");
    assert!(error.to_string().contains(".json or .md"), "got: {error}");
    assert!(
        !temp.path().join("exports").join("notes.txt").exists(),
        "a refused export must not leave a file"
    );
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
