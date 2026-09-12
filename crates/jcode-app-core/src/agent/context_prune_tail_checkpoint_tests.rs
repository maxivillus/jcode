//! Тесты подписанных точек среза хвоста.
//!
//! Подписанные точки (`compaction-boundary` и `session-start`) не зависят от
//! выбора: их предлагает `preview`, и по ним можно резать хвост. Модуль вынесен
//! отдельно, чтобы файлы тестов оставались в бюджете размера; хелперы берутся
//! из `context_action_tests`, чтобы не дублировать настройку тестового агента.

use super::Agent;
use super::context_action_tests::{
    last_outcome, record_preview_snapshot, request_action, request_tail_prune, test_agent,
};
use crate::context_controller::{
    ContextActionKind, ContextActionOutcome, ContextPruneKind, ContextPruneProjection,
    MAX_PROJECTION_LEVELS,
};
use crate::message::{ContentBlock, Role};

/// Текстовый блок для сообщений теста.
fn text_blocks(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text {
        text: text.to_string(),
        cache_control: None,
    }]
}

/// Проекция хвоста из снимка, который снимает preflight на границе turn-а.
fn tail_projection(agent: &Agent) -> ContextPruneProjection {
    agent
        .context_controller
        .lock()
        .expect("controller lock")
        .prune_projections()
        .iter()
        .find(|projection| projection.kind == ContextPruneKind::Tail)
        .cloned()
        .expect("the snapshot must cover the tail kind")
}

#[tokio::test]
async fn tail_preview_names_compaction_boundary_and_session_start() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = test_agent().await;
    let mut ids = Vec::new();
    for index in 0..4 {
        ids.push(agent.add_message(Role::User, text_blocks(&format!("turn {index}"))));
    }
    agent.session.compaction = Some(crate::session::StoredCompactionState {
        summary_text: "summary".to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: 2,
        original_turn_count: 2,
        compacted_count: 2,
    });
    record_preview_snapshot(&mut agent);

    let projection = tail_projection(&agent);
    assert!(
        agent.session.messages[0]
            .content
            .iter()
            .any(|block| matches!(
                block,
                ContentBlock::Text { text, .. } if text.contains("# Session Context")
            )),
        "the oldest message is the seeded session context reminder"
    );
    assert_eq!(
        ids[0], agent.session.messages[1].id,
        "Agent::new seeds one session-context message before the test turns"
    );
    let labels: Vec<&str> = projection
        .tail_checkpoint_samples(4)
        .into_iter()
        .map(|(label, _)| label)
        .collect();
    assert_eq!(
        labels,
        vec!["compaction-boundary", "session-start"],
        "both natural cuts must be named"
    );
    assert_eq!(
        projection.tail_checkpoint_samples(4)[0].1,
        ids[0],
        "the compaction boundary is the last message covered by the summary (transcript index 1)"
    );
    assert_eq!(
        projection.tail_checkpoint_samples(4)[1].1,
        agent.session.messages[0].id,
        "session-start names the oldest message of the session"
    );

    // Срез по границе сжатия возвращает транскрипт к состоянию на момент сжатия.
    let total = agent.session.messages.len();
    request_tail_prune(&agent, &ids[0]);
    agent.apply_pending_context_actions();

    assert!(matches!(
        last_outcome(&agent),
        ContextActionOutcome::Completed { .. }
    ));
    assert_eq!(
        agent.session.messages.len(),
        2,
        "only the messages covered by the last compaction stay"
    );

    request_action(&agent, ContextActionKind::UndoPrune);
    agent.apply_pending_context_actions();

    assert_eq!(
        agent.session.messages.len(),
        total,
        "undo must restore the whole transcript"
    );
}

#[tokio::test]
async fn tail_projection_keeps_named_checkpoints_when_the_window_overflows() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = test_agent().await;
    for index in 0..MAX_PROJECTION_LEVELS + 40 {
        agent.add_message(Role::User, text_blocks(&format!("turn {index}")));
    }
    record_preview_snapshot(&mut agent);

    let projection = tail_projection(&agent);
    assert!(
        projection.levels.len() <= MAX_PROJECTION_LEVELS,
        "the window stays bounded"
    );
    assert!(
        !projection.levels.iter().any(|level| level.index == 1),
        "ordinary cuts that deep stay outside the window"
    );
    assert_eq!(
        projection.tail_checkpoint_samples(4),
        vec![("session-start", agent.session.messages[0].id.as_str())],
        "a named cut must be offered even when its depth is outside the window"
    );
}
