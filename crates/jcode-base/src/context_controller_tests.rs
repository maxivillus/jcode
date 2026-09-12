//! Тесты контроллера контекста: revision, preflight, обрезка и хвостовые срезы.

use super::*;
use crate::execution_state::{ExecutionStateError, ExecutionStateRevision, PatchValue};

fn components(value: &str) -> ContextComponentHashes {
    ContextComponentHashes::from_texts(Some(value), None, None, None, None, None)
}

fn budget(estimated_input_tokens: usize) -> ContextBudget {
    ContextBudget {
        provider_context_limit: 100,
        reserved_output_tokens: 20,
        safety_margin_tokens: 10,
        estimated_input_tokens,
    }
}

fn state_patch(controller: &ContextController) -> ExecutionStatePatch {
    let state = controller.execution_state();
    let mut patch = ExecutionStatePatch::new(state.state_schema.clone(), state.revision);
    patch.phase = Some(PatchValue::Set("preflight".to_string()));
    patch
}

#[test]
fn controller_owns_valid_default_execution_state() {
    let controller = ContextController::default();

    assert_eq!(
        controller.execution_state().revision,
        ExecutionStateRevision::INITIAL
    );
    assert!(controller.execution_state().validate().is_ok());
}

#[test]
fn preview_does_not_mutate_controller_state() {
    let controller = ContextController::default();
    let before = controller.execution_state().clone();

    let preview = controller
        .preview_execution_state_patch(&state_patch(&controller))
        .expect("preview should accept a fresh patch");

    assert_eq!(controller.execution_state(), &before);
    assert_eq!(preview.phase.as_deref(), Some("preflight"));
    assert_eq!(preview.revision, ExecutionStateRevision(1));
}

#[test]
fn controller_applies_state_patch_and_returns_new_revision() {
    let mut controller = ContextController::default();

    let revision = controller
        .apply_execution_state_patch(&state_patch(&controller))
        .expect("controller should apply a fresh patch");

    assert_eq!(revision, ExecutionStateRevision(1));
    assert_eq!(
        controller.execution_state().phase.as_deref(),
        Some("preflight")
    );
}

#[test]
fn stale_controller_patch_is_rejected_atomically() {
    let mut controller = ContextController::default();
    let stale_patch = state_patch(&controller);
    controller
        .apply_execution_state_patch(&stale_patch)
        .expect("first patch should apply");
    let before = controller.execution_state().clone();

    let error = controller
        .apply_execution_state_patch(&stale_patch)
        .expect_err("stale patch should be rejected");

    assert!(matches!(
        error,
        ExecutionStateError::RevisionMismatch { .. }
    ));
    assert_eq!(controller.execution_state(), &before);
}

#[test]
fn injected_execution_state_is_validated_before_ownership() {
    let mut invalid = ExecutionState::default();
    invalid.schema_version += 1;

    let error = ContextController::new_with_execution_state(ContextManifest::default(), invalid)
        .expect_err("invalid injected state should be rejected");

    assert!(matches!(
        error,
        ExecutionStateError::UnsupportedStateSchemaVersion { .. }
    ));
}

#[test]
fn fresh_budget_allows_send() {
    let mut controller = ContextController::default();
    controller.update_sources(components("same"), 1);

    let plan = controller.plan(&budget(50), &components("same"), 1);

    assert_eq!(plan.action, ContextPreflightAction::Send);
    assert!(!plan.needs_refresh());
    assert!(!plan.needs_compaction());
}

#[test]
fn stale_sources_require_refresh_before_send() {
    let mut controller = ContextController::default();
    controller.update_sources(components("old"), 1);

    let plan = controller.plan(&budget(50), &components("new"), 1);

    assert_eq!(plan.action, ContextPreflightAction::Refresh);
    assert!(plan.needs_refresh());
    assert!(!plan.needs_compaction());
}

#[test]
fn oversized_request_requires_compaction() {
    let mut controller = ContextController::default();
    controller.update_sources(components("same"), 1);

    let plan = controller.plan(&budget(71), &components("same"), 1);

    assert_eq!(plan.action, ContextPreflightAction::Compact);
    assert!(!plan.needs_refresh());
    assert!(plan.needs_compaction());
}

#[test]
fn stale_oversized_request_requires_both_steps() {
    let controller = ContextController::default();

    let plan = controller.plan(&budget(71), &components("current"), 1);

    assert_eq!(plan.action, ContextPreflightAction::RefreshThenCompact);
    assert!(plan.needs_refresh());
    assert!(plan.needs_compaction());
}

#[test]
fn late_usage_from_old_revision_is_rejected() {
    let mut controller = ContextController::default();
    controller.update_sources(components("first"), 1);
    let old_revision = controller.manifest().revision;
    controller.update_sources(components("second"), 1);

    assert!(!controller.record_observed_input_tokens(old_revision, 10));
    assert!(controller.record_observed_input_tokens(controller.manifest().revision, 20));
    assert_eq!(controller.manifest().observed_input_tokens, Some(20));
}

#[test]
fn prepare_refreshes_revision_and_records_estimate() {
    let mut controller = ContextController::default();
    let plan = controller.prepare(&budget(71), components("current"), 1);

    assert_eq!(plan.action, ContextPreflightAction::RefreshThenCompact);
    assert_eq!(plan.revision, ContextRevision(1));
    assert_eq!(controller.manifest().estimated_input_tokens, 71);
    assert!(controller.manifest().is_fresh(&components("current"), 1));
}

#[test]
fn action_request_requires_current_revision() {
    let mut controller = ContextController::default();
    controller.update_sources(components("first"), 1);
    let current = controller.manifest().revision;

    let error = controller
        .request_action(ContextActionKind::Compact, ContextRevision::INITIAL)
        .expect_err("stale revision must be rejected");

    assert_eq!(
        error,
        ContextActionError::StaleRevision {
            expected: ContextRevision::INITIAL,
            current,
        }
    );
    assert!(controller.pending_actions().is_empty());

    let request = controller
        .request_action(ContextActionKind::Compact, current)
        .expect("current revision is accepted");
    assert_eq!(request.base_revision, current);
    assert_eq!(request.sequence, 1);
}

#[test]
fn repeated_action_request_is_idempotent_while_pending() {
    let mut controller = ContextController::default();
    let revision = controller.manifest().revision;

    let first = controller
        .request_action(ContextActionKind::ResetProvider, revision)
        .expect("first request is accepted");
    let second = controller
        .request_action(ContextActionKind::ResetProvider, revision)
        .expect("repeat is accepted idempotently");

    assert_eq!(first, second);
    assert_eq!(controller.pending_actions().len(), 1);
}

#[test]
fn refresh_keeps_pending_action_across_revision_change() {
    let mut controller = ContextController::default();
    let request = controller
        .request_action(ContextActionKind::Refresh, ContextRevision::INITIAL)
        .expect("request is accepted");

    controller.prepare(&budget(50), components("current"), 1);

    assert_eq!(controller.pending_actions().len(), 1);
    assert_eq!(controller.pending_actions()[0], request);
    assert!(controller.manifest().revision.0 > request.base_revision.0);
}

#[test]
fn take_pending_actions_clears_queue_and_keeps_outcome() {
    let mut controller = ContextController::default();
    let request = controller
        .request_action(ContextActionKind::Export, ContextRevision::INITIAL)
        .expect("request is accepted");

    let taken = controller.take_pending_actions();
    assert_eq!(taken, vec![request.clone()]);
    assert!(controller.pending_actions().is_empty());

    controller.record_action_outcome(
        request.clone(),
        ContextActionOutcome::Completed {
            detail: "exported".to_string(),
        },
    );
    let record = controller.last_action().expect("outcome is retained");
    assert_eq!(record.request, request);
    assert_eq!(record.applied_revision, controller.manifest().revision);
}

fn level(index: usize, tokens: usize) -> ContextPruneLevel {
    ContextPruneLevel {
        index,
        tokens,
        items: 1,
        message_id: None,
        checkpoint: None,
    }
}

/// Уровень turn-группы: одно удаление забирает несколько сообщений.
fn group(index: usize, tokens: usize, items: usize) -> ContextPruneLevel {
    ContextPruneLevel {
        index,
        tokens,
        items,
        message_id: None,
        checkpoint: None,
    }
}

/// Точка среза хвоста: `index` — последнее сохраняемое сообщение.
fn tail_cut(index: usize, tokens: usize, items: usize, message_id: &str) -> ContextPruneLevel {
    ContextPruneLevel {
        index,
        tokens,
        items,
        message_id: Some(message_id.to_string()),
        checkpoint: None,
    }
}

fn projection(
    kind: ContextPruneKind,
    levels: Vec<ContextPruneLevel>,
    overflow_levels: usize,
    overflow_items: usize,
    overflow_tokens: usize,
    total_tokens: usize,
) -> ContextPruneProjection {
    ContextPruneProjection {
        kind,
        levels,
        overflow_levels,
        overflow_items,
        overflow_tokens,
        total_tokens,
        source_messages: 7,
    }
}

#[test]
fn prune_defaults_are_shared_with_the_apply_path() {
    assert_eq!(ContextPruneKind::Images.default_keep_recent(), 1);
    assert_eq!(ContextPruneKind::MemoryInjections.default_keep_recent(), 1);
    assert_eq!(ContextPruneKind::SystemReminders.default_keep_recent(), 1);
    assert_eq!(ContextPruneKind::ToolResults.default_keep_recent(), 2);
    assert_eq!(ContextPruneKind::Turns.default_keep_recent(), 6);
    assert_eq!(
        ContextPruneKind::Tail.default_keep_recent(),
        0,
        "tail has no keep_recent: its cut comes from after"
    );
}

#[test]
fn prune_keep_recent_floors_keep_the_transcript_usable() {
    assert_eq!(ContextPruneKind::Turns.min_keep_recent(), 1);
    assert_eq!(ContextPruneKind::Images.min_keep_recent(), 0);
    assert_eq!(ContextPruneKind::MemoryInjections.min_keep_recent(), 0);
    assert_eq!(ContextPruneKind::SystemReminders.min_keep_recent(), 0);
    assert_eq!(ContextPruneKind::ToolResults.min_keep_recent(), 0);
    assert_eq!(ContextPruneKind::Tail.min_keep_recent(), 0);
}

#[test]
fn tail_checkpoint_samples_name_only_marked_cuts() {
    let projection = projection(
        ContextPruneKind::Tail,
        vec![
            tail_cut(9, 10, 1, "message-9"),
            ContextPruneLevel {
                checkpoint: Some("compaction-boundary".to_string()),
                ..tail_cut(4, 20, 2, "message-4")
            },
        ],
        0,
        0,
        0,
        100,
    );

    assert_eq!(
        projection.tail_checkpoint_samples(4),
        vec![("compaction-boundary", "message-4")]
    );
    assert_eq!(
        projection.tail_cut_samples(4),
        vec!["message-9", "message-4"],
        "a named cut must stay an ordinary cut point for the apply path"
    );
}

#[test]
fn forecast_keeps_newest_levels_and_reports_savings() {
    let projection = projection(
        ContextPruneKind::Images,
        vec![level(5, 10), level(6, 20), level(7, 30)],
        0,
        0,
        0,
        100,
    );

    let forecast = projection.forecast(1);

    assert_eq!(forecast.removable_items, 2);
    assert_eq!(forecast.removable_tokens, 50);
    assert_eq!(forecast.kept_items, 1);
    assert_eq!(forecast.remaining_tokens, 50);
    assert!(forecast.tokens_exact);
    assert!(!forecast.is_no_op());
    assert_eq!(forecast.sample_removed, vec![7, 6]);
}

#[test]
fn forecast_removes_the_aggregated_remainder_while_it_keeps_fewer_levels() {
    let projection = projection(
        ContextPruneKind::ToolResults,
        vec![level(1, 10), level(2, 10)],
        5,
        5,
        50,
        200,
    );

    let forecast = projection.forecast(1);

    assert_eq!(forecast.removable_items, 6);
    assert_eq!(forecast.removable_tokens, 60);
    assert!(forecast.tokens_exact);
}

#[test]
fn forecast_reports_aggregated_estimate_beyond_the_snapshot() {
    let projection = projection(ContextPruneKind::ToolResults, Vec::new(), 10, 24, 100, 500);

    let partial = projection.forecast(4);
    assert_eq!(
        partial.removable_items, 14,
        "6 of 10 aggregated levels keep a proportional share of 24 items"
    );
    assert_eq!(partial.removable_tokens, 60);
    assert!(!partial.tokens_exact, "estimate is averaged, not itemized");

    let all = projection.forecast(0);
    assert_eq!(all.removable_items, 24);
    assert_eq!(all.removable_tokens, 100);
    assert!(all.tokens_exact);

    let none = projection.forecast(10);
    assert!(none.is_no_op());
    assert_eq!(none.removable_tokens, 0);
    assert!(none.tokens_exact);
    assert!(none.sample_removed.is_empty());
}

#[test]
fn forecast_counts_items_inside_a_turn_level() {
    let projection = projection(
        ContextPruneKind::Turns,
        vec![group(12, 100, 4), group(0, 50, 2)],
        0,
        0,
        0,
        900,
    );

    let forecast = projection.forecast(1);

    assert_eq!(
        forecast.removable_items, 2,
        "two messages leave the history"
    );
    assert_eq!(forecast.removable_tokens, 50);
    assert_eq!(forecast.kept_items, 4);
    assert_eq!(forecast.remaining_tokens, 850);
    assert_eq!(forecast.sample_removed, vec![0]);
}

#[test]
fn turns_forecast_keeps_at_least_the_newest_group() {
    let projection = projection(
        ContextPruneKind::Turns,
        vec![group(12, 100, 4), group(0, 50, 2)],
        0,
        0,
        0,
        900,
    );

    let clamped = projection.forecast(0);
    let single = projection.forecast(1);

    assert_eq!(
        clamped.keep_recent,
        ContextPruneKind::Turns.min_keep_recent()
    );
    assert_eq!(clamped.removable_items, single.removable_items);
    assert_eq!(clamped.removable_tokens, single.removable_tokens);
    assert!(
        projection.forecast(2).is_no_op(),
        "keeping every group removes nothing"
    );
}

#[test]
fn tail_forecast_reports_the_cut_after_a_named_message() {
    let projection = projection(
        ContextPruneKind::Tail,
        vec![
            tail_cut(6, 20, 1, "message-7"),
            tail_cut(4, 60, 3, "message-5"),
        ],
        0,
        0,
        0,
        400,
    );

    let forecast = projection
        .forecast_tail("message-5")
        .expect("the snapshot knows this cut");

    assert_eq!(forecast.kind, ContextPruneKind::Tail);
    assert_eq!(forecast.removable_items, 3);
    assert_eq!(forecast.removable_tokens, 60);
    assert_eq!(forecast.keep_recent, 5, "five messages stay before the cut");
    assert_eq!(forecast.kept_items, 4);
    assert_eq!(forecast.remaining_tokens, 340);
    assert!(forecast.tokens_exact);
    assert_eq!(forecast.sample_removed, vec![5, 6, 7]);
    assert!(!forecast.is_no_op());
}

#[test]
fn tail_forecast_ignores_unknown_cuts_and_has_no_keep_recent_default() {
    let projection = projection(
        ContextPruneKind::Tail,
        vec![tail_cut(6, 20, 1, "message-7")],
        0,
        0,
        0,
        400,
    );

    assert!(
        projection.forecast_tail("message-1").is_none(),
        "a cut the snapshot does not know cannot be forecast"
    );
    assert!(
        projection
            .forecast(ContextPruneKind::Tail.default_keep_recent())
            .is_no_op(),
        "without after the tail prune removes nothing"
    );
}

#[test]
fn tail_cut_samples_list_the_newest_cuts() {
    let projection = projection(
        ContextPruneKind::Tail,
        vec![
            tail_cut(6, 20, 1, "message-7"),
            tail_cut(5, 35, 2, "message-6"),
        ],
        4,
        4,
        80,
        400,
    );

    assert_eq!(
        projection.tail_cut_samples(8),
        vec!["message-7", "message-6"]
    );
    assert_eq!(projection.tail_cut_samples(1), vec!["message-7"]);
}

#[test]
fn recorded_projection_is_forecastable_and_marked_stale_after_a_revision_change() {
    let mut controller = ContextController::default();
    let revision = controller.manifest().revision;
    controller.record_prune_projections(
        revision,
        vec![projection(
            ContextPruneKind::Images,
            vec![level(1, 10)],
            0,
            0,
            0,
            40,
        )],
    );

    assert!(!controller.prune_projection_stale());
    let forecast = controller
        .prune_forecast(ContextPruneSpec::new(ContextPruneKind::Images))
        .expect("snapshot covers images");
    assert_eq!(forecast.keep_recent, 1);
    assert!(forecast.is_no_op());
    assert!(
        controller
            .prune_forecast(ContextPruneSpec::new(ContextPruneKind::Turns))
            .is_none(),
        "kinds without a snapshot stay unknown"
    );

    controller.update_sources(components("changed"), 1);

    assert!(controller.prune_projection_stale());
    assert_eq!(
        controller.prune_projection_revision(),
        Some(ContextRevision::INITIAL)
    );
}

#[test]
fn prune_forecast_without_a_snapshot_is_unknown() {
    let controller = ContextController::default();

    assert!(controller.prune_projections().is_empty());
    assert!(controller.prune_projection_revision().is_none());
    assert!(!controller.prune_projection_stale());
    assert!(
        controller
            .prune_forecast(ContextPruneSpec::new(ContextPruneKind::Images).keep_recent(0))
            .is_none()
    );
}
