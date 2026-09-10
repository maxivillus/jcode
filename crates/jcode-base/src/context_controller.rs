//! Context preflight and validated execution state flow for the next request.
//!
//! The controller keeps the decision rule separate from `Session` and the
//! transcript. The turn loop may use its result to select an existing
//! compaction path without making the controller a second transcript owner.

use crate::context::{ContextBudget, ContextComponentHashes, ContextManifest, ContextRevision};
use crate::execution_state::{
    ExecutionState, ExecutionStateError, ExecutionStatePatch, ExecutionStateRevision,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextPreflightAction {
    Send,
    Refresh,
    Compact,
    RefreshThenCompact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Действие над контекстом, запрошенное моделью.
///
/// Запрос не исполняется в момент вызова: runtime применяет его на
/// безопасной границе turn-а, когда провайдерский запрос ещё не отправлен.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContextActionKind {
    Refresh,
    Compact,
    ResetProvider,
    Export,
}

/// Проверенный запрос действия с revision, на которой он основан.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextActionRequest {
    pub action: ContextActionKind,
    pub base_revision: ContextRevision,
    pub sequence: u64,
}

/// Итог применения запроса на границе turn-а.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ContextActionOutcome {
    Completed { detail: String },
    Skipped { reason: String },
    Failed { reason: String },
    Rejected { reason: String },
}

/// Последний применённый запрос и его результат.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextActionRecord {
    pub request: ContextActionRequest,
    pub outcome: ContextActionOutcome,
    pub applied_revision: ContextRevision,
}

/// Отказ в постановке запроса действия.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextActionError {
    StaleRevision {
        expected: ContextRevision,
        current: ContextRevision,
    },
}

impl std::fmt::Display for ContextActionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleRevision { expected, current } => write!(
                formatter,
                "context revision changed from {} to {}; read status again",
                expected.0, current.0
            ),
        }
    }
}

impl std::error::Error for ContextActionError {}

#[derive(Debug, Clone, Default)]
pub struct ContextController {
    manifest: ContextManifest,
    execution_state: ExecutionState,
    last_budget: Option<ContextBudget>,
    last_plan: Option<ContextPreflightPlan>,
    pending_actions: Vec<ContextActionRequest>,
    last_action: Option<ContextActionRecord>,
    next_action_sequence: u64,
}

impl ContextController {
    pub fn new(manifest: ContextManifest) -> Self {
        Self {
            manifest,
            execution_state: ExecutionState::default(),
            last_budget: None,
            last_plan: None,
            pending_actions: Vec::new(),
            last_action: None,
            next_action_sequence: 0,
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
            last_budget: None,
            last_plan: None,
            pending_actions: Vec::new(),
            last_action: None,
            next_action_sequence: 0,
        })
    }

    pub fn manifest(&self) -> &ContextManifest {
        &self.manifest
    }

    /// Возвращает единственный state, которым управляет controller.
    pub fn execution_state(&self) -> &ExecutionState {
        &self.execution_state
    }

    /// Возвращает budget последнего provider preflight, если он уже выполнялся.
    pub fn last_budget(&self) -> Option<&ContextBudget> {
        self.last_budget.as_ref()
    }

    /// Возвращает план последнего provider preflight, если он уже выполнялся.
    pub fn last_plan(&self) -> Option<&ContextPreflightPlan> {
        self.last_plan.as_ref()
    }

    /// Ставит запрос действия, проверяя revision, на которой он основан.
    ///
    /// Повторный запрос того же действия, пока он ещё не применён, возвращает
    /// уже поставленный запрос: повторный вызов модели идемпотентен.
    pub fn request_action(
        &mut self,
        action: ContextActionKind,
        expected_revision: ContextRevision,
    ) -> Result<ContextActionRequest, ContextActionError> {
        let current = self.manifest.revision;
        if expected_revision != current {
            return Err(ContextActionError::StaleRevision {
                expected: expected_revision,
                current,
            });
        }
        if let Some(existing) = self
            .pending_actions
            .iter()
            .find(|pending| pending.action == action)
        {
            return Ok(*existing);
        }
        self.next_action_sequence = self.next_action_sequence.saturating_add(1);
        let request = ContextActionRequest {
            action,
            base_revision: current,
            sequence: self.next_action_sequence,
        };
        self.pending_actions.push(request);
        Ok(request)
    }

    /// Запросы, ожидающие безопасной границы turn-а.
    pub fn pending_actions(&self) -> &[ContextActionRequest] {
        &self.pending_actions
    }

    /// Забирает очередь запросов: runtime исполняет её до следующего запроса.
    pub fn take_pending_actions(&mut self) -> Vec<ContextActionRequest> {
        std::mem::take(&mut self.pending_actions)
    }

    /// Запоминает результат применения запроса для последующих status/preview.
    pub fn record_action_outcome(
        &mut self,
        request: ContextActionRequest,
        outcome: ContextActionOutcome,
    ) {
        self.last_action = Some(ContextActionRecord {
            request,
            outcome,
            applied_revision: self.manifest.revision,
        });
    }

    /// Последний применённый запрос действия, если он был.
    pub fn last_action(&self) -> Option<&ContextActionRecord> {
        self.last_action.as_ref()
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
        let changed = self
            .manifest
            .update_components(components, provider_generation);
        if changed {
            self.last_budget = None;
            self.last_plan = None;
        }
        changed
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
        self.last_budget = Some(*budget);
        self.last_plan = Some(plan);
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

        assert_eq!(controller.pending_actions(), &[request]);
        assert!(controller.manifest().revision.0 > request.base_revision.0);
    }

    #[test]
    fn take_pending_actions_clears_queue_and_keeps_outcome() {
        let mut controller = ContextController::default();
        let request = controller
            .request_action(ContextActionKind::Export, ContextRevision::INITIAL)
            .expect("request is accepted");

        assert_eq!(controller.take_pending_actions(), vec![request]);
        assert!(controller.pending_actions().is_empty());

        controller.record_action_outcome(
            request,
            ContextActionOutcome::Completed {
                detail: "exported".to_string(),
            },
        );
        let record = controller.last_action().expect("outcome is retained");
        assert_eq!(record.request, request);
        assert_eq!(record.applied_revision, controller.manifest().revision);
    }
}
