use super::Agent;
use crate::context_controller::{ContextActionOutcome, ContextPruneKind, ContextPruneSpec};
use crate::message::{ContentBlock, Message, Role};
use crate::session::StoredMessage;

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
        let keep_recent = spec.keep_recent.unwrap_or(match spec.kind {
            ContextPruneKind::Images => 1,
            ContextPruneKind::ToolResults => 2,
            ContextPruneKind::Turns => 6,
        });
        let before_tokens = self.provider_token_estimate();
        let snapshot = ContextPruneUndoSnapshot {
            messages: self.session.messages.clone(),
            provider_session_id: self.provider_session_id.clone(),
            session_provider_session_id: self.session.provider_session_id.clone(),
        };

        let pruned = match spec.kind {
            ContextPruneKind::Images => self.prune_images(keep_recent),
            ContextPruneKind::ToolResults => self.prune_tool_results(keep_recent),
            ContextPruneKind::Turns => self.prune_turns(keep_recent),
        };

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
        self.persist_session_best_effort("context prune");
        let after_tokens = self.provider_token_estimate();
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
        self.persist_session_best_effort("context prune undo");
        ContextActionOutcome::Completed {
            detail: "restored the transcript from before the last prune".to_string(),
        }
    }

    fn provider_token_estimate(&mut self) -> usize {
        let messages: Vec<Message> = self.session.messages_for_provider().to_vec();
        super::context_control::message_token_estimate(&messages)
    }

    fn prune_images(&mut self, keep_recent: usize) -> usize {
        let mut remaining_keep = keep_recent;
        let mut pruned = 0;
        for message in self.session.messages.iter_mut().rev() {
            for block in message.content.iter_mut().rev() {
                if matches!(block, ContentBlock::Image { .. }) {
                    if remaining_keep > 0 {
                        remaining_keep -= 1;
                        continue;
                    }
                    *block = ContentBlock::Text {
                        text: "[image pruned to save context]".to_string(),
                        cache_control: None,
                    };
                    pruned += 1;
                }
            }
        }
        pruned
    }

    fn prune_tool_results(&mut self, keep_recent: usize) -> usize {
        let mut remaining_keep = keep_recent;
        let mut pruned = 0;
        for message in self.session.messages.iter_mut().rev() {
            for block in message.content.iter_mut().rev() {
                if let ContentBlock::ToolResult {
                    content, is_error, ..
                } = block
                {
                    if remaining_keep > 0 {
                        remaining_keep -= 1;
                        continue;
                    }
                    if content.starts_with("[tool result pruned") {
                        continue;
                    }
                    *content = "[tool result pruned to save context]".to_string();
                    *is_error = None;
                    pruned += 1;
                }
            }
        }
        pruned
    }

    /// Оставляет только последние `keep_recent` видимых сообщений, удаляя
    /// более старые turn-группы целиком. Срез сдвигается к ближайшему
    /// пользовательскому сообщению, чтобы провайдерский transcript начинался
    /// корректно.
    fn prune_turns(&mut self, keep_recent: usize) -> usize {
        let targets = self.session.rewind_target_stored_indices();
        if targets.len() <= keep_recent {
            return 0;
        }
        let start_visible = targets.len() - keep_recent;
        let mut start_index = targets[start_visible];
        while start_index < self.session.messages.len()
            && self.session.messages[start_index].role != Role::User
        {
            start_index += 1;
        }
        if start_index >= self.session.messages.len() {
            return 0;
        }
        let removed = self.session.messages.len() - start_index;
        let rest = self.session.messages[start_index..].to_vec();
        self.session.replace_messages(rest);
        removed
    }
}
