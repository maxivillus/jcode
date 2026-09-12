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
    /// Системные напоминания харнесса (`<system-reminder>`), кроме
    /// memory-инъекций: у них свой вид `memory-injections`.
    SystemReminders,
    ToolResults,
    Turns,
    /// Хвост после выбранного сообщения: `after` задаёт последнее
    /// сохраняемое сообщение, всё после него удаляется.
    Tail,
}

/// Параметры обрезки: что именно режем и сколько последних элементов щадим.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPruneSpec {
    pub kind: ContextPruneKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent: Option<usize>,
    /// Последнее сохраняемое сообщение для `tail`. Другие виды его не
    /// используют. В JSON поле называется `after`: так же его называет
    /// инструмент.
    #[serde(default, rename = "after", skip_serializing_if = "Option::is_none")]
    pub after_message_id: Option<String>,
}

impl ContextPruneSpec {
    pub fn new(kind: ContextPruneKind) -> Self {
        Self {
            kind,
            keep_recent: None,
            after_message_id: None,
        }
    }

    pub fn keep_recent(mut self, keep_recent: usize) -> Self {
        self.keep_recent = Some(keep_recent);
        self
    }

    /// Точка среза хвоста: сохраняется это сообщение и всё до него.
    pub fn after(mut self, message_id: impl Into<String>) -> Self {
        self.after_message_id = Some(message_id.into());
        self
    }
}

impl ContextPruneKind {
    /// Сколько последних элементов этот вид обрезки щадит по умолчанию.
    ///
    /// `tail` не использует `keep_recent`: его срез задаёт `after`.
    pub fn default_keep_recent(self) -> usize {
        match self {
            Self::Images => 1,
            Self::MemoryInjections => 1,
            Self::SystemReminders => 1,
            Self::ToolResults => 2,
            Self::Turns => 6,
            Self::Tail => 0,
        }
    }

    /// Наименьший допустимый `keep_recent` для вида обрезки.
    ///
    /// Turn-группы требуют хотя бы одной сохраняемой группы: иначе провайдерский
    /// transcript остался бы пустым.
    pub fn min_keep_recent(self) -> usize {
        match self {
            Self::Turns => 1,
            Self::Images | Self::MemoryInjections | Self::SystemReminders | Self::ToolResults => 0,
            Self::Tail => 0,
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
///
/// Обычные виды описывают одним уровнем один элемент или одну turn-группу.
/// `tail` использует уровень как готовую точку среза: `message_id` называет
/// последнее сохраняемое сообщение, а `items` и `tokens` описывают весь
/// удаляемый хвост после него.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPruneLevel {
    pub index: usize,
    pub tokens: usize,
    /// Элементов контекста в уровне: для изображений, результатов и
    /// memory-инъекций это 1, для turn-группы — число её сообщений.
    pub items: usize,
    /// Id последнего сохраняемого сообщения. Заполняется только `tail`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Подпись естественной точки среза. Заполняется только `tail`, и только
    /// для точек, которые не зависят от выбора: граница последнего сжатия и
    /// начало сессии. Остальные точки среза остаются безымянными.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
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
    ///
    /// У `tail` нет `keep_recent`: срез задаёт `after`, поэтому общий прогноз
    /// для этого вида ничего не удаляет. Точный расчёт даёт [`Self::forecast_tail`].
    pub fn forecast(&self, keep_recent: usize) -> ContextPruneForecast {
        if self.kind == ContextPruneKind::Tail {
            return ContextPruneForecast {
                kind: self.kind,
                keep_recent: 0,
                removable_items: 0,
                removable_tokens: 0,
                kept_items: self.source_messages,
                remaining_tokens: self.total_tokens,
                tokens_exact: true,
                sample_removed: Vec::new(),
            };
        }
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

    /// Предварительный расчёт обрезки хвоста после `after_message_id`.
    ///
    /// Возвращает `None`, если снимок не знает такого последнего сохраняемого
    /// сообщения: оно старше окна снимка или после него остался бы незакрытый
    /// вызов инструмента, поэтому срез по нему не предлагается.
    pub fn forecast_tail(&self, after_message_id: &str) -> Option<ContextPruneForecast> {
        let level = self
            .levels
            .iter()
            .find(|level| level.message_id.as_deref() == Some(after_message_id))?;
        Some(ContextPruneForecast {
            kind: self.kind,
            // Для `tail` это число сообщений, которые остаются до среза.
            keep_recent: level.index.saturating_add(1),
            removable_items: level.items,
            removable_tokens: level.tokens,
            kept_items: self.source_messages.saturating_sub(level.items),
            remaining_tokens: self.total_tokens.saturating_sub(level.tokens),
            tokens_exact: true,
            sample_removed: (level.index.saturating_add(1)
                ..=level.index.saturating_add(level.items))
                .take(MAX_FORECAST_SAMPLE)
                .collect(),
        })
    }

    /// Самые новые точки среза хвоста: id последнего сохраняемого сообщения.
    pub fn tail_cut_samples(&self, limit: usize) -> Vec<&str> {
        self.levels
            .iter()
            .filter_map(|level| level.message_id.as_deref())
            .take(limit)
            .collect()
    }

    /// Подписанные точки среза хвоста: подпись и id последнего сохраняемого
    /// сообщения. Эти точки не зависят от выбора: граница последнего сжатия и
    /// начало сессии.
    pub fn tail_checkpoint_samples(&self, limit: usize) -> Vec<(&str, &str)> {
        self.levels
            .iter()
            .filter_map(|level| Some((level.checkpoint.as_deref()?, level.message_id.as_deref()?)))
            .take(limit)
            .collect()
    }
}

/// Итог предварительного расчёта одного вида обрезки.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPruneForecast {
    pub kind: ContextPruneKind,
    /// Для видов с `keep_recent` — сколько последних элементов сохраняется.
    /// Для `tail` — сколько сообщений остаётся до точки среза.
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
            .find(|pending| pending.action == action && pending.prune.as_ref() == prune.as_ref())
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
    /// Возвращает `None`, если снимок проекции ещё не снят или не покрывает
    /// запрошенный срез.
    pub fn prune_forecast(&self, spec: ContextPruneSpec) -> Option<ContextPruneForecast> {
        let projection = self.prune_projection(spec.kind)?;
        if spec.kind == ContextPruneKind::Tail {
            return projection.forecast_tail(spec.after_message_id.as_deref()?);
        }
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
#[path = "context_controller_tests.rs"]
mod tests;
