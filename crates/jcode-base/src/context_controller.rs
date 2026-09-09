//! Context preflight and validated execution state flow for the next request.
//!
//! The controller keeps the decision rule separate from `Session` and the
//! transcript. The turn loop may use its result to select an existing
//! compaction path without making the controller a second transcript owner.

use crate::context::{ContextBudget, ContextComponentHashes, ContextManifest, ContextRevision};
use crate::execution_state::{
    ExecutionState, ExecutionStateError, ExecutionStatePatch, ExecutionStateRevision,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextPreflightAction {
    Send,
    Refresh,
    Compact,
    RefreshThenCompact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextPreflightPlan {
    pub action: ContextPreflightAction,
    pub revision: ContextRevision,
    pub estimated_input_tokens: usize,
    pub max_input_tokens: usize,
}

impl ContextPreflightPlan {
    pub fn needs_refresh(&self) -> bool {
        matches!(
            self.action,
            ContextPreflightAction::Refresh | ContextPreflightAction::RefreshThenCompact
        )
    }

    pub fn needs_compaction(&self) -> bool {
        matches!(
            self.action,
            ContextPreflightAction::Compact | ContextPreflightAction::RefreshThenCompact
        )
    }
}

#[derive(Debug, Clone, Default)]
pub struct ContextController {
    manifest: ContextManifest,
    execution_state: ExecutionState,
}

impl ContextController {
    pub fn new(manifest: ContextManifest) -> Self {
        Self {
            manifest,
            execution_state: ExecutionState::default(),
        }
    }

    /// Создаёт controller с восстановленным и проверенным execution state.
    pub fn new_with_execution_state(
        manifest: ContextManifest,
        execution_state: ExecutionState,
    ) -> Result<Self, ExecutionStateError> {
        execution_state.validate()?;
        Ok(Self {
            manifest,
            execution_state,
        })
    }

    pub fn manifest(&self) -> &ContextManifest {
        &self.manifest
    }

    /// Возвращает единственный state, которым управляет controller.
    pub fn execution_state(&self) -> &ExecutionState {
        &self.execution_state
    }

    /// Проверяет patch без изменения state controller.
    pub fn preview_execution_state_patch(
        &self,
        patch: &ExecutionStatePatch,
    ) -> Result<ExecutionState, ExecutionStateError> {
        self.execution_state.preview_patch(patch)
    }

    /// Атомарно применяет проверенный patch и возвращает новую state revision.
    pub fn apply_execution_state_patch(
        &mut self,
        patch: &ExecutionStatePatch,
    ) -> Result<ExecutionStateRevision, ExecutionStateError> {
        self.execution_state.apply_patch(patch)?;
        Ok(self.execution_state.revision)
    }

    /// Обновляет hashes и provider generation до отправки запроса.
    pub fn update_sources(
        &mut self,
        components: ContextComponentHashes,
        provider_generation: u64,
    ) -> bool {
        self.manifest
            .update_components(components, provider_generation)
    }

    /// Строит план без изменения Session, transcript или provider state.
    pub fn plan(
        &self,
        budget: &ContextBudget,
        current_components: &ContextComponentHashes,
        provider_generation: u64,
    ) -> ContextPreflightPlan {
        let needs_refresh = !self
            .manifest
            .is_fresh(current_components, provider_generation);
        let needs_compaction = !budget.fits();
        let action = match (needs_refresh, needs_compaction) {
            (false, false) => ContextPreflightAction::Send,
            (true, false) => ContextPreflightAction::Refresh,
            (false, true) => ContextPreflightAction::Compact,
            (true, true) => ContextPreflightAction::RefreshThenCompact,
        };

        ContextPreflightPlan {
            action,
            revision: self.manifest.revision,
            estimated_input_tokens: budget.estimated_input_tokens,
            max_input_tokens: budget.max_input_tokens(),
        }
    }

    /// Обновляет revision при изменении источников и запоминает estimate.
    /// Session, transcript и provider state не изменяются.
    pub fn prepare(
        &mut self,
        budget: &ContextBudget,
        current_components: ContextComponentHashes,
        provider_generation: u64,
    ) -> ContextPreflightPlan {
        let mut plan = self.plan(budget, &current_components, provider_generation);
        if plan.needs_refresh() {
            self.update_sources(current_components, provider_generation);
            plan.revision = self.manifest.revision;
        }
        self.manifest.estimated_input_tokens = budget.estimated_input_tokens;
        plan
    }

    /// Записывает фактический usage только для той revision, которая была
    /// отправлена. Поздний результат старой revision отклоняется.
    pub fn record_observed_input_tokens(
        &mut self,
        revision: ContextRevision,
        observed_input_tokens: usize,
    ) -> bool {
        if self.manifest.revision != revision {
            return false;
        }
        self.manifest.observed_input_tokens = Some(observed_input_tokens);
        true
    }
}

#[cfg(test)]
mod tests {
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

        let error =
            ContextController::new_with_execution_state(ContextManifest::default(), invalid)
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
}
