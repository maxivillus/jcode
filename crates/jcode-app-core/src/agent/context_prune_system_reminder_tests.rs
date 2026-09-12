//! Тесты обрезки системных напоминаний харнесса.
//!
//! Вынесены из `context_action_tests`, чтобы файл укладывался в бюджет размера
//! тестов (`scripts/check_test_size_budget.py`). Общие хелперы берутся из
//! `context_action_tests`, чтобы не дублировать настройку тестового агента.

use super::Agent;
use super::context_action_tests::{
    completed_detail, estimated_tokens, last_outcome, prune_forecast, record_preview_snapshot,
    request_action, request_prune, test_agent,
};
use crate::context_controller::{ContextActionKind, ContextActionOutcome, ContextPruneKind};
use crate::message::{ContentBlock, Role};

/// Текстовое сообщение для тестов селектора напоминаний.
fn reminder_blocks(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text {
        text: text.to_string(),
        cache_control: None,
    }]
}

/// Есть ли в транскрипте сообщение с такой подстрокой текста.
fn transcript_contains(agent: &Agent, needle: &str) -> bool {
    agent.session.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text, .. } if text.contains(needle)))
    })
}

/// Сколько напоминаний обрезка вида `system-reminders` считает целями.
///
/// `Agent::new` добавляет стартовое напоминание `# Session Context`
/// (`ensure_initial_session_context_message`), поэтому целей всегда на одну
/// больше, чем сообщений, которые добавил тест.
fn prunable_reminders(agent: &Agent) -> usize {
    agent
        .session
        .messages
        .iter()
        .filter(|message| super::context_prune::is_prunable_system_reminder(message))
        .count()
}

const SESSION_CONTEXT_REMINDER: &str =
    "<system-reminder>\n# Session Context\nstart of session\n</system-reminder>";
const ENVIRONMENT_REMINDER: &str =
    "<system-reminder>\n# Environment\nold environment\n</system-reminder>";
const MEMORY_REMINDER: &str = "<system-reminder>\n# Memory\nremembered fact\n</system-reminder>";

#[tokio::test]
async fn prune_system_reminders_removes_stale_reminders_and_keeps_memory_injections() {
    let mut agent = test_agent().await;
    agent.add_message(Role::User, reminder_blocks(SESSION_CONTEXT_REMINDER));
    agent.add_message(Role::User, reminder_blocks(ENVIRONMENT_REMINDER));
    agent.add_message(Role::User, reminder_blocks("real turn"));
    agent.add_message(Role::User, reminder_blocks(MEMORY_REMINDER));
    let before = agent.session.messages.len();
    let prunable_before = prunable_reminders(&agent);

    request_prune(&agent, ContextPruneKind::SystemReminders, Some(1));
    agent.apply_pending_context_actions();

    assert!(matches!(
        last_outcome(&agent),
        ContextActionOutcome::Completed { .. }
    ));
    assert_eq!(
        agent.session.messages.len(),
        before - (prunable_before - 1),
        "keep_recent = 1 must protect exactly one reminder"
    );
    assert!(
        transcript_contains(&agent, "# Environment"),
        "keep_recent must protect the newest reminder"
    );
    assert!(
        !transcript_contains(&agent, "# Session Context"),
        "the stale reminder must be gone"
    );
    assert!(
        transcript_contains(&agent, "# Memory"),
        "memory injections belong to their own prune kind"
    );
    assert!(transcript_contains(&agent, "real turn"));

    request_action(&agent, ContextActionKind::UndoPrune);
    agent.apply_pending_context_actions();

    assert_eq!(agent.session.messages.len(), before);
    assert!(
        transcript_contains(&agent, "# Session Context"),
        "undo must restore the pruned reminder"
    );
}

#[tokio::test]
async fn preview_forecast_matches_applied_system_reminder_prune() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = test_agent().await;
    agent.add_message(Role::User, reminder_blocks("real turn"));
    for index in 0..3 {
        agent.add_message(
            Role::User,
            reminder_blocks(&format!(
                "<system-reminder>\n# Environment\nenvironment {index}\n</system-reminder>"
            )),
        );
    }
    record_preview_snapshot(&mut agent);

    let forecast = prune_forecast(&agent, ContextPruneKind::SystemReminders, Some(1));
    assert_eq!(forecast.removable_items, prunable_reminders(&agent) - 1);

    request_prune(&agent, ContextPruneKind::SystemReminders, Some(1));
    agent.apply_pending_context_actions();

    let detail = completed_detail(&agent);
    let (before, after) = estimated_tokens(&detail);
    assert!(
        detail.contains(&format!("pruned {} item(s)", forecast.removable_items)),
        "applied prune must match the forecast, got: {detail}"
    );
    assert_eq!(
        before - after,
        forecast.removable_tokens,
        "preview must predict the tokens the prune frees: {detail}"
    );
}

#[tokio::test]
async fn system_reminder_prune_never_drops_a_message_with_tool_blocks() {
    let mut agent = test_agent().await;
    agent.add_message(Role::User, reminder_blocks(SESSION_CONTEXT_REMINDER));
    agent.add_message(
        Role::Assistant,
        vec![ContentBlock::ToolUse {
            id: "call-1".to_string(),
            name: "read".to_string(),
            input: serde_json::json!({"file_path": "Cargo.toml"}),
            thought_signature: None,
        }],
    );
    agent.add_message(
        Role::User,
        vec![
            ContentBlock::Text {
                text: ENVIRONMENT_REMINDER.to_string(),
                cache_control: None,
            },
            ContentBlock::ToolResult {
                tool_use_id: "call-1".to_string(),
                content: "payload".to_string(),
                is_error: None,
            },
        ],
    );
    let before = agent.session.messages.len();
    let prunable_before = prunable_reminders(&agent);

    request_prune(&agent, ContextPruneKind::SystemReminders, Some(0));
    agent.apply_pending_context_actions();

    assert!(matches!(
        last_outcome(&agent),
        ContextActionOutcome::Completed { .. }
    ));
    assert_eq!(
        agent.session.messages.len(),
        before - prunable_before,
        "keep_recent = 0 must remove every prunable reminder"
    );
    assert_eq!(
        prunable_reminders(&agent),
        0,
        "nothing prunable may be left behind"
    );
    assert!(
        transcript_contains(&agent, "old environment"),
        "a reminder inside a tool-result message must stay"
    );
    assert!(
        !transcript_contains(&agent, "# Session Context"),
        "the pure reminder is still pruned"
    );
}
