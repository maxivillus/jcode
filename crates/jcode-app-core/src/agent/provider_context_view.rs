use crate::config::ContextRetention;
use crate::message::{
    ContentBlock, Message, Role, cache_relevant_message_hashes, extend_stable_hash,
};

use super::context_control::message_token_estimate;

const RESERVED_OUTPUT_TOKENS: usize = 4096;
const SAFETY_MARGIN_TOKENS: usize = 512;
const AUTOMATIC_TRIGGER_NUMERATOR: usize = 3;
const AUTOMATIC_TRIGGER_DENOMINATOR: usize = 5;
const AUTOMATIC_REBUILD_NUMERATOR: usize = 4;
const AUTOMATIC_REBUILD_DENOMINATOR: usize = 5;
const RECENT_TURN_GROUPS: usize = 8;
const MAX_TURN_GROUPS_BEFORE_PROJECTION: usize = 24;
const MAX_PROJECTED_TURN_GROUPS_BEFORE_REBUILD: usize = 16;
const SUMMARY_MAX_ITEMS: usize = 8;
const SUMMARY_FIELD_CHARS: usize = 96;
const MEMORY_INJECTION_PREFIX: &str = "<system-reminder>\n# Memory\n";
const SYSTEM_REMINDER_PREFIX: &str = "<system-reminder>";
const IMAGE_OMITTED_NOTE: &str =
    "[older image omitted automatically; canonical session retains it]";
const TOOL_RESULT_OMITTED_NOTE: &str =
    "[older tool result omitted automatically; canonical session retains it]";
const MEMORY_OMITTED_NOTE: &str =
    "[older memory injection omitted automatically; canonical session retains it]";

#[derive(Debug, Clone, Copy)]
struct RetentionSettings {
    trigger_numerator: usize,
    trigger_denominator: usize,
    recent_turn_groups: usize,
    max_turn_groups_before_projection: usize,
    max_projected_turn_groups_before_rebuild: usize,
    workflow_tail_groups: usize,
}

fn retention_settings(retention: ContextRetention) -> RetentionSettings {
    match retention {
        ContextRetention::High => RetentionSettings {
            trigger_numerator: 9,
            trigger_denominator: 10,
            recent_turn_groups: 16,
            max_turn_groups_before_projection: 48,
            max_projected_turn_groups_before_rebuild: 32,
            workflow_tail_groups: 16,
        },
        ContextRetention::Mid => RetentionSettings {
            trigger_numerator: 4,
            trigger_denominator: 5,
            recent_turn_groups: 12,
            max_turn_groups_before_projection: 32,
            max_projected_turn_groups_before_rebuild: 24,
            workflow_tail_groups: 8,
        },
        ContextRetention::Low => RetentionSettings {
            trigger_numerator: AUTOMATIC_TRIGGER_NUMERATOR,
            trigger_denominator: AUTOMATIC_TRIGGER_DENOMINATOR,
            recent_turn_groups: RECENT_TURN_GROUPS,
            max_turn_groups_before_projection: MAX_TURN_GROUPS_BEFORE_PROJECTION,
            max_projected_turn_groups_before_rebuild: MAX_PROJECTED_TURN_GROUPS_BEFORE_REBUILD,
            workflow_tail_groups: 1,
        },
        ContextRetention::Disabled => RetentionSettings {
            trigger_numerator: 1,
            trigger_denominator: 1,
            recent_turn_groups: usize::MAX,
            max_turn_groups_before_projection: usize::MAX,
            max_projected_turn_groups_before_rebuild: usize::MAX,
            workflow_tail_groups: usize::MAX,
        },
    }
}

type RenderedProjection = (Vec<Message>, usize, usize, usize);

#[derive(Debug, Clone, Copy)]
struct Boundary {
    message_count: usize,
    prefix_hash: u64,
}

#[derive(Debug, Clone, Copy)]
struct ViewFingerprint {
    mode: &'static str,
    retention: ContextRetention,
    message_count: usize,
    prefix_hash: u64,
}

#[derive(Debug, Clone, Default)]
pub struct WorkflowContextProjector {
    stable_boundary: Option<Boundary>,
    last_view: Option<ViewFingerprint>,
}

#[derive(Debug, Clone)]
pub struct WorkflowContextView {
    pub messages: Vec<Message>,
    pub mode: &'static str,
    pub retention: ContextRetention,
    pub representation_changed: bool,
    pub active: bool,
    pub source_version: u64,
    pub view_version: u64,
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub before_turn_groups: usize,
    pub after_turn_groups: usize,
    pub excluded_messages: usize,
    pub excluded_turn_groups: usize,
    pub summary_messages: usize,
    pub summary_source_version: Option<u64>,
    pub reason: String,
}

impl WorkflowContextProjector {
    pub fn reset(&mut self) {
        self.stable_boundary = None;
        self.last_view = None;
    }

    pub fn project(
        &mut self,
        messages: &[Message],
        provider_context_limit: usize,
        system_prompt_tokens: usize,
        tool_definition_tokens: usize,
    ) -> WorkflowContextView {
        self.project_with_retention(
            messages,
            provider_context_limit,
            system_prompt_tokens,
            tool_definition_tokens,
            ContextRetention::Low,
        )
    }

    pub fn project_with_retention(
        &mut self,
        messages: &[Message],
        provider_context_limit: usize,
        system_prompt_tokens: usize,
        tool_definition_tokens: usize,
        retention: ContextRetention,
    ) -> WorkflowContextView {
        if retention == ContextRetention::Disabled {
            return self.unmodified_view(
                messages,
                retention,
                "disabled",
                false,
                "retention_disabled",
            );
        }

        let settings = retention_settings(retention);
        let before_tokens = message_token_estimate(messages);
        let before_turn_groups = turn_group_ranges(messages).len();
        let source_prefix_hashes = rolling_prefix_hashes(messages);
        let source_version = source_prefix_hashes.last().copied().unwrap_or(0);
        let usable_budget = provider_context_limit
            .saturating_sub(system_prompt_tokens)
            .saturating_sub(tool_definition_tokens)
            .saturating_sub(RESERVED_OUTPUT_TOKENS)
            .saturating_sub(SAFETY_MARGIN_TOKENS);
        let trigger_tokens = budget_fraction(
            usable_budget,
            settings.trigger_numerator,
            settings.trigger_denominator,
        );
        let rebuild_tokens = budget_fraction(
            usable_budget,
            AUTOMATIC_REBUILD_NUMERATOR,
            AUTOMATIC_REBUILD_DENOMINATOR,
        );
        let pressure = before_tokens >= trigger_tokens
            || before_turn_groups > settings.max_turn_groups_before_projection;

        if self
            .last_view
            .is_some_and(|previous| previous.retention != retention)
        {
            self.stable_boundary = None;
        }

        let mut reason = "not_needed".to_string();
        let mut cutoff = self
            .stable_boundary
            .and_then(|boundary| find_boundary(&source_prefix_hashes, boundary));
        let mut rendered =
            cutoff.map(|value| render_projection(messages, value, &source_prefix_hashes));

        if let (Some(existing_cutoff), Some(existing_rendered)) = (cutoff, rendered.as_ref()) {
            let projected_turn_groups = turn_group_ranges(&existing_rendered.0).len();
            let needs_rebuild = existing_rendered.0.len() > messages.len()
                || message_token_estimate(&existing_rendered.0) > rebuild_tokens
                || projected_turn_groups > settings.max_projected_turn_groups_before_rebuild;
            if needs_rebuild {
                cutoff = None;
                rendered = None;
                reason = "stable_view_rebuild".to_string();
            } else {
                reason = format!("stable_tail_append@{existing_cutoff}");
            }
        }

        if cutoff.is_none() && pressure {
            if let Some((selected_cutoff, selected_rendered)) = choose_projection(
                messages,
                trigger_tokens,
                &source_prefix_hashes,
                settings.recent_turn_groups,
            ) {
                cutoff = Some(selected_cutoff);
                rendered = Some(selected_rendered);
                reason = "automatic_tail_and_turns".to_string();
            } else {
                reason = "current_turn_exceeds_budget".to_string();
            }
        }

        let (output, excluded_messages, excluded_turn_groups, summary_messages) =
            rendered.unwrap_or_else(|| (messages.to_vec(), 0, 0, 0));
        let after_tokens = message_token_estimate(&output);
        let after_turn_groups = turn_group_ranges(&output).len();
        let view_prefix_hashes = rolling_prefix_hashes(&output);
        let view_version = view_prefix_hashes.last().copied().unwrap_or(0);
        let summary_source_version = if summary_messages > 0 {
            Some(
                cutoff
                    .and_then(|value| value.checked_sub(1))
                    .and_then(|index| source_prefix_hashes.get(index).copied())
                    .unwrap_or(0),
            )
        } else {
            None
        };
        let representation_changed = self.last_view.is_some_and(|previous| {
            previous.mode != "transcript"
                || previous.retention != retention
                || previous.message_count > view_prefix_hashes.len()
                || view_prefix_hashes
                    .get(previous.message_count.saturating_sub(1))
                    .copied()
                    .unwrap_or(0)
                    != previous.prefix_hash
        });

        if let Some(selected_cutoff) = cutoff {
            let prefix_hash = selected_cutoff
                .checked_sub(1)
                .and_then(|index| source_prefix_hashes.get(index).copied())
                .unwrap_or(0);
            self.stable_boundary = Some(Boundary {
                message_count: selected_cutoff,
                prefix_hash,
            });
        } else if !pressure {
            self.stable_boundary = None;
        }
        self.last_view = Some(ViewFingerprint {
            mode: "transcript",
            retention,
            message_count: view_prefix_hashes.len(),
            prefix_hash: view_version,
        });

        WorkflowContextView {
            messages: output,
            mode: "transcript",
            retention,
            representation_changed,
            active: cutoff.is_some(),
            source_version,
            view_version,
            before_tokens,
            after_tokens,
            before_turn_groups,
            after_turn_groups,
            excluded_messages,
            excluded_turn_groups,
            summary_messages,
            summary_source_version,
            reason,
        }
    }

    fn unmodified_view(
        &mut self,
        messages: &[Message],
        retention: ContextRetention,
        mode: &'static str,
        active: bool,
        reason: &str,
    ) -> WorkflowContextView {
        let source_prefix_hashes = rolling_prefix_hashes(messages);
        let source_version = source_prefix_hashes.last().copied().unwrap_or(0);
        let view_version = source_version;
        let representation_changed = self.last_view.is_some_and(|previous| {
            previous.mode != mode
                || previous.retention != retention
                || previous.message_count > messages.len()
                || (previous.message_count > 0
                    && source_prefix_hashes
                        .get(previous.message_count.saturating_sub(1))
                        .copied()
                        .unwrap_or(0)
                        != previous.prefix_hash)
        });
        self.stable_boundary = None;
        self.last_view = Some(ViewFingerprint {
            mode,
            retention,
            message_count: messages.len(),
            prefix_hash: view_version,
        });
        WorkflowContextView {
            messages: messages.to_vec(),
            mode,
            retention,
            representation_changed,
            active,
            source_version,
            view_version,
            before_tokens: message_token_estimate(messages),
            after_tokens: message_token_estimate(messages),
            before_turn_groups: turn_group_ranges(messages).len(),
            after_turn_groups: turn_group_ranges(messages).len(),
            excluded_messages: 0,
            excluded_turn_groups: 0,
            summary_messages: 0,
            summary_source_version: None,
            reason: reason.to_string(),
        }
    }

    /// Projects an active workflow to a bounded recent tail.
    ///
    /// The immutable workflow specification and bounded `WorkflowRunState` are
    /// carried by the split system prompt. The message view keeps only the
    /// current request and its tool observations, while the canonical session
    /// remains unchanged for audit and recovery.
    pub fn project_workflow_context(&mut self, messages: &[Message]) -> WorkflowContextView {
        self.project_workflow_context_with_retention(messages, ContextRetention::Low)
    }

    pub fn project_workflow_context_with_retention(
        &mut self,
        messages: &[Message],
        retention: ContextRetention,
    ) -> WorkflowContextView {
        if retention == ContextRetention::Disabled {
            return self.unmodified_view(
                messages,
                retention,
                "disabled",
                false,
                "retention_disabled",
            );
        }

        let settings = retention_settings(retention);
        let before_tokens = message_token_estimate(messages);
        let before_turn_groups = turn_group_ranges(messages).len();
        let source_version = rolling_prefix_hashes(messages).last().copied().unwrap_or(0);
        let ranges = turn_group_ranges(messages);
        let first_kept_group = ranges
            .len()
            .saturating_sub(settings.workflow_tail_groups.min(ranges.len()));
        let latest_start = ranges
            .get(first_kept_group)
            .map(|(start, _)| balanced_suffix_start(messages, *start));
        let (output, reason) = match latest_start {
            Some(start) if start < messages.len() => (
                messages[start..].to_vec(),
                if settings.workflow_tail_groups == 1 {
                    "state_first_latest_turn".to_string()
                } else {
                    "state_first_recent_tail".to_string()
                },
            ),
            _ => (
                messages.to_vec(),
                "state_first_no_turn_fallback".to_string(),
            ),
        };
        let after_tokens = message_token_estimate(&output);
        let after_turn_groups = turn_group_ranges(&output).len();
        let view_prefix_hashes = rolling_prefix_hashes(&output);
        let view_version = view_prefix_hashes.last().copied().unwrap_or(0);
        let excluded_messages = messages.len().saturating_sub(output.len());
        let excluded_turn_groups = before_turn_groups.saturating_sub(after_turn_groups);
        let representation_changed = self.last_view.is_some_and(|previous| {
            previous.mode != "state_first"
                || previous.retention != retention
                || previous.message_count > view_prefix_hashes.len()
                || view_prefix_hashes
                    .get(previous.message_count.saturating_sub(1))
                    .copied()
                    .unwrap_or(0)
                    != previous.prefix_hash
        });

        self.stable_boundary = None;
        self.last_view = Some(ViewFingerprint {
            mode: "state_first",
            retention,
            message_count: view_prefix_hashes.len(),
            prefix_hash: view_version,
        });

        WorkflowContextView {
            messages: output,
            mode: "state_first",
            retention,
            representation_changed,
            active: true,
            source_version,
            view_version,
            before_tokens,
            after_tokens,
            before_turn_groups,
            after_turn_groups,
            excluded_messages,
            excluded_turn_groups,
            summary_messages: 0,
            summary_source_version: None,
            reason,
        }
    }

    /// Compatibility wrapper for callers using the earlier algorithm name.
    #[deprecated(note = "use project_workflow_context")]
    pub fn project_state_first(&mut self, messages: &[Message]) -> WorkflowContextView {
        self.project_workflow_context(messages)
    }
}

/// Compatibility name retained for transcript-oriented callers.
pub type ProviderContextViewState = WorkflowContextProjector;
/// Compatibility result name retained for downstream callers.
pub type ProviderContextViewResult = WorkflowContextView;

fn budget_fraction(budget: usize, numerator: usize, denominator: usize) -> usize {
    let calculated = budget.saturating_mul(numerator) / denominator;
    calculated.max(1)
}

fn rolling_prefix_hashes(messages: &[Message]) -> Vec<u64> {
    let mut hashes = Vec::with_capacity(messages.len());
    let mut current = None;
    for message_hash in cache_relevant_message_hashes(messages) {
        current = Some(
            current
                .map(|prefix| extend_stable_hash(prefix, message_hash))
                .unwrap_or(message_hash),
        );
        hashes.push(current.unwrap_or(0));
    }
    hashes
}

fn find_boundary(prefix_hashes: &[u64], boundary: Boundary) -> Option<usize> {
    if boundary.message_count == 0 {
        return Some(0);
    }
    prefix_hashes
        .get(boundary.message_count.saturating_sub(1))
        .filter(|hash| **hash == boundary.prefix_hash)
        .map(|_| boundary.message_count)
}

fn choose_projection(
    messages: &[Message],
    target_tokens: usize,
    source_prefix_hashes: &[u64],
    recent_turn_groups: usize,
) -> Option<(usize, RenderedProjection)> {
    let ranges = turn_group_ranges(messages);
    if ranges.len() <= recent_turn_groups {
        return None;
    }

    let maximum_recent = recent_turn_groups.min(ranges.len().saturating_sub(1));
    for recent_groups in (1..=maximum_recent).rev() {
        let group_index = ranges.len().saturating_sub(recent_groups);
        let candidate = balanced_suffix_start(messages, ranges[group_index].0);
        let rendered = render_projection(messages, candidate, source_prefix_hashes);
        if message_token_estimate(&rendered.0) <= target_tokens || recent_groups == 1 {
            return Some((candidate, rendered));
        }
    }
    None
}

fn render_projection(
    messages: &[Message],
    cutoff: usize,
    source_prefix_hashes: &[u64],
) -> RenderedProjection {
    let cutoff = cutoff.min(messages.len());
    let ranges = turn_group_ranges(messages);
    let excluded_turn_groups = ranges
        .iter()
        .filter(|(start, end)| *start < cutoff && *end <= cutoff)
        .count();
    let mut output = Vec::with_capacity(messages.len());
    let mut omitted = Vec::new();
    let mut summary_index = None;

    for message in messages.iter().take(cutoff) {
        if is_preserved_prefix_message(message) {
            output.push(message.clone());
        } else {
            if summary_index.is_none() {
                summary_index = Some(output.len());
                output.push(empty_summary_message());
            }
            omitted.push(message.clone());
        }
    }

    if let Some(index) = summary_index {
        let source_boundary_version = cutoff
            .checked_sub(1)
            .and_then(|index| source_prefix_hashes.get(index).copied())
            .unwrap_or(0);
        output[index] = build_summary_message(
            &omitted,
            excluded_turn_groups,
            cutoff,
            source_boundary_version,
        );
    }

    let latest_turn_start = ranges
        .last()
        .map(|(start, _)| *start)
        .unwrap_or(messages.len());
    for (index, message) in messages.iter().enumerate().skip(cutoff) {
        let shrink_safe_blocks = index < latest_turn_start;
        output.push(if shrink_safe_blocks {
            shrink_message(message)
        } else {
            message.clone()
        });
    }

    let excluded_messages = omitted.len();
    let summary_messages = usize::from(summary_index.is_some());
    (
        output,
        excluded_messages,
        excluded_turn_groups,
        summary_messages,
    )
}

fn turn_group_ranges(messages: &[Message]) -> Vec<(usize, usize)> {
    let starts: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| is_turn_start(message))
        .map(|(index, _)| index)
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            let end = starts.get(index + 1).copied().unwrap_or(messages.len());
            (*start, end)
        })
        .collect()
}

fn is_turn_start(message: &Message) -> bool {
    message.role == Role::User && !is_tool_result_only(message) && !is_system_reminder(message)
}

fn is_tool_result_only(message: &Message) -> bool {
    let mut has_tool_result = false;
    let mut has_other_content = false;
    for block in &message.content {
        match block {
            ContentBlock::ToolResult { .. } => has_tool_result = true,
            ContentBlock::ReasoningTrace { .. } => {}
            ContentBlock::Text { text, .. } if text.trim().is_empty() => {}
            _ => has_other_content = true,
        }
    }
    has_tool_result && !has_other_content
}

fn is_system_reminder(message: &Message) -> bool {
    message.content.iter().any(|block| {
        matches!(block, ContentBlock::Text { text, .. } if text.trim_start().starts_with(SYSTEM_REMINDER_PREFIX))
    })
}

fn is_memory_injection(message: &Message) -> bool {
    message.content.iter().any(|block| {
        matches!(block, ContentBlock::Text { text, .. } if text.starts_with(MEMORY_INJECTION_PREFIX))
    })
}

fn is_preserved_prefix_message(message: &Message) -> bool {
    if is_memory_injection(message) {
        return false;
    }
    if is_system_reminder(message) {
        return true;
    }
    message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::OpenAICompaction { .. }))
}

fn balanced_suffix_start(messages: &[Message], start: usize) -> usize {
    let mut start = start.min(messages.len());
    loop {
        let mut calls = std::collections::HashSet::new();
        let mut unmatched_results = Vec::new();
        for message in messages.iter().skip(start) {
            for block in &message.content {
                match block {
                    ContentBlock::ToolUse { id, .. } => {
                        calls.insert(id.as_str());
                    }
                    ContentBlock::ToolResult { tool_use_id, .. }
                        if !calls.contains(tool_use_id.as_str()) =>
                    {
                        unmatched_results.push(tool_use_id.as_str());
                    }
                    _ => {}
                }
            }
        }
        if unmatched_results.is_empty() || start == 0 {
            return start;
        }
        let result_id = unmatched_results[0];
        let call_index = messages
            .iter()
            .take(start)
            .enumerate()
            .rev()
            .find(|(_, message)| {
                message.content.iter().any(
                    |block| matches!(block, ContentBlock::ToolUse { id, .. } if id == result_id),
                )
            })
            .map(|(index, _)| index);
        start = call_index.unwrap_or_else(|| start.saturating_sub(1));
    }
}

fn shrink_message(message: &Message) -> Message {
    if is_memory_injection(message) {
        return marker_message(message.role.clone(), MEMORY_OMITTED_NOTE);
    }

    let content = message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Image { .. } => ContentBlock::Text {
                text: IMAGE_OMITTED_NOTE.to_string(),
                cache_control: None,
            },
            ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => ContentBlock::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: TOOL_RESULT_OMITTED_NOTE.to_string(),
                is_error: *is_error,
            },
            other => other.clone(),
        })
        .collect();

    Message {
        role: message.role.clone(),
        content,
        timestamp: message.timestamp,
        tool_duration_ms: message.tool_duration_ms,
    }
}

fn empty_summary_message() -> Message {
    marker_message(Role::User, "[automatic context summary pending]")
}

fn marker_message(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
            cache_control: None,
        }],
        timestamp: None,
        tool_duration_ms: None,
    }
}

fn build_summary_message(
    messages: &[Message],
    excluded_turn_groups: usize,
    cutoff: usize,
    source_boundary_version: u64,
) -> Message {
    let pairs = turn_group_ranges(messages)
        .into_iter()
        .filter_map(|(start, end)| {
            let group = &messages[start..end];
            let question = group.iter().find_map(|message| {
                (message.role == Role::User
                    && !is_system_reminder(message)
                    && !is_tool_result_only(message))
                .then(|| first_text(message))
                .flatten()
            })?;
            let answer = group
                .iter()
                .filter(|message| message.role == Role::Assistant)
                .filter_map(first_text)
                .next_back()?;
            Some((
                clip_text(&question, SUMMARY_FIELD_CHARS),
                clip_text(&answer, SUMMARY_FIELD_CHARS),
            ))
        })
        .collect::<Vec<_>>();
    let selected_pairs = if pairs.len() <= SUMMARY_MAX_ITEMS {
        pairs
    } else {
        let head = SUMMARY_MAX_ITEMS / 2;
        let tail = SUMMARY_MAX_ITEMS.saturating_sub(head);
        pairs
            .iter()
            .take(head)
            .chain(pairs.iter().rev().take(tail).rev())
            .cloned()
            .collect()
    };
    let mut text = format!(
        "[Automatic context summary. Full history remains in the canonical session and was not deleted.]\nSummary semantics: structural completed question-answer pairs; factual truth is not independently verified.\nOmitted completed turn groups: {excluded_turn_groups}. Source boundary: {cutoff}. Source boundary version: {source_boundary_version:016x}.\nIncluded complete question-answer pairs: {}.",
        selected_pairs.len()
    );
    for (question, answer) in selected_pairs {
        text.push_str("\n- Q: ");
        text.push_str(&question);
        text.push_str("\n  A: ");
        text.push_str(&answer);
    }
    marker_message(Role::User, &text)
}

fn first_text(message: &Message) -> Option<String> {
    message.content.iter().find_map(|block| match block {
        ContentBlock::Text { text, .. } if !text.trim().is_empty() => Some(text.clone()),
        _ => None,
    })
}

fn clip_text(text: &str, max_chars: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut clipped = normalized.chars().take(max_chars).collect::<String>();
    if normalized.chars().count() > max_chars {
        clipped.push('…');
    }
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Message {
        Message::user(text)
    }

    fn assistant(text: &str) -> Message {
        Message::assistant_text(text)
    }

    fn tool_call(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: "read".to_string(),
                input: serde_json::json!({"path":"file"}),
                thought_signature: None,
            }],
            timestamp: None,
            tool_duration_ms: None,
        }
    }

    fn tool_result(id: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: "large result".to_string(),
                is_error: None,
            }],
            timestamp: None,
            tool_duration_ms: None,
        }
    }

    fn groups(count: usize) -> Vec<Message> {
        let mut messages = Vec::new();
        for index in 0..count {
            messages.push(user(&format!("question {index} {}", "x".repeat(80))));
            messages.push(assistant(&format!("answer {index} {}", "y".repeat(80))));
        }
        messages
    }

    fn first_gap(messages: &[Message]) -> Option<String> {
        let mut calls = std::collections::HashSet::new();
        for message in messages {
            for block in &message.content {
                match block {
                    ContentBlock::ToolUse { id, .. } => {
                        calls.insert(id.as_str());
                    }
                    ContentBlock::ToolResult { tool_use_id, .. }
                        if !calls.contains(tool_use_id.as_str()) =>
                    {
                        return Some(tool_use_id.clone());
                    }
                    _ => {}
                }
            }
        }
        None
    }

    fn text_contains(messages: &[Message], needle: &str) -> bool {
        messages.iter().any(|message| {
            message.content.iter().any(
                |block| matches!(block, ContentBlock::Text { text, .. } if text.contains(needle)),
            )
        })
    }

    #[test]
    fn long_histories_are_bounded_without_mutating_input() {
        for count in [50, 100, 200] {
            let messages = groups(count);
            let before = serde_json::to_string(&messages).unwrap();
            let mut state = WorkflowContextProjector::default();
            let result = state.project(&messages, 20_000, 0, 0);

            assert!(result.active);
            assert!(result.after_turn_groups <= MAX_PROJECTED_TURN_GROUPS_BEFORE_REBUILD);
            assert!(result.after_tokens < result.before_tokens);
            assert_eq!(serde_json::to_string(&messages).unwrap(), before);
        }
    }

    #[test]
    fn summary_contains_source_boundary_and_question_answer_pairs() {
        let messages = groups(30);
        let mut state = WorkflowContextProjector::default();
        let result = state.project(&messages, 10_000, 0, 0);
        let summary = result
            .messages
            .iter()
            .find_map(first_text)
            .expect("projection should contain a summary");

        assert!(summary.contains("Source boundary version:"));
        assert!(summary.contains("Q: question"));
        assert!(summary.contains("A: answer"));
        assert!(!summary.contains("Earlier user topic:"));
    }

    #[test]
    fn changed_omitted_source_drops_stale_fact_from_summary_and_view() {
        let mut messages = groups(30);
        messages[1] = assistant("stale fact: the deployment is green");
        let mut state = WorkflowContextProjector::default();
        let old = state.project(&messages, 10_000, 0, 0);
        let old_summary = old
            .messages
            .iter()
            .find_map(first_text)
            .expect("initial projection should contain a summary");
        assert!(old_summary.contains("stale fact: the deployment is green"));
        let old_summary_source = old
            .summary_source_version
            .expect("initial projection should expose summary provenance");

        messages[1] = assistant("fresh fact: the deployment is red");
        let fresh = state.project(&messages, 10_000, 0, 0);

        assert_ne!(fresh.source_version, old.source_version);
        assert!(fresh.representation_changed);
        assert!(text_contains(
            &fresh.messages,
            "fresh fact: the deployment is red"
        ));
        assert!(!text_contains(
            &fresh.messages,
            "stale fact: the deployment is green"
        ));
        let fresh_summary_source = fresh
            .summary_source_version
            .expect("fresh projection should expose summary provenance");
        assert_ne!(fresh_summary_source, old_summary_source);
        let fresh_summary = fresh
            .messages
            .iter()
            .find_map(first_text)
            .expect("fresh projection should contain a summary");
        assert!(fresh_summary.contains(&format!("{:016x}", fresh_summary_source)));
        assert!(!fresh_summary.contains(&format!("{:016x}", old_summary_source)));
    }

    #[test]
    fn repeated_weather_history_keeps_current_question_and_compresses_old_turns() {
        let mut messages = Vec::new();
        for index in 0..8 {
            messages.push(user(&format!(
                "Какая сейчас погода? Предыдущий уточняющий шаг {index}."
            )));
            messages.push(assistant("Тепло."));
        }
        messages.push(user(
            "Какая сейчас погода? Какая погода в Амстердаме? Какая погода будет завтра?",
        ));
        messages.push(assistant("Тепло. В Амстердаме +24. Завтра будет +26."));
        let mut state = WorkflowContextProjector::default();
        let result = state.project(&messages, 1_000, 0, 0);

        assert_eq!(
            result.messages.last().and_then(first_text).as_deref(),
            Some("Тепло. В Амстердаме +24. Завтра будет +26.")
        );
        assert_eq!(result.before_turn_groups, 9);
        assert!(result.active);
        assert!(result.excluded_messages > 0);
        assert!(result.excluded_turn_groups > 0);
        assert_eq!(result.summary_messages, 1);
        assert!(result.messages.iter().any(|message| {
            first_text(message).is_some_and(|text| text.starts_with("[Automatic context summary."))
        }));
        assert!(!result.representation_changed);
    }

    #[test]
    fn workflow_context_keeps_only_latest_run_observations() {
        let mut messages = groups(4);
        messages[1] = assistant("old value: source:v1");
        messages.push(user("Check the latest source record."));
        messages.push(tool_call("weather-call"));
        messages.push(tool_result("weather-call"));
        messages.push(assistant("updated value: source:v2"));

        let mut state = WorkflowContextProjector::default();
        let result = state.project_workflow_context(&messages);

        assert_eq!(result.mode, "state_first");
        assert!(result.active);
        assert_eq!(result.after_turn_groups, 1);
        assert!(result.excluded_messages > 0);
        assert!(text_contains(
            &result.messages,
            "Check the latest source record."
        ));
        assert!(text_contains(&result.messages, "updated value: source:v2"));
        assert!(!text_contains(&result.messages, "old value: source:v1"));
        assert_eq!(first_gap(&result.messages), None);
    }

    #[test]
    fn workflow_retention_policy_controls_recent_tail() {
        let messages = groups(20);
        let cases = [
            (ContextRetention::High, 16),
            (ContextRetention::Mid, 8),
            (ContextRetention::Low, 1),
        ];

        for (retention, expected_groups) in cases {
            let mut state = WorkflowContextProjector::default();
            let result = state.project_workflow_context_with_retention(&messages, retention);

            assert_eq!(result.retention, retention);
            assert_eq!(result.mode, "state_first");
            assert_eq!(result.after_turn_groups, expected_groups);
            assert_eq!(result.excluded_turn_groups, 20 - expected_groups);
        }
    }

    #[test]
    fn disabled_retention_preserves_the_full_provider_view() {
        let messages = groups(20);
        let mut state = WorkflowContextProjector::default();
        let result =
            state.project_workflow_context_with_retention(&messages, ContextRetention::Disabled);

        assert_eq!(result.retention, ContextRetention::Disabled);
        assert_eq!(result.mode, "disabled");
        assert!(!result.active);
        assert_eq!(
            serde_json::to_string(&result.messages).unwrap(),
            serde_json::to_string(&messages).unwrap()
        );
        assert_eq!(result.excluded_messages, 0);
        assert_eq!(result.excluded_turn_groups, 0);
    }

    #[test]
    fn disabled_retention_bypasses_transcript_projection() {
        let messages = groups(50);
        let mut state = WorkflowContextProjector::default();
        let result =
            state.project_with_retention(&messages, 1_000, 0, 0, ContextRetention::Disabled);

        assert_eq!(
            serde_json::to_string(&result.messages).unwrap(),
            serde_json::to_string(&messages).unwrap()
        );
        assert_eq!(result.mode, "disabled");
        assert_eq!(result.reason, "retention_disabled");
    }

    #[test]
    fn automatic_projection_never_leaves_a_tool_result_without_its_call() {
        let mut messages = Vec::new();
        for index in 0..30 {
            messages.push(user(&format!("question {index} {}", "x".repeat(80))));
            messages.push(tool_call(&format!("call-{index}")));
            messages.push(tool_result(&format!("call-{index}")));
            messages.push(assistant("done"));
        }
        let mut state = WorkflowContextProjector::default();
        let result = state.project(&messages, 10_000, 0, 0);

        assert_eq!(first_gap(&result.messages), None);
    }

    #[test]
    fn stable_projection_accepts_append_only_growth_without_representation_reset() {
        let mut messages = groups(30);
        let mut state = WorkflowContextProjector::default();
        let first = state.project(&messages, 10_000, 0, 0);
        assert!(first.active);

        messages.push(user("new question"));
        messages.push(assistant("new answer"));
        let second = state.project(&messages, 10_000, 0, 0);

        assert!(!second.representation_changed);
        assert_eq!(second.summary_source_version, first.summary_source_version);
        assert!(second.reason.starts_with("stable_tail_append"));
    }
}
