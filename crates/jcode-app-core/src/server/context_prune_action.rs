//! Пользовательская обрезка контекста.
//!
//! Опасные для контекста виды (`turns`) модель запросить не может: инструмент
//! `context_control` их отклоняет, поэтому единственный путь это явная команда
//! клиента (`/context prune turns [keep_recent=N]`). Заявка, как и заявка
//! модели, применяется на границе turn-а, поэтому транскрипт не меняется
//! посреди обработки запроса.

use crate::agent::Agent;
use crate::context_controller::{ContextPruneKind, ContextPruneSpec};
use crate::protocol::ServerEvent;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// Виды обрезки, которые может запросить только пользователь.
fn user_prune_kind(kind: &str) -> Option<ContextPruneKind> {
    match kind {
        "turns" => Some(ContextPruneKind::Turns),
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
    agent: &Arc<Mutex<Agent>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let requested = kind.trim().to_ascii_lowercase();
    let undo = is_user_prune_undo(&requested);
    let prune_kind = user_prune_kind(&requested);
    if !undo && prune_kind.is_none() {
        send_result(
            client_event_tx,
            id,
            format!(
                "Обрезка `{requested}` недоступна пользовательской команде; доступно: turns, undo"
            ),
            false,
        );
        return;
    }

    let agent = Arc::clone(agent);
    let tx = client_event_tx.clone();
    tokio::spawn(async move {
        let result = {
            let mut agent_guard = agent.lock().await;
            if undo {
                agent_guard.queue_user_prune_undo()
            } else {
                let mut spec = ContextPruneSpec::new(prune_kind.unwrap_or(ContextPruneKind::Turns));
                if let Some(keep_recent) = keep_recent {
                    spec = spec.keep_recent(keep_recent);
                }
                agent_guard.queue_user_prune(spec)
            }
        };

        match result {
            Ok(request) => {
                let detail = match request.prune.as_ref() {
                    Some(spec) => format!(
                        "Обрезка turn-групп поставлена в очередь (keep_recent={}, revision {})",
                        spec.keep_recent
                            .unwrap_or_else(|| ContextPruneKind::Turns.default_keep_recent()),
                        request.base_revision.0
                    ),
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
