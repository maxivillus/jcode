use super::{Session, StoredMessage, StoredReplayEventKind};
use crate::message::ContentBlock;
use serde::{Deserialize, Serialize};

/// Состояние transcript и provider до последней обратимой мутации.
///
/// Snapshot хранится в том же полном Session snapshot, что и transcript. Это
/// позволяет восстановить undo после перезапуска без отдельного sidecar-файла.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionUndoSnapshot {
    pub messages: Vec<StoredMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_provider_session_id: Option<String>,
    /// Число видимых сообщений до rewind. Для prune значение равно нулю.
    #[serde(default)]
    pub visible_message_count: usize,
}

impl Session {
    pub fn redacted_for_export(&self) -> Self {
        let mut redacted = self.clone();
        if let Some(title) = redacted.title.as_mut() {
            *title = crate::message::redact_secrets(title);
        }
        if let Some(title) = redacted.custom_title.as_mut() {
            *title = crate::message::redact_secrets(title);
        }
        if let Some(compaction) = redacted.compaction.as_mut() {
            compaction.summary_text = crate::message::redact_secrets(&compaction.summary_text);
        }
        // Undo snapshots are private recovery state, not part of an export.
        // Dropping them also prevents an export from carrying a second copy of
        // the untrimmed transcript.
        redacted.rewind_undo_snapshot = None;
        redacted.prune_undo_snapshot = None;
        for msg in &mut redacted.messages {
            for block in &mut msg.content {
                match block {
                    ContentBlock::Text { text, .. }
                    | ContentBlock::Reasoning { text }
                    | ContentBlock::ReasoningTrace { text } => {
                        *text = crate::message::redact_secrets(text);
                    }
                    ContentBlock::AnthropicThinking { thinking, .. } => {
                        *thinking = crate::message::redact_secrets(thinking);
                    }
                    ContentBlock::OpenAIReasoning { summary, .. } => {
                        for item in summary {
                            *item = crate::message::redact_secrets(item);
                        }
                    }
                    ContentBlock::ToolResult { content, .. } => {
                        *content = crate::message::redact_secrets(content);
                    }
                    ContentBlock::ToolUse { input, .. } => redact_json_value(input),
                    ContentBlock::Image { .. } => {}
                    ContentBlock::OpenAICompaction { .. } => {}
                }
            }
        }
        for event in &mut redacted.replay_events {
            match &mut event.kind {
                StoredReplayEventKind::DisplayMessage { title, content, .. } => {
                    if let Some(title) = title.as_mut() {
                        *title = crate::message::redact_secrets(title);
                    }
                    *content = crate::message::redact_secrets(content);
                }
                StoredReplayEventKind::SwarmStatus { members } => {
                    for member in members {
                        if let Some(detail) = member.detail.as_mut() {
                            *detail = crate::message::redact_secrets(detail);
                        }
                    }
                }
                StoredReplayEventKind::SwarmPlan { items, reason, .. } => {
                    if let Some(reason) = reason.as_mut() {
                        *reason = crate::message::redact_secrets(reason);
                    }
                    for item in items {
                        item.content = crate::message::redact_secrets(&item.content);
                    }
                }
            }
        }
        redacted
    }
}

fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            *s = crate::message::redact_secrets(s);
        }
        serde_json::Value::Array(values) => {
            for entry in values {
                redact_json_value(entry);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if is_sensitive_json_key(key) {
                    *entry = serde_json::Value::String("[REDACTED_SECRET]".to_string());
                } else {
                    redact_json_value(entry);
                }
            }
        }
        _ => {}
    }
}

fn is_sensitive_json_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    normalized.contains("apikey")
        || normalized.ends_with("token")
        || normalized.ends_with("secret")
        || normalized.contains("password")
        || matches!(
            normalized.as_str(),
            "authorization" | "cookie" | "setcookie" | "privatekey" | "clientsecret"
        )
}
