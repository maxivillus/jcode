use super::Agent;
use crate::context::{ContextBudget, ContextComponentHashes, ContextRevision, sha256_hex};
use crate::context_controller::{
    ContextActionKind, ContextActionOutcome, ContextActionRequest, ContextPreflightAction,
    ContextPreflightPlan,
};
use crate::message::{ContentBlock, Message, Role, ToolDefinition};
use crate::prompt::SplitSystemPrompt;
use crate::session::StoredMessage;
use crate::skill_runtime::SkillRuntimeRegistry;
use serde::Serialize;

const RESERVED_OUTPUT_TOKENS: usize = 4096;
const SAFETY_MARGIN_TOKENS: usize = 512;
const IMAGE_TOKEN_ESTIMATE: usize = 256;
const OPAQUE_NATIVE_ITEM_TOKEN_ESTIMATE: usize = 64;

/// Начало сообщения memory-инъекции, как его собирает memory runtime.
pub(super) const MEMORY_INJECTION_MARKER: &str = "<system-reminder>\n# Memory\n";

fn serialized_fingerprint<T: Serialize + ?Sized>(value: &T) -> String {
    match serde_json::to_vec(value) {
        Ok(encoded) => sha256_hex(encoded),
        Err(_) => sha256_hex("serialization-error"),
    }
}

fn serialized_token_estimate<T: Serialize + ?Sized>(value: &T) -> usize {
    match serde_json::to_string(value) {
        Ok(encoded) => crate::util::estimate_tokens(&encoded),
        Err(_) => usize::MAX,
    }
}

fn selected_block_fingerprint(
    messages: &[Message],
    mut include: impl FnMut(&Message, &ContentBlock) -> bool,
) -> String {
    let selected = messages
        .iter()
        .flat_map(|message| message.content.iter().map(move |block| (message, block)))
        .filter(|(message, block)| include(message, block))
        .map(|(_, block)| block)
        .collect::<Vec<_>>();
    serialized_fingerprint(&selected)
}

fn memory_fingerprint(messages: &[Message]) -> String {
    selected_block_fingerprint(messages, |message, block| {
        message.role == Role::User
            && matches!(
                block,
                ContentBlock::Text { text, .. } if text.starts_with(MEMORY_INJECTION_MARKER)
            )
    })
}

fn image_fingerprint(messages: &[Message]) -> String {
    selected_block_fingerprint(messages, |_, block| {
        matches!(block, ContentBlock::Image { .. })
    })
}

fn tool_result_fingerprint(messages: &[Message]) -> String {
    selected_block_fingerprint(messages, |_, block| {
        matches!(block, ContentBlock::ToolResult { .. })
    })
}

/// Оценка токенов одного блока provider-запроса.
///
/// Один и тот же расчёт используют preflight, обрезка контекста и её
/// предварительный просмотр, поэтому числа `preview` и применённой операции
/// совпадают.
pub(super) fn block_token_estimate(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text, .. } | ContentBlock::Reasoning { text } => {
            crate::util::estimate_tokens(text)
        }
        ContentBlock::ReasoningTrace { .. } => 0,
        ContentBlock::AnthropicThinking {
            thinking,
            signature,
        } => crate::util::estimate_tokens(thinking)
            .saturating_add(crate::util::estimate_tokens(signature)),
        ContentBlock::OpenAIReasoning {
            id,
            summary,
            status,
            ..
        } => crate::util::estimate_tokens(id)
            .saturating_add(
                summary
                    .iter()
                    .map(|text| crate::util::estimate_tokens(text))
                    .fold(0usize, |total, tokens| total.saturating_add(tokens)),
            )
            .saturating_add(
                status
                    .as_deref()
                    .map(crate::util::estimate_tokens)
                    .unwrap_or_default(),
            ),
        ContentBlock::ToolUse {
            id,
            name,
            input,
            thought_signature,
        } => crate::util::estimate_tokens(id)
            .saturating_add(crate::util::estimate_tokens(name))
            .saturating_add(serialized_token_estimate(input))
            .saturating_add(
                thought_signature
                    .as_deref()
                    .map(crate::util::estimate_tokens)
                    .unwrap_or_default(),
            ),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => crate::util::estimate_tokens(tool_use_id)
            .saturating_add(crate::util::estimate_tokens(content)),
        ContentBlock::Image { media_type, .. } => {
            IMAGE_TOKEN_ESTIMATE.saturating_add(crate::util::estimate_tokens(media_type))
        }
        ContentBlock::OpenAICompaction { .. } => OPAQUE_NATIVE_ITEM_TOKEN_ESTIMATE,
    }
}

/// Оценка токенов сообщения с учётом накладных расходов на сообщение.
pub(super) fn message_token_estimate(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|message| content_token_estimate(&message.content))
        .fold(0usize, |total, tokens| total.saturating_add(tokens))
}

/// Оценка токенов сохранённого сообщения теми же правилами.
pub(super) fn stored_message_token_estimate(message: &StoredMessage) -> usize {
    content_token_estimate(&message.content)
}

fn content_token_estimate(content: &[ContentBlock]) -> usize {
    content
        .iter()
        .map(block_token_estimate)
        .fold(4usize, |total, tokens| total.saturating_add(tokens))
}

fn preflight_action_name(action: ContextPreflightAction) -> &'static str {
    match action {
        ContextPreflightAction::Send => "send",
        ContextPreflightAction::Refresh => "refresh",
        ContextPreflightAction::Compact => "compact",
        ContextPreflightAction::RefreshThenCompact => "refresh_then_compact",
    }
}

fn context_action_name(action: ContextActionKind) -> &'static str {
    match action {
        ContextActionKind::Refresh => "refresh",
        ContextActionKind::Compact => "compact",
        ContextActionKind::ResetProvider => "reset_provider",
        ContextActionKind::Export => "export",
        ContextActionKind::Prune => "prune",
        ContextActionKind::UndoPrune => "undo_prune",
    }
}

fn context_action_status(outcome: &ContextActionOutcome) -> &'static str {
    match outcome {
        ContextActionOutcome::Completed { .. } => "completed",
        ContextActionOutcome::Skipped { .. } => "skipped",
        ContextActionOutcome::Failed { .. } => "failed",
        ContextActionOutcome::Rejected { .. } => "rejected",
    }
}

impl Agent {
    pub(super) fn trace_context(
        &self,
        trace: bool,
        plan: &ContextPreflightPlan,
        messages: &[Message],
        split_prompt: &SplitSystemPrompt,
        tools: &[ToolDefinition],
    ) {
        if !trace {
            return;
        }
        let system_prompt_estimated_tokens = split_prompt.estimated_tokens();
        let tool_definition_estimated_tokens =
            ToolDefinition::aggregate_prompt_token_estimate(tools);
        let (image_count, image_estimated_tokens) = messages
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::Image { .. } => Some(block_token_estimate(block)),
                _ => None,
            })
            .fold((0usize, 0usize), |(count, tokens), estimate| {
                (count.saturating_add(1), tokens.saturating_add(estimate))
            });
        let provider_context_limit = self.provider.context_window();
        crate::logging::event_debug(
            "CONTEXT_METRICS",
            vec![
                ("revision".to_string(), plan.revision.0.to_string()),
                (
                    "estimated_input_tokens".to_string(),
                    plan.estimated_input_tokens.to_string(),
                ),
                (
                    "message_estimated_tokens".to_string(),
                    plan.estimated_input_tokens
                        .saturating_sub(system_prompt_estimated_tokens)
                        .saturating_sub(tool_definition_estimated_tokens)
                        .to_string(),
                ),
                (
                    "system_prompt_estimated_tokens".to_string(),
                    system_prompt_estimated_tokens.to_string(),
                ),
                ("tool_definition_count".to_string(), tools.len().to_string()),
                (
                    "tool_definition_estimated_tokens".to_string(),
                    tool_definition_estimated_tokens.to_string(),
                ),
                ("image_count".to_string(), image_count.to_string()),
                (
                    "image_estimated_tokens".to_string(),
                    image_estimated_tokens.to_string(),
                ),
                (
                    "provider_context_limit".to_string(),
                    provider_context_limit.to_string(),
                ),
                (
                    "max_input_tokens".to_string(),
                    plan.max_input_tokens.to_string(),
                ),
            ],
        );
        eprintln!(
            "[trace] context_metrics revision={} estimated_input={} message_estimated={} system_prompt_estimated={} tool_definition_count={} tool_definition_estimated={} image_count={} image_estimated_tokens={} provider_context_limit={} max_input_tokens={}",
            plan.revision.0,
            plan.estimated_input_tokens,
            plan.estimated_input_tokens
                .saturating_sub(system_prompt_estimated_tokens)
                .saturating_sub(tool_definition_estimated_tokens),
            system_prompt_estimated_tokens,
            tools.len(),
            tool_definition_estimated_tokens,
            image_count,
            image_estimated_tokens,
            provider_context_limit,
            plan.max_input_tokens,
        );
    }

    fn refresh_static_prompt_binding(&mut self, static_part: &str) {
        let current_hash = sha256_hex(static_part.as_bytes());
        let has_provider_session =
            self.provider_session_id.is_some() || self.session.provider_session_id.is_some();
        let changed = self
            .last_provider_static_prompt_hash
            .as_deref()
            .is_some_and(|previous| previous != current_hash);
        let restored_without_binding =
            has_provider_session && self.last_provider_static_prompt_hash.is_none();

        if changed || restored_without_binding {
            self.invalidate_provider_context("static system prompt changed");
        }

        self.last_provider_static_prompt_hash = Some(current_hash);
    }

    /// Сбрасывает resumable provider session, когда tools, skills или
    /// AGENTS snapshot изменились с прошлого preflight.
    ///
    /// Static prompt уже отслеживается отдельно. Без этой проверки upstream
    /// продолжал бы разговор со старым набором инструментов: провайдер может
    /// вернуть вызов инструмента, которого в новом наборе уже нет.
    pub(super) fn refresh_components_binding(&mut self, components: &ContextComponentHashes) {
        let fingerprint = format!(
            "{}|{}|{}",
            components.agents.as_deref().unwrap_or(""),
            components.skills.as_deref().unwrap_or(""),
            components.tools.as_deref().unwrap_or(""),
        );
        let has_provider_session =
            self.provider_session_id.is_some() || self.session.provider_session_id.is_some();
        let changed = self
            .last_provider_components_fingerprint
            .as_deref()
            .is_some_and(|previous| previous != fingerprint);
        if changed && has_provider_session {
            self.invalidate_provider_context("tools, skills or AGENTS snapshot changed");
        }
        self.last_provider_components_fingerprint = Some(fingerprint);
    }

    /// Снимает provider-facing snapshot перед запросом.
    ///
    /// Здесь проверяется привязка static prompt к provider session. При
    /// рассогласовании старая resumable session сбрасывается. Controller
    /// обновляет локальную revision и запоминает estimate. Если registry
    /// отсутствует или повреждён, текущий transcript flow сохраняется.
    pub(super) fn prepare_context_preflight(
        &mut self,
        messages: &[Message],
        tools: &[ToolDefinition],
        split_prompt: &SplitSystemPrompt,
    ) -> ContextPreflightPlan {
        self.refresh_static_prompt_binding(&split_prompt.static_part);
        let system_prompt = if split_prompt.dynamic_part.is_empty() {
            split_prompt.static_part.clone()
        } else {
            format!(
                "{}\n\n{}",
                split_prompt.static_part, split_prompt.dynamic_part
            )
        };
        let skill_runtime_fingerprint = match SkillRuntimeRegistry::load_default() {
            Ok(registry) => Some(registry.fingerprint()),
            Err(_) => {
                crate::logging::warn(
                    "Skill runtime registry ignored during context preflight: invalid or unavailable",
                );
                None
            }
        };
        let components = ContextComponentHashes {
            system_prompt: Some(sha256_hex(system_prompt.as_bytes())),
            agents: self.agents_md_snapshot.0.as_deref().map(sha256_hex),
            skills: skill_runtime_fingerprint,
            memory: Some(memory_fingerprint(messages)),
            tools: Some(serialized_fingerprint(tools)),
            messages: Some(serialized_fingerprint(messages)),
            images: Some(image_fingerprint(messages)),
            tool_results: Some(tool_result_fingerprint(messages)),
        };
        self.refresh_components_binding(&components);
        let message_tokens = message_token_estimate(messages);
        let system_prompt_estimated_tokens = split_prompt.estimated_tokens();
        let tool_definition_estimated_tokens =
            ToolDefinition::aggregate_prompt_token_estimate(tools);
        let estimated_input_tokens = system_prompt_estimated_tokens
            .saturating_add(tool_definition_estimated_tokens)
            .saturating_add(message_tokens);
        let provider_context_limit = self.provider.context_window();
        let reserved_output_tokens = RESERVED_OUTPUT_TOKENS.min(provider_context_limit / 4);
        let safety_margin_tokens = SAFETY_MARGIN_TOKENS.min(
            provider_context_limit
                .saturating_sub(reserved_output_tokens)
                .saturating_div(16),
        );
        let budget = ContextBudget {
            provider_context_limit,
            reserved_output_tokens,
            safety_margin_tokens,
            estimated_input_tokens,
        };
        let provider_generation = super::stable_hash_str(&format!(
            "{}:{}:{}",
            self.provider.name(),
            self.provider.model(),
            provider_context_limit
        ));
        let plan = self
            .context_controller
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .prepare(&budget, components, provider_generation);

        // Снимок проекции обрезки нужен, чтобы `preview` отвечал по текущему
        // транскрипту, не читая его через контроллер.
        self.record_prune_projections(plan.revision, message_tokens);

        crate::logging::event_debug(
            "CONTEXT_PREFLIGHT",
            vec![
                (
                    "action".to_string(),
                    preflight_action_name(plan.action).to_string(),
                ),
                ("revision".to_string(), plan.revision.0.to_string()),
                (
                    "estimated_input_tokens".to_string(),
                    plan.estimated_input_tokens.to_string(),
                ),
                (
                    "message_estimated_tokens".to_string(),
                    message_tokens.to_string(),
                ),
                (
                    "system_prompt_estimated_tokens".to_string(),
                    system_prompt_estimated_tokens.to_string(),
                ),
                (
                    "tool_definition_estimated_tokens".to_string(),
                    tool_definition_estimated_tokens.to_string(),
                ),
                ("message_count".to_string(), messages.len().to_string()),
                ("tool_count".to_string(), tools.len().to_string()),
                (
                    "provider_context_limit".to_string(),
                    provider_context_limit.to_string(),
                ),
                (
                    "max_input_tokens".to_string(),
                    plan.max_input_tokens.to_string(),
                ),
                (
                    "needs_refresh".to_string(),
                    plan.needs_refresh().to_string(),
                ),
                (
                    "needs_compaction".to_string(),
                    plan.needs_compaction().to_string(),
                ),
                (
                    "provider_session_present".to_string(),
                    self.provider_session_id.is_some().to_string(),
                ),
            ],
        );

        crate::logging::info(&format!(
            "Context preflight: action={:?} revision={} estimated={} max={} provider_limit={}",
            plan.action,
            plan.revision.0,
            plan.estimated_input_tokens,
            plan.max_input_tokens,
            provider_context_limit
        ));
        if plan.needs_compaction() {
            crate::logging::warn(&format!(
                "Context preflight exceeded budget at revision {}: estimated {} > max {}; existing compaction path will be used",
                plan.revision.0, plan.estimated_input_tokens, plan.max_input_tokens
            ));
        }
        plan
    }

    /// Проверяет revision непосредственно перед отправкой provider request.
    ///
    /// Между preflight и вызовом provider могут прийти внешние действия над
    /// context. В таком случае локальный snapshot уже нельзя отправлять:
    /// следующий проход соберёт новый transcript и новый preflight.
    pub(super) fn final_provider_revision_gate(&self, revision: ContextRevision) -> bool {
        let current = self
            .context_controller
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .manifest()
            .revision;
        if current == revision {
            return true;
        }
        crate::logging::warn(&format!(
            "Skipping provider request for stale context revision {} (current {})",
            revision.0, current.0
        ));
        crate::logging::event_warn(
            "CONTEXT_PROVIDER_REQUEST_REJECTED",
            vec![
                ("reason".to_string(), "stale_revision".to_string()),
                ("requested_revision".to_string(), revision.0.to_string()),
                ("current_revision".to_string(), current.0.to_string()),
            ],
        );
        false
    }

    /// Фиксирует завершение provider-ответа: сверяет revision и пишет usage.
    ///
    /// Возвращает `false` для устаревшего ответа: usage не записывается, а
    /// resumable provider session сбрасывается. Ответ без usage тоже
    /// проверяется, поэтому окно устаревания не зависит от провайдера.
    /// Вызывающий код обязан при `false` отбросить содержимое ответа и его
    /// tool calls, не сохраняя и не исполняя их.
    pub(super) fn record_context_usage(
        &mut self,
        revision: ContextRevision,
        observed_input_tokens: Option<u64>,
    ) -> bool {
        if !self.note_provider_response_revision(revision) {
            return false;
        }
        if let Some(observed_input_tokens) = observed_input_tokens {
            let observed = usize::try_from(observed_input_tokens).unwrap_or(usize::MAX);
            self.context_controller
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .record_observed_input_tokens(revision, observed);
        }
        crate::logging::event_debug(
            "CONTEXT_PROVIDER_RESPONSE_ACCEPTED",
            vec![
                ("revision".to_string(), revision.0.to_string()),
                (
                    "observed_input_tokens".to_string(),
                    observed_input_tokens
                        .map(|tokens| tokens.to_string())
                        .unwrap_or_else(|| "none".to_string()),
                ),
            ],
        );
        true
    }

    /// Проверяет, что provider-ответ всё ещё относится к текущей revision.
    ///
    /// Возвращает `false`, если контекст изменился, пока запрос был в полёте:
    /// такой ответ описывает транскрипт, которого больше нет. Resumable
    /// provider session сбрасывается, чтобы устаревший upstream-разговор не
    /// продолжался как актуальный, а revision сохраняется для наблюдаемости.
    pub(super) fn note_provider_response_revision(&mut self, revision: ContextRevision) -> bool {
        let current = self
            .context_controller
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .manifest()
            .revision;
        if current == revision {
            return true;
        }
        crate::logging::warn(&format!(
            "Ignoring late provider response for stale context revision {} (current {})",
            revision.0, current.0
        ));
        crate::logging::event_warn(
            "CONTEXT_PROVIDER_RESPONSE_REJECTED",
            vec![
                ("reason".to_string(), "stale_revision".to_string()),
                ("response_revision".to_string(), revision.0.to_string()),
                ("current_revision".to_string(), current.0.to_string()),
            ],
        );
        self.last_stale_provider_revision = Some(revision.0);
        self.invalidate_provider_context("late provider response for a stale context revision");
        false
    }

    /// Revision устаревшего provider-ответа, который уже нельзя применять.
    ///
    /// Ответ устарел, если контекст изменился, пока запрос был в полёте:
    /// такой ответ описывает транскрипт, которого больше нет.
    pub(crate) fn last_stale_provider_revision(&self) -> Option<u64> {
        self.last_stale_provider_revision
    }

    /// Применяет заявки модели к контексту на безопасной границе turn-а.
    ///
    /// Вызывается до сборки provider request: refresh учитывается preflight-ом,
    /// compact уменьшает активную историю, reset-provider сбрасывает
    /// resumable provider session. Заявка с устаревшей revision отклоняется.
    pub(super) fn apply_pending_context_actions(&mut self) {
        let (pending, current_revision) = {
            let mut controller = self
                .context_controller
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (
                controller.take_pending_actions(),
                controller.manifest().revision,
            )
        };
        for request in pending {
            let outcome = self.apply_context_action(&request, current_revision);
            crate::logging::event_debug(
                "CONTEXT_ACTION_RESULT",
                vec![
                    (
                        "action".to_string(),
                        context_action_name(request.action).to_string(),
                    ),
                    ("sequence".to_string(), request.sequence.to_string()),
                    (
                        "base_revision".to_string(),
                        request.base_revision.0.to_string(),
                    ),
                    (
                        "boundary_revision".to_string(),
                        current_revision.0.to_string(),
                    ),
                    (
                        "status".to_string(),
                        context_action_status(&outcome).to_string(),
                    ),
                ],
            );
            self.context_controller
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .record_action_outcome(request, outcome);
        }
    }

    fn apply_context_action(
        &mut self,
        request: &ContextActionRequest,
        current_revision: ContextRevision,
    ) -> ContextActionOutcome {
        if request.base_revision != current_revision {
            return ContextActionOutcome::Rejected {
                reason: format!(
                    "request was based on revision {} but context is at {}",
                    request.base_revision.0, current_revision.0
                ),
            };
        }
        match request.action {
            ContextActionKind::Refresh => ContextActionOutcome::Completed {
                detail: "preflight refreshes component hashes for the next request".to_string(),
            },
            ContextActionKind::Compact => self.compact_for_model_request(),
            ContextActionKind::ResetProvider => {
                self.reset_provider_session();
                ContextActionOutcome::Completed {
                    detail: "provider session reset; next request sends full context".to_string(),
                }
            }
            ContextActionKind::Export => ContextActionOutcome::Completed {
                detail: "export is read-only; call context_control export for the manifest"
                    .to_string(),
            },
            ContextActionKind::Prune => match request.prune.clone() {
                Some(spec) => self.prune_for_model_request(spec),
                None => ContextActionOutcome::Rejected {
                    reason: "prune request is missing its spec".to_string(),
                },
            },
            ContextActionKind::UndoPrune => self.undo_prune_for_model_request(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_event_labels_are_stable() {
        assert_eq!(preflight_action_name(ContextPreflightAction::Send), "send");
        assert_eq!(
            preflight_action_name(ContextPreflightAction::RefreshThenCompact),
            "refresh_then_compact"
        );
        assert_eq!(
            context_action_name(ContextActionKind::ResetProvider),
            "reset_provider"
        );
        assert_eq!(
            context_action_name(ContextActionKind::UndoPrune),
            "undo_prune"
        );
    }

    #[test]
    fn context_event_statuses_cover_all_outcomes() {
        assert_eq!(
            context_action_status(&ContextActionOutcome::Completed {
                detail: "ok".to_string(),
            }),
            "completed"
        );
        assert_eq!(
            context_action_status(&ContextActionOutcome::Skipped {
                reason: "not needed".to_string(),
            }),
            "skipped"
        );
        assert_eq!(
            context_action_status(&ContextActionOutcome::Failed {
                reason: "failed".to_string(),
            }),
            "failed"
        );
        assert_eq!(
            context_action_status(&ContextActionOutcome::Rejected {
                reason: "stale".to_string(),
            }),
            "rejected"
        );
    }
}
