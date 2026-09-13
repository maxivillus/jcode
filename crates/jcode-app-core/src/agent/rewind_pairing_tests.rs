//! Регрессия: `/rewind` не оставляет вызов инструмента без ответа.
//!
//! Срез по видимому сообщению может попасть между вызовом инструмента и его
//! результатом. Провайдеры такой транскрипт либо отвергают (Anthropic), либо
//! молча теряют вывод инструмента (OpenAI-совместимый путь), поэтому rewind
//! обязан отказывать до изменения истории.

use super::Agent;
use super::context_action_tests::test_agent;
use crate::message::{ContentBlock, Role};

fn provider_transcript_hash(agent: &Agent) -> String {
    let messages = agent.session.messages_for_provider_uncached();
    let projection = crate::message::cache_relevant_messages(&messages);
    crate::context::sha256_hex(serde_json::to_vec(&projection).expect("serialize transcript"))
}

fn text_blocks(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text {
        text: text.to_string(),
        cache_control: None,
    }]
}

#[tokio::test]
async fn rewind_refuses_to_cut_between_a_tool_call_and_its_result() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = test_agent().await;
    agent.add_message(Role::User, text_blocks("one"));
    agent.add_message(
        Role::Assistant,
        vec![
            ContentBlock::Text {
                text: "checking".to_string(),
                cache_control: None,
            },
            ContentBlock::ToolUse {
                id: "call-1".to_string(),
                name: "read".to_string(),
                input: serde_json::json!({"file_path": "Cargo.toml"}),
                thought_signature: None,
            },
        ],
    );
    agent.add_message(
        Role::User,
        vec![ContentBlock::ToolResult {
            tool_use_id: "call-1".to_string(),
            content: "payload".to_string(),
            is_error: None,
        }],
    );
    agent.add_message(Role::User, text_blocks("two"));
    let before = agent.session.messages.len();
    let before_hash = provider_transcript_hash(&agent);

    let error = agent
        .rewind_to_message(2)
        .expect_err("a cut that keeps the tool call without its result must be refused");
    assert!(
        error.contains("call-1") && error.contains("no result"),
        "the refusal must name the gap: {error}"
    );
    assert_eq!(
        agent.session.messages.len(),
        before,
        "a refused rewind must leave the transcript intact"
    );
    assert_eq!(
        provider_transcript_hash(&agent),
        before_hash,
        "a refused rewind must not change the provider transcript hash"
    );

    let removed = agent
        .rewind_to_message(1)
        .expect("a cut that keeps the pair whole must still work");
    assert_eq!(
        removed, 2,
        "rewinding to the first visible message removes the rest"
    );
    assert_eq!(
        agent.session.messages.len(),
        2,
        "the seeded session-context message and the first visible message stay"
    );
    assert_ne!(
        provider_transcript_hash(&agent),
        before_hash,
        "a successful rewind must change the provider transcript hash"
    );

    let restored = agent.undo_rewind().expect("the rewind must be undoable");
    assert_eq!(restored, 2, "undo must restore the two removed messages");
    assert_eq!(
        provider_transcript_hash(&agent),
        before_hash,
        "rewind undo must restore the exact provider transcript hash"
    );
}

#[tokio::test]
async fn rewind_undo_survives_session_reload() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = test_agent().await;
    for text in ["one", "two", "three"] {
        agent.add_message(Role::User, text_blocks(text));
    }
    let before_hash = provider_transcript_hash(&agent);

    let removed = agent
        .rewind_to_message(1)
        .expect("rewind should persist an undo snapshot");
    assert_eq!(removed, 2);

    let persisted = crate::session::Session::load(agent.session_id())
        .expect("rewind snapshot should be persisted");
    assert!(persisted.rewind_undo_snapshot.is_some());

    let provider = agent.provider.fork();
    let registry = crate::tool::Registry::new(provider.clone()).await;
    let mut restored = Agent::new_with_session(provider, registry, persisted, None);
    let restored_count = restored
        .undo_rewind()
        .expect("reloaded rewind should remain undoable");

    assert_eq!(restored_count, 2);
    assert_eq!(provider_transcript_hash(&restored), before_hash);
    let after_undo = crate::session::Session::load(restored.session_id())
        .expect("restored session should remain loadable");
    assert!(after_undo.rewind_undo_snapshot.is_none());
}
