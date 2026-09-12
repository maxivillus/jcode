//! Регрессия: `/rewind` не оставляет вызов инструмента без ответа.
//!
//! Срез по видимому сообщению может попасть между вызовом инструмента и его
//! результатом. Провайдеры такой транскрипт либо отвергают (Anthropic), либо
//! молча теряют вывод инструмента (OpenAI-совместимый путь), поэтому rewind
//! обязан отказывать до изменения истории.

use super::context_action_tests::test_agent;
use crate::message::{ContentBlock, Role};

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
}
