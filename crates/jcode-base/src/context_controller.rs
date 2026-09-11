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
    Prune,
    UndoPrune,
}

/// Вид структурной обрезки контекста.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContextPruneKind {
    Images,
    MemoryInjections,
    ToolResults,
    Turns,
}

/// Параметры обрезки: что именно режем и сколько последних элементов щадим.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPruneSpec {
    pub kind: ContextPruneKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent: Option<usize>,
}

impl ContextPruneSpec {
    pub fn new(kind: ContextPruneKind) -> Self {
        Self {
            kind,
            keep_recent: None,
        }
    }

    pub fn keep_recent(mut self, keep_recent: usize) -> Self {
        self.keep_recent = Some(keep_recent);
        self
    }
}

impl ContextPruneKind {
    /// Сколько последних элементов этот вид обрезки щадит по умолчанию.
    pub fn default_keep_recent(self) -> usize {
        match self {
            Self::Images => 1,
            Self::MemoryInjections => 1,
            Self::ToolResults => 2,
            Self::Turns => 6,
        }
    }

    /// Наименьший допустимый `keep_recent` для вида обрезки.
    ///
    /// Turn-группы требуют хотя бы одной сохраняемой группы: иначе провайдерский
    /// transcript остался бы пустым.
    pub fn min_keep_recent(self) -> usize {
        match self {
            Self::Turns => 1,
            Self::Images | Self::MemoryInjections | Self::ToolResults => 0,
        }
    }
}

/// Сколько элементов проекция хранит поимённо. Более старые элементы
/// складываются в агрегированный остаток.
pub const MAX_PROJECTION_LEVELS: usize = 256;

/// Сколько удаляемых позиций прогноз показывает для наблюдаемости.
const MAX_FORECAST_SAMPLE: usize = 8;

/// Один удаляемый уровень проекции: с какой позиции начинается удаление,
/// сколько элементов контекста оно забирает и сколько токенов освободит.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPruneLevel {
    pub index: usize,
    pub tokens: usize,
    /// Элементов контекста в уровне: для изображений, результатов и
    /// memory-инъекций это 1, для turn-группы — число её сообщений.
    pub items: usize,
}

/// Проекция одного вида обрезки, снятая владельцем транскрипта.
///
/// Controller не владеет сообщениями и хранит только позиции и оценки
/// токенов. Поэтому `preview` отвечает по снимку с границы turn-а, не читая
/// транскрипт и не становясь вторым владельцем истории.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPruneProjection {
    pub kind: ContextPruneKind,
    /// Уровни удаления от самых новых элементов к самым старым.
    pub levels: Vec<ContextPruneLevel>,
    /// Сколько старых элементов не попало в `levels`.
    pub overflow_levels: usize,
    /// Сколько элементов контекста в этих старых уровнях.
    pub overflow_items: usize,
    /// Суммарные токены этих старых элементов.
    pub overflow_tokens: usize,
    /// Оценка токенов всего контекста на момент снимка.
    pub total_tokens: usize,
    /// Сколько сообщений было в транскрипте на момент снимка.
    pub source_messages: usize,
}

impl ContextPruneProjection {
    /// Предварительный расчёт: что уйдёт при `keep_recent` и сколько освободит.
    ///
    /// Элементы упорядочены от новых к старым, поэтому обрезка сохраняет
    /// `keep_recent` первых уровней и удаляет все остальные.
    pub fn forecast(&self, keep_recent: usize) -> ContextPruneForecast {
        // Расчёт повторяет применение обрезки, поэтому нижняя граница та же.
        let keep_recent = keep_recent.max(self.kind.min_keep_recent());
        let stored = self.levels.len();
        let total_items = self
            .levels
            .iter()
            .map(|level| level.items)
            .fold(self.overflow_items, usize::saturating_add);
        let total_levels = stored.saturating_add(self.overflow_levels);
        let removable_levels = total_levels.saturating_sub(keep_recent);
        let (removable_items, removable_tokens, tokens_exact) = if keep_recent < stored {
            let removed = &self.levels[keep_recent..];
            (
                removed
                    .iter()
                    .map(|level| level.items)
                    .fold(self.overflow_items, usize::saturating_add),
                removed
                    .iter()
                    .map(|level| level.tokens)
                    .fold(self.overflow_tokens, usize::saturating_add),
                true,
            )
        } else if removable_levels == 0 {
            (0, 0, true)
        } else if removable_levels == self.overflow_levels {
            (self.overflow_items, self.overflow_tokens, true)
        } else {
            let share = self.overflow_levels.max(1);
            (
                self.overflow_items.saturating_mul(removable_levels) / share,
                self.overflow_tokens.saturating_mul(removable_levels) / share,
                false,
            )
        };
        let sample_removed = self
            .levels
            .iter()
            .skip(keep_recent)
            .rev()
            .take(MAX_FORECAST_SAMPLE)
            .map(|level| level.index)
            .collect();
        ContextPruneForecast {
            kind: self.kind,
            keep_recent,
            removable_items,
            removable_tokens,
            kept_items: total_items.saturating_sub(removable_items),
            remaining_tokens: self.total_tokens.saturating_sub(removable_tokens),
            tokens_exact,
            sample_removed,
        }
    }
}

/// Итог предварительного расчёта одного вида обрезки.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPruneForecast {
    pub kind: ContextPruneKind,
    pub keep_recent: usize,
    pub removable_items: usize,
    pub removable_tokens: usize,
    pub kept_items: usize,
    pub remaining_tokens: usize,
    /// false, если токены посчитаны по среднему: снимок хранит только самые
    /// новые элементы, а `keep_recent` больше их числа.
    pub tokens_exact: bool,
    /// До восьми самых старых удаляемых позиций.
    pub sample_removed: Vec<usize>,
}

impl ContextPruneForecast {
    /// Обрезка по этому расчёту ничего не изменит.
    pub fn is_no_op(&self) -> bool {
        self.removable_items == 0
    }
}

/// Проверенный запрос действия с revision, на которой он основан.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextActionRequest {
    pub action: ContextActionKind,
    pub base_revision: ContextRevision,
    pub sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prune: Option<ContextPruneSpec>,
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
    prune_projections: Vec<ContextPruneProjection>,
    prune_projection_revision: Option<ContextRevision>,
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
            prune_projections: Vec::new(),
            prune_projection_revision: None,
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
            prune_projections: Vec::new(),
            prune_projection_revision: None,
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
        self.request_action_with_prune(action, expected_revision, None)
    }

    /// Ставит запрос структурной обрезки с явными параметрами.
    pub fn request_prune(
        &mut self,
        spec: ContextPruneSpec,
        expected_revision: ContextRevision,
    ) -> Result<ContextActionRequest, ContextActionError> {
        self.request_action_with_prune(ContextActionKind::Prune, expected_revision, Some(spec))
    }

    fn request_action_with_prune(
        &mut self,
        action: ContextActionKind,
        expected_revision: ContextRevision,
        prune: Option<ContextPruneSpec>,
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
            .find(|pending| pending.action == action && pending.prune == prune)
        {
            return Ok(existing.clone());
        }
        self.next_action_sequence = self.next_action_sequence.saturating_add(1);
        let request = ContextActionRequest {
            action,
            base_revision: current,
            sequence: self.next_action_sequence,
            prune,
        };
        self.pending_actions.push(request.clone());
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

    /// Запоминает проекцию обрезки, снятую владельцем транскрипта.
    ///
    /// Снимок нужен, чтобы `preview` отвечал без чтения транскрипта. Revision
    /// фиксирует, к какому состоянию контекста снимок относится.
    pub fn record_prune_projections(
        &mut self,
        revision: ContextRevision,
        projections: Vec<ContextPruneProjection>,
    ) {
        self.prune_projections = projections;
        self.prune_projection_revision = Some(revision);
    }

    /// Проекции обрезки из последнего снимка.
    pub fn prune_projections(&self) -> &[ContextPruneProjection] {
        &self.prune_projections
    }

    /// Revision, на которой был снят последний снимок проекции.
    pub fn prune_projection_revision(&self) -> Option<ContextRevision> {
        self.prune_projection_revision
    }

    /// Снимок проекции относится к более старой revision и требует обновления.
    pub fn prune_projection_stale(&self) -> bool {
        self.prune_projection_revision
            .is_some_and(|revision| revision != self.manifest.revision)
    }

    /// Проекция одного вида обрезки, если снимок её содержит.
    pub fn prune_projection(&self, kind: ContextPruneKind) -> Option<&ContextPruneProjection> {
        self.prune_projections
            .iter()
            .find(|projection| projection.kind == kind)
    }

    /// Предварительный расчёт обрезки без изменения контекста.
    ///
    /// Возвращает `None`, если снимок проекции ещё не снят.
    pub fn prune_forecast(&self, spec: ContextPruneSpec) -> Option<ContextPruneForecast> {
        let projection = self.prune_projection(spec.kind)?;
        let keep_recent = spec
            .keep_recent
            .unwrap_or_else(|| spec.kind.default_keep_recent());
        Some(projection.forecast(keep_recent))
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
        }
    }

    /// Уровень turn-группы: одно удаление забирает несколько сообщений.
    fn group(index: usize, tokens: usize, items: usize) -> ContextPruneLevel {
        ContextPruneLevel {
            index,
            tokens,
            items,
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
        assert_eq!(ContextPruneKind::ToolResults.default_keep_recent(), 2);
        assert_eq!(ContextPruneKind::Turns.default_keep_recent(), 6);
    }

    #[test]
    fn prune_keep_recent_floors_keep_the_transcript_usable() {
        assert_eq!(ContextPruneKind::Turns.min_keep_recent(), 1);
        assert_eq!(ContextPruneKind::Images.min_keep_recent(), 0);
        assert_eq!(ContextPruneKind::MemoryInjections.min_keep_recent(), 0);
        assert_eq!(ContextPruneKind::ToolResults.min_keep_recent(), 0);
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
}
