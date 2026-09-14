use crate::tui::backend::RemoteConnection;
use anyhow::Result;

/// Разобранная пользовательская команда обрезки контекста.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::tui::app) struct ContextPruneCommand {
    pub kind: String,
    pub keep_recent: Option<usize>,
    pub after_message_id: Option<String>,
}

/// `/context prune <kind> [keep_recent=N|--after <message-id>]`.
///
/// Опасные виды модель запросить не может: их ставит пользователь этой
/// командой, а сервер применяет заявку на границе turn-а.
pub(in crate::tui::app) fn parse_context_prune_command(
    trimmed: &str,
) -> Option<ContextPruneCommand> {
    let arguments = trimmed.strip_prefix("/context prune")?.trim();
    let mut parts = arguments.split_whitespace();
    let kind = parts.next()?.to_ascii_lowercase();
    if !matches!(kind.as_str(), "turns" | "tail" | "undo") {
        return None;
    }

    let mut keep_recent = None;
    let mut after_message_id = None;
    while let Some(part) = parts.next() {
        if let Some(value) = part.strip_prefix("keep_recent=") {
            if keep_recent.is_some() {
                return None;
            }
            let Ok(parsed) = value.parse::<usize>() else {
                return None;
            };
            keep_recent = Some(parsed);
        } else if part == "--after" {
            let value = parts.next()?;
            if value.is_empty() || value.starts_with('-') || value.contains('=') {
                return None;
            }
            if after_message_id.is_some() {
                return None;
            }
            after_message_id = Some(value.to_string());
        } else {
            let value = part.strip_prefix("--after=")?;
            if value.is_empty() || value.starts_with('-') || value.contains('=') {
                return None;
            }
            if after_message_id.is_some() {
                return None;
            }
            after_message_id = Some(value.to_string());
        }
    }

    match kind.as_str() {
        "turns" if after_message_id.is_none() => {}
        "tail" if keep_recent.is_none() && after_message_id.is_some() => {}
        "undo" if keep_recent.is_none() && after_message_id.is_none() => {}
        _ => return None,
    }

    Some(ContextPruneCommand {
        kind,
        keep_recent,
        after_message_id,
    })
}

pub(in crate::tui::app) async fn submit_prune(
    remote: &mut RemoteConnection,
    command: &ContextPruneCommand,
) -> Result<u64> {
    remote
        .context_prune_with_after(
            &command.kind,
            command.keep_recent,
            command.after_message_id.as_deref(),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_user_prune_command() {
        assert_eq!(
            parse_context_prune_command("/context prune turns keep_recent=4"),
            Some(ContextPruneCommand {
                kind: "turns".to_string(),
                keep_recent: Some(4),
                after_message_id: None,
            })
        );
        assert_eq!(
            parse_context_prune_command("/context prune undo"),
            Some(ContextPruneCommand {
                kind: "undo".to_string(),
                keep_recent: None,
                after_message_id: None,
            })
        );
        assert_eq!(parse_context_prune_command("/context prune"), None);
        assert_eq!(parse_context_prune_command("/context prune tail"), None);
        assert_eq!(
            parse_context_prune_command("/context prune tail --after message-2"),
            Some(ContextPruneCommand {
                kind: "tail".to_string(),
                keep_recent: None,
                after_message_id: Some("message-2".to_string()),
            })
        );
        assert_eq!(
            parse_context_prune_command("/context prune tail --after=message-2"),
            Some(ContextPruneCommand {
                kind: "tail".to_string(),
                keep_recent: None,
                after_message_id: Some("message-2".to_string()),
            })
        );
        assert_eq!(
            parse_context_prune_command("/context prune turns keep_recent=x"),
            None
        );
        assert_eq!(
            parse_context_prune_command("/context prune undo keep_recent=1"),
            None
        );
        assert_eq!(
            parse_context_prune_command("/context prune turns --after message-2"),
            None
        );
        assert_eq!(
            parse_context_prune_command("/context prune tail keep_recent=1 --after message-2"),
            None
        );
        assert_eq!(
            parse_context_prune_command("/context prune undo --after message-2"),
            None
        );
    }
}
