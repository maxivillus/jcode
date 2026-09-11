use super::Agent;
use crate::context::ContextRevision;
use crate::context_controller::{
    ContextActionOutcome, ContextPruneKind, ContextPruneLevel, ContextPruneProjection,
    ContextPruneSpec, MAX_PROJECTION_LEVELS,
};
use crate::message::{ContentBlock, Message, Role};
use crate::session::StoredMessage;

/// Начало сообщения memory-инъекции, как его собирает `memory_injection_message`.
const MEMORY_INJECTION_MARKER: &str = "<system-reminder>\n# Memory\n";
/// Заметка, которой заменяется удалённое изображение.
const PRUNED_IMAGE_NOTE: &str = "[image pruned to save context]";
/// Заметка, которой заменяется удалённый результат инструмента.
const PRUNED_TOOL_RESULT_NOTE: &str = "[tool result pruned to save context]";
/// Начало уже удалённого результата: такие блоки не считаются кандидатами.
const PRUNED_TOOL_RESULT_PREFIX: &str = "[tool result pruned";

/// Виды обрезки в порядке, в котором их показывает снимок проекции.
const PRUNE_KINDS: [ContextPruneKind; 4] = [
    ContextPruneKind::Images,
    ContextPruneKind::MemoryInjections,
    ContextPruneKind::ToolResults,
    ContextPruneKind::Turns,
];

fn is_memory_injection(message: &StoredMessage) -> bool {
    message.role == Role::User
        && message.content.iter().any(|block| match block {
            ContentBlock::Text { text, .. } => text.starts_with(MEMORY_INJECTION_MARKER),
            _ => false,
        })
}

/// Позиция блока внутри транскрипта.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockPosition {
    message: usize,
    block: usize,
}

/// Блоки, которые обрезка заменит короткой заметкой: от новых к старым.
///
/// Это единственное место, где выбираются цели для `images` и `tool-results`:
/// обрезка и проекция `preview` используют один и тот же список.
fn prunable_block_positions(
    messages: &[StoredMessage],
    kind: ContextPruneKind,
) -> Vec<BlockPosition> {
    let mut positions = Vec::new();
    for (message_index, message) in messages.iter().enumerate().rev() {
        for (block_index, block) in message.content.iter().enumerate().rev() {
            let prunable = match (kind, block) {
                (ContextPruneKind::Images, ContentBlock::Image { .. }) => true,
                (ContextPruneKind::ToolResults, ContentBlock::ToolResult { content, .. }) => {
                    !content.starts_with(PRUNED_TOOL_RESULT_PREFIX)
                }
                _ => false,
            };
            if prunable {
                positions.push(BlockPosition {
                    message: message_index,
                    block: block_index,
                });
            }
        }
    }
    positions
}

/// Позиции memory-инъекций, которые обрезка может удалить целиком.
fn prunable_memory_injections(messages: &[StoredMessage]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| is_memory_injection(message))
        .map(|(index, _)| index)
        .collect()
}

/// Начала turn-груп: группа идёт от сообщения пользователя до следующего.
///
/// Это те же границы, по которым удаляет обрезка turn-групп, поэтому
/// `keep_recent` считает именно группы, а не отдельные записи транскрипта.
fn turn_group_starts(messages: &[StoredMessage]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == Role::User)
        .map(|(index, _)| index)
        .collect()
}

/// Сколько токенов освободит замена блока короткой заметкой.
fn block_savings_tokens(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Image { .. } => super::context_control::block_token_estimate(block)
            .saturating_sub(crate::util::estimate_tokens(PRUNED_IMAGE_NOTE)),
        ContentBlock::ToolResult { content, .. } => crate::util::estimate_tokens(content)
            .saturating_sub(crate::util::estimate_tokens(PRUNED_TOOL_RESULT_NOTE)),
        _ => 0,
    }
}

/// Уровни обрезки одного вида: от новых к старым, с оценкой экономии.
///
/// Уровень turn-группы забирает все свои сообщения, поэтому он несёт их число.
fn prune_levels(messages: &[StoredMessage], kind: ContextPruneKind) -> Vec<ContextPruneLevel> {
    match kind {
        ContextPruneKind::Images | ContextPruneKind::ToolResults => {
            prunable_block_positions(messages, kind)
                .into_iter()
                .map(|position| ContextPruneLevel {
                    index: position.message,
                    tokens: messages
                        .get(position.message)
                        .and_then(|message| message.content.get(position.block))
                        .map_or(0, block_savings_tokens),
                    items: 1,
                })
                .collect()
        }
        ContextPruneKind::MemoryInjections => prunable_memory_injections(messages)
            .into_iter()
            .map(|index| ContextPruneLevel {
                index,
                tokens: messages
                    .get(index)
                    .map_or(0, super::context_control::stored_message_token_estimate),
                items: 1,
            })
            .collect(),
        ContextPruneKind::Turns => turn_prune_levels(messages),
    }
}

/// Уровни обрезки turn-групп: один уровень на группу.
///
/// Границы берутся из того же источника, что и при применении обрезки, иначе
/// `preview` показал бы не то, что удалит обрезка.
fn turn_prune_levels(messages: &[StoredMessage]) -> Vec<ContextPruneLevel> {
    let mut boundaries = turn_group_starts(messages);
    if boundaries.len() <= ContextPruneKind::Turns.min_keep_recent() {
        // Единственную группу обрезка не забирает: transcript не остаётся пустым.
        return Vec::new();
    }
    boundaries.push(messages.len());
    boundaries
        .windows(2)
        .rev()
        .map(|window| {
            let slice = messages.get(window[0]..window[1]).unwrap_or(&[]);
            ContextPruneLevel {
                index: window[0],
                tokens: slice
                    .iter()
                    .map(super::context_control::stored_message_token_estimate)
                    .fold(0usize, usize::saturating_add),
                items: slice.len(),
            }
        })
        .collect()
}

/// Снимок проекции одного вида обрезки для контроллера.
fn build_prune_projection(
    kind: ContextPruneKind,
    messages: &[StoredMessage],
    total_tokens: usize,
) -> ContextPruneProjection {
    let mut levels = prune_levels(messages, kind);
    let (overflow_levels, overflow_items, overflow_tokens) = if levels.len() > MAX_PROJECTION_LEVELS
    {
        let overflow = levels.split_off(MAX_PROJECTION_LEVELS);
        (
            overflow.len(),
            overflow
                .iter()
                .map(|level| level.items)
                .fold(0usize, usize::saturating_add),
            overflow
                .iter()
                .map(|level| level.tokens)
                .fold(0usize, usize::saturating_add),
        )
    } else {
        (0, 0, 0)
    };
    ContextPruneProjection {
        kind,
        levels,
        overflow_levels,
        overflow_items,
        overflow_tokens,
        total_tokens,
        source_messages: messages.len(),
    }
}

/// История и provider-сессия до обратимой обрезки контекста.
#[derive(Clone)]
pub(super) struct ContextPruneUndoSnapshot {
    messages: Vec<StoredMessage>,
    provider_session_id: Option<String>,
    session_provider_session_id: Option<String>,
}

impl Agent {
    /// Структурная обрезка контекста по явному запросу модели.
    ///
    /// Каждый вид режет только свой тип данных: изображения и результаты
    /// инструментов заменяются короткой заметкой (парность tool_use и
    /// tool_result сохраняется), а лишние старые turn-группы удаляются
    /// целиком. Перед изменением сохраняется снапшот истории для undo.
    pub(super) fn prune_for_model_request(
        &mut self,
        spec: ContextPruneSpec,
    ) -> ContextActionOutcome {
        let keep_recent = spec
            .keep_recent
            .unwrap_or_else(|| spec.kind.default_keep_recent());
        let before_tokens = self.provider_token_estimate();
        let snapshot = ContextPruneUndoSnapshot {
            messages: self.session.messages.clone(),
            provider_session_id: self.provider_session_id.clone(),
            session_provider_session_id: self.session.provider_session_id.clone(),
        };

        let pruned = self.apply_prune(spec.kind, keep_recent);

        if pruned == 0 {
            return ContextActionOutcome::Skipped {
                reason: format!(
                    "nothing to prune for {:?} with keep_recent={keep_recent}",
                    spec.kind
                ),
            };
        }

        self.prune_undo_snapshot = Some(snapshot);
        self.session.updated_at = chrono::Utc::now();
        self.invalidate_provider_context("context prune");
        self.locked_tools = None;
        self.reset_tool_output_tracking();
        self.note_transcript_mutation();
        self.persist_session_best_effort("context prune");
        let after_tokens = self.provider_token_estimate();
        self.record_prune_projections(self.context_revision(), after_tokens);
        crate::logging::info(&format!(
            "Model-requested context prune ({:?}): {pruned} item(s), estimated tokens {before_tokens} -> {after_tokens}",
            spec.kind
        ));
        ContextActionOutcome::Completed {
            detail: format!(
                "pruned {pruned} item(s); estimated tokens {before_tokens} -> {after_tokens}"
            ),
        }
    }

    /// Возвращает историю и provider-сессию к состоянию до последней обрезки.
    pub(super) fn undo_prune_for_model_request(&mut self) -> ContextActionOutcome {
        let Some(snapshot) = self.prune_undo_snapshot.take() else {
            return ContextActionOutcome::Skipped {
                reason: "no context prune to undo".to_string(),
            };
        };
        self.session.replace_messages(snapshot.messages);
        self.provider_session_id = snapshot.provider_session_id;
        self.session.provider_session_id = snapshot.session_provider_session_id;
        self.session.updated_at = chrono::Utc::now();
        self.cache_tracker.reset();
        self.locked_tools = None;
        self.reset_tool_output_tracking();
        self.note_transcript_mutation();
        self.persist_session_best_effort("context prune undo");
        self.refresh_prune_projections();
        ContextActionOutcome::Completed {
            detail: "restored the transcript from before the last prune".to_string(),
        }
    }

    /// Применяет обрезку вида и возвращает число удалённых элементов.
    fn apply_prune(&mut self, kind: ContextPruneKind, keep_recent: usize) -> usize {
        // Та же нижняя граница, что и у `forecast`: transcript не остаётся пустым.
        let keep_recent = keep_recent.max(kind.min_keep_recent());
        match kind {
            ContextPruneKind::Images | ContextPruneKind::ToolResults => {
                let positions = prunable_block_positions(&self.session.messages, kind);
                self.replace_prunable_blocks(kind, &positions, keep_recent)
            }
            ContextPruneKind::MemoryInjections => {
                let targets = prunable_memory_injections(&self.session.messages);
                self.drop_messages(&targets, keep_recent)
            }
            ContextPruneKind::Turns => self.drop_turn_prefix(keep_recent),
        }
    }

    /// Заменяет заметкой все цели, кроме последних `keep_recent`.
    fn replace_prunable_blocks(
        &mut self,
        kind: ContextPruneKind,
        positions: &[BlockPosition],
        keep_recent: usize,
    ) -> usize {
        let mut pruned = 0;
        for position in positions.iter().skip(keep_recent) {
            let Some(block) = self
                .session
                .messages
                .get_mut(position.message)
                .and_then(|message| message.content.get_mut(position.block))
            else {
                continue;
            };
            let replaced = match kind {
                ContextPruneKind::Images => match block {
                    ContentBlock::Image { .. } => {
                        *block = ContentBlock::Text {
                            text: PRUNED_IMAGE_NOTE.to_string(),
                            cache_control: None,
                        };
                        true
                    }
                    _ => false,
                },
                ContextPruneKind::ToolResults => match block {
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } => {
                        *content = PRUNED_TOOL_RESULT_NOTE.to_string();
                        *is_error = None;
                        true
                    }
                    _ => false,
                },
                _ => false,
            };
            if replaced {
                pruned += 1;
            }
        }
        if pruned > 0 {
            // Замена блоков не меняет число сообщений: без явного сброса
            // следующий запрос к провайдеру ушёл бы со старым содержимым.
            self.session.mark_block_content_changed();
        }
        pruned
    }

    /// Удаляет сообщения, кроме последних `keep_recent` целей.
    fn drop_messages(&mut self, targets: &[usize], keep_recent: usize) -> usize {
        if targets.len() <= keep_recent {
            return 0;
        }
        let mut keep: Vec<bool> = vec![true; self.session.messages.len()];
        for index in targets.iter().skip(keep_recent) {
            if let Some(slot) = keep.get_mut(*index) {
                *slot = false;
            }
        }
        let retained: Vec<StoredMessage> = self
            .session
            .messages
            .iter()
            .zip(keep)
            .filter(|(_, keep)| *keep)
            .map(|(message, _)| message.clone())
            .collect();
        let removed = self.session.messages.len() - retained.len();
        if removed == 0 {
            return 0;
        }
        self.session.replace_messages(retained);
        removed
    }

    /// Удаляет историю до границы сохранения turn-групп.
    fn drop_turn_prefix(&mut self, keep_recent: usize) -> usize {
        let starts = turn_group_starts(&self.session.messages);
        if starts.len() <= keep_recent {
            return 0;
        }
        let start_index = starts[starts.len() - keep_recent];
        // Удаляется префикс до границы, поэтому число удалённых сообщений
        // равно самой границе.
        let removed = start_index;
        let rest = self.session.messages[start_index..].to_vec();
        self.session.replace_messages(rest);
        removed
    }

    /// Пересчитывает снимок проекции после изменения транскрипта.
    pub(super) fn refresh_prune_projections(&mut self) {
        let total_tokens = self.provider_token_estimate();
        self.record_prune_projections(self.context_revision(), total_tokens);
    }

    /// Запоминает проекцию обрезки для `preview` на указанной revision.
    ///
    /// Контроллер получает только позиции и оценки токенов, поэтому остаётся
    /// владельцем решения и не становится владельцем транскрипта.
    pub(super) fn record_prune_projections(
        &mut self,
        revision: ContextRevision,
        total_tokens: usize,
    ) {
        let projections = {
            let messages = &self.session.messages;
            PRUNE_KINDS
                .iter()
                .map(|kind| build_prune_projection(*kind, messages, total_tokens))
                .collect()
        };
        self.context_controller
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record_prune_projections(revision, projections);
    }

    fn context_revision(&self) -> ContextRevision {
        self.context_controller
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .manifest()
            .revision
    }

    fn provider_token_estimate(&mut self) -> usize {
        let messages: Vec<Message> = self.session.messages_for_provider().to_vec();
        super::context_control::message_token_estimate(&messages)
    }
}
