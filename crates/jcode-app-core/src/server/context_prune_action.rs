//! Пользовательская обрезка контекста.
//!
//! Опасные для контекста виды (`turns`) модель запросить не может: инструмент
//! `context_control` их отклоняет, поэтому единственный путь это явная команда
//! клиента (`/context prune turns [keep_recent=N]` или
//! `/context prune tail --after <message-id>`). Заявка, как и заявка
//! модели, применяется на границе turn-а, поэтому транскрипт не меняется
//! посреди обработки запроса.

use crate::agent::Agent;
use crate::context_controller::{ContextPruneKind, ContextPruneSpec};
use crate::protocol::{Request, ServerEvent};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// Виды обрезки, которые может запросить только пользователь.
fn user_prune_kind(kind: &str) -> Option<ContextPruneKind> {
    match kind {
        "turns" => Some(ContextPruneKind::Turns),
        "tail" => Some(ContextPruneKind::Tail),
        _ => None,
    }
}

/// Отмена последней обрезки доступна пользователю как `undo`.
fn is_user_prune_undo(kind: &str) -> bool {
    kind == "undo"
}

/// Отправляет результат заявки клиенту и сообщает в лог, если клиент ушёл.
fn send_result(tx: &mpsc::UnboundedSender<ServerEvent>, id: u64, message: String, success: bool) {
    let event = ServerEvent::ContextPruneResult {
        id,
        message,
        success,
    };
    if let Err(error) = tx.send(event) {
        crate::logging::warn(&format!(
            "Context prune result for request {id} could not be delivered: {error}"
        ));
    }
}

/// Ставит пользовательскую обрезку в очередь и отвечает клиенту.
pub(super) fn handle_context_prune(
    id: u64,
    kind: &str,
    keep_recent: Option<usize>,
    after_message_id: Option<String>,
    agent: &Arc<Mutex<Agent>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let requested = kind.trim().to_ascii_lowercase();
    let undo = is_user_prune_undo(&requested);
    let prune_kind = user_prune_kind(&requested);
    let after_message_id = after_message_id.map(|value| value.trim().to_string());
    if !undo && prune_kind.is_none() {
        send_result(
            client_event_tx,
            id,
            format!(
                "Обрезка `{requested}` недоступна пользовательской команде; доступно: turns, tail, undo"
            ),
            false,
        );
        return;
    }

    if after_message_id
        .as_ref()
        .is_some_and(|value| value.is_empty())
    {
        send_result(
            client_event_tx,
            id,
            "Для tail нужно указать непустой message-id".to_string(),
            false,
        );
        return;
    }

    if undo {
        if keep_recent.is_some() || after_message_id.is_some() {
            send_result(
                client_event_tx,
                id,
                "Обрезка undo не принимает keep_recent или --after".to_string(),
                false,
            );
            return;
        }
    } else {
        match prune_kind.expect("non-undo request must have a prune kind") {
            ContextPruneKind::Turns if after_message_id.is_some() => {
                send_result(
                    client_event_tx,
                    id,
                    "Обрезка turns не принимает --after".to_string(),
                    false,
                );
                return;
            }
            ContextPruneKind::Tail if keep_recent.is_some() => {
                send_result(
                    client_event_tx,
                    id,
                    "Обрезка tail принимает только --after <message-id>".to_string(),
                    false,
                );
                return;
            }
            ContextPruneKind::Tail if after_message_id.is_none() => {
                send_result(
                    client_event_tx,
                    id,
                    "Обрезка tail требует --after <message-id>".to_string(),
                    false,
                );
                return;
            }
            _ => {}
        }
    }

    let agent = Arc::clone(agent);
    let tx = client_event_tx.clone();
    tokio::spawn(async move {
        let result = {
            let mut agent_guard = agent.lock().await;
            if undo {
                agent_guard.queue_user_prune_undo()
            } else {
                let kind = prune_kind.expect("validated prune kind");
                let mut spec = ContextPruneSpec::new(kind);
                match kind {
                    ContextPruneKind::Tail => {
                        if let Some(after_message_id) = after_message_id {
                            spec = spec.after(after_message_id);
                        }
                    }
                    ContextPruneKind::Turns => {
                        if let Some(keep_recent) = keep_recent {
                            spec = spec.keep_recent(keep_recent);
                        }
                    }
                    _ => unreachable!("user_prune_kind only returns user-only kinds"),
                }
                agent_guard.queue_user_prune(spec)
            }
        };

        match result {
            Ok(request) => {
                let detail = match request.prune.as_ref() {
                    Some(spec) => match spec.kind {
                        ContextPruneKind::Tail => format!(
                            "Обрезка хвоста поставлена в очередь (after={}, revision {})",
                            spec.after_message_id.as_deref().unwrap_or("unknown"),
                            request.base_revision.0
                        ),
                        _ => format!(
                            "Обрезка turn-групп поставлена в очередь (keep_recent={}, revision {})",
                            spec.keep_recent.unwrap_or_else(|| {
                                ContextPruneKind::Turns.default_keep_recent()
                            }),
                            request.base_revision.0
                        ),
                    },
                    None => format!(
                        "Отмена последней обрезки поставлена в очередь (revision {})",
                        request.base_revision.0
                    ),
                };
                send_result(
                    &tx,
                    id,
                    format!("{detail}. Заявка применится на следующей границе turn-а."),
                    true,
                );
            }
            Err(error) => send_result(
                &tx,
                id,
                format!("Обрезка не поставлена в очередь: {error}"),
                false,
            ),
        }
    });
}

pub(super) fn handle_context_prune_request(
    request: Request,
    agent: &Arc<Mutex<Agent>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let Request::ContextPrune {
        id,
        kind,
        keep_recent,
        after_message_id,
    } = request
    else {
        unreachable!("context prune handler received a different request");
    };
    handle_context_prune(
        id,
        &kind,
        keep_recent,
        after_message_id,
        agent,
        client_event_tx,
    );
}
