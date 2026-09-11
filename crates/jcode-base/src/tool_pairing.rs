//! Согласованность пар «вызов инструмента — ответ инструмента» при обрезке.
//!
//! План управления контекстом требует после каждого изменения проверять пары
//! `tool call` и `tool result`. Любая структурная обрезка двигает границу
//! транскрипта, и на новой границе пара может разорваться:
//!
//! - ответ остался, а вызов ушёл — провайдер отвергает такой запрос;
//! - вызов остался, а ответ ушёл — провайдер достраивает синтетический ответ.
//!
//! Этот модуль считает границы, на которых пары целиком лежат по одну сторону,
//! и находит первое нарушение в готовом транскрипте.

use crate::message::ContentBlock;
use crate::session::StoredMessage;
use std::collections::HashSet;

/// Нарушение пар в транскрипте.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingGap {
    /// Вызов инструмента без своего ответа.
    UnansweredCall(String),
    /// Ответ инструмента без своего вызова.
    UnmatchedResult(String),
}

impl PairingGap {
    /// Id вызова или ответа, на котором нашлось нарушение.
    pub fn id(&self) -> &str {
        match self {
            Self::UnansweredCall(id) | Self::UnmatchedResult(id) => id,
        }
    }
}

impl std::fmt::Display for PairingGap {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnansweredCall(id) => write!(formatter, "tool call {id} has no result"),
            Self::UnmatchedResult(id) => write!(formatter, "tool result {id} has no call"),
        }
    }
}

/// Id вызовов без ответов и ответов без вызовов внутри диапазона.
///
/// Порядок появления в транскрипте сохраняется, чтобы ответ был предсказуем.
fn scan(messages: &[StoredMessage]) -> (Vec<String>, Vec<String>) {
    let mut seen_calls: HashSet<String> = HashSet::new();
    let mut calls: Vec<String> = Vec::new();
    let mut seen_results: HashSet<String> = HashSet::new();
    let mut results: Vec<String> = Vec::new();
    for message in messages {
        for block in &message.content {
            match block {
                ContentBlock::ToolUse { id, .. } => {
                    if seen_calls.insert(id.clone()) {
                        calls.push(id.clone());
                    }
                }
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    if seen_results.insert(tool_use_id.clone()) {
                        results.push(tool_use_id.clone());
                    }
                }
                _ => {}
            }
        }
    }
    let answered: HashSet<&String> = results.iter().collect();
    let called: HashSet<&String> = calls.iter().collect();
    (
        calls
            .iter()
            .filter(|id| !answered.contains(id))
            .cloned()
            .collect(),
        results
            .iter()
            .filter(|id| !called.contains(id))
            .cloned()
            .collect(),
    )
}

/// Первое нарушение пар в транскрипте, если оно есть.
pub fn first_gap(messages: &[StoredMessage]) -> Option<PairingGap> {
    let (unanswered, unmatched) = scan(messages);
    if let Some(id) = unanswered.first() {
        return Some(PairingGap::UnansweredCall(id.clone()));
    }
    unmatched
        .first()
        .map(|id| PairingGap::UnmatchedResult(id.clone()))
}

/// Для каждого сообщения: самодостаточен ли префикс, который им заканчивается.
///
/// Префикс самодостаточен, когда каждый вызов внутри него уже получил ответ
/// внутри него. Ответы без вызовов в префикс не попадают: вызов всегда идёт
/// раньше своего ответа.
pub fn balanced_prefix_ends(messages: &[StoredMessage]) -> Vec<bool> {
    let mut open_calls: HashSet<&str> = HashSet::new();
    let mut balanced = Vec::with_capacity(messages.len());
    for message in messages {
        for block in &message.content {
            match block {
                ContentBlock::ToolUse { id, .. } => {
                    open_calls.insert(id.as_str());
                }
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    open_calls.remove(tool_use_id.as_str());
                }
                _ => {}
            }
        }
        balanced.push(open_calls.is_empty());
    }
    balanced
}

/// Начало суффикса, в котором все пары целиком.
///
/// Граница сдвигается назад, пока суффикс начинается с ответа без вызова.
/// Позиция `0` означает, что обрезать префикс без разрыва пары нельзя.
pub fn balanced_suffix_start(messages: &[StoredMessage], start: usize) -> usize {
    let mut start = start.min(messages.len());
    loop {
        let (_, unmatched) = scan(&messages[start..]);
        if unmatched.is_empty() || start == 0 {
            return start;
        }
        start -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Role;

    fn call(id: &str) -> StoredMessage {
        StoredMessage {
            id: format!("call-message-{id}"),
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: "read".to_string(),
                input: serde_json::json!({}),
                thought_signature: None,
            }],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        }
    }

    fn result(id: &str) -> StoredMessage {
        StoredMessage {
            id: format!("result-message-{id}"),
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: "payload".to_string(),
                is_error: None,
            }],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        }
    }

    fn text(id: &str) -> StoredMessage {
        StoredMessage {
            id: id.to_string(),
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: id.to_string(),
                cache_control: None,
            }],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        }
    }

    #[test]
    fn prefix_ends_marks_only_the_end_of_a_complete_pair() {
        let messages = vec![text("turn"), call("a"), result("a"), text("next")];

        assert_eq!(
            balanced_prefix_ends(&messages),
            vec![true, false, true, true]
        );
    }

    #[test]
    fn suffix_start_moves_back_to_keep_the_call_with_its_result() {
        // Границы: turn | call a | result a | next
        let messages = vec![text("turn"), call("a"), result("a"), text("next")];

        assert_eq!(
            balanced_suffix_start(&messages, 2),
            1,
            "суффикс с ответа без вызова сдвигается назад к вызову"
        );
        assert_eq!(balanced_suffix_start(&messages, 3), 3);
        assert_eq!(
            balanced_suffix_start(&messages, 0),
            0,
            "дальше начала транскрипта сдвигаться некуда"
        );
    }

    #[test]
    fn first_gap_names_the_broken_pair() {
        assert_eq!(first_gap(&[text("turn"), call("a"), result("a")]), None);
        assert_eq!(
            first_gap(&[call("a")]),
            Some(PairingGap::UnansweredCall("a".to_string()))
        );
        assert_eq!(
            first_gap(&[result("a")]),
            Some(PairingGap::UnmatchedResult("a".to_string()))
        );
        assert_eq!(
            first_gap(&[call("a"), result("a"), result("a")]).is_none(),
            true,
            "повтор ответа парой не считается: дубликаты чинит провайдер"
        );
    }
}
