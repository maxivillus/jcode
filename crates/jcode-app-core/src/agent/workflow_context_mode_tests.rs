use super::Agent;
use super::context_action_tests::test_agent;
use crate::message::{ContentBlock, Message, Role};

#[test]
fn active_workflow_defaults_to_state_first_without_environment_flags() {
    assert!(Agent::workflow_context_mode_requested_from_values(
        None, None
    ));
}

#[test]
fn explicit_transcript_mode_keeps_legacy_projection() {
    assert!(!Agent::workflow_context_mode_requested_from_values(
        Some("transcript"),
        None,
    ));
}

#[test]
fn legacy_state_first_false_flag_disables_the_default() {
    assert!(!Agent::workflow_context_mode_requested_from_values(
        None,
        Some("0")
    ));
    assert!(!Agent::workflow_context_mode_requested_from_values(
        None,
        Some("off")
    ));
}

#[test]
fn explicit_state_first_mode_enables_workflow_projection() {
    assert!(Agent::workflow_context_mode_requested_from_values(
        Some("state_first"),
        None,
    ));
    assert!(Agent::workflow_context_mode_requested_from_values(
        None,
        Some("true")
    ));
}

#[tokio::test]
async fn active_workflow_uses_state_first_projection_by_default() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = test_agent().await;
    agent.active_skill = Some("synthetic-workflow".to_string());

    assert!(agent.should_use_workflow_context_view());

    let messages = vec![
        Message::user("historical request one"),
        Message::assistant_text("historical answer one"),
        Message::user("historical request two"),
        Message::assistant_text("historical answer two"),
        Message::user("current request"),
    ];
    let split_prompt = agent.build_system_prompt_split(None);
    let projected = agent.project_context(&messages, &split_prompt, &[]);

    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].role, Role::User);
    assert!(matches!(
        &projected[0].content[0],
        ContentBlock::Text { text, .. } if text == "current request"
    ));
}

#[tokio::test]
async fn fifty_turn_active_workflow_stays_bounded_without_context_control_calls() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = test_agent().await;
    agent.active_skill = Some("synthetic-workflow".to_string());
    let split_prompt = agent.build_system_prompt_split(None);
    let mut messages = Vec::new();

    for turn in 0..50 {
        let user_text = format!("turn-{turn}: current request with authoritative value");
        let assistant_text = format!("turn-{turn}: response");
        messages.push(Message::user(&user_text));
        messages.push(Message::assistant_text(&assistant_text));

        let projected = agent.project_context(&messages, &split_prompt, &[]);
        assert!(
            projected.len() <= 2,
            "turn {turn} projected too much history"
        );

        if turn == 49 {
            let encoded = serde_json::to_string(&projected).expect("projected messages serialize");
            assert!(!encoded.contains("turn-0:"));
            assert!(encoded.contains("turn-49:"));
        }
    }
}
