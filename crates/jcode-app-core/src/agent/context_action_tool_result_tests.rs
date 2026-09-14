use super::*;

#[tokio::test]
async fn prune_tool_results_keeps_tool_pairing() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = test_agent().await;
    for index in 1..=2 {
        agent.add_message(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: format!("call-{index}"),
                name: "read".to_string(),
                input: serde_json::json!({"file_path": "Cargo.toml"}),
                thought_signature: None,
            }],
        );
        agent.add_message(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_use_id: format!("call-{index}"),
                content: format!("payload {index}"),
                is_error: None,
            }],
        );
    }

    request_prune(&agent, ContextPruneKind::ToolResults, Some(1));
    agent.apply_pending_context_actions();

    assert!(matches!(
        last_outcome(&agent),
        ContextActionOutcome::Completed { .. }
    ));
    let results: Vec<(String, String)> = agent
        .session
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => Some((tool_use_id.clone(), content.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2, "tool results must not be dropped");
    assert_eq!(results[0].0, "call-1");
    assert!(
        results[0].1.starts_with("[tool result pruned"),
        "stale result must be elided, got: {}",
        results[0].1
    );
    assert_eq!(results[1], ("call-2".to_string(), "payload 2".to_string()));

    request_action(&agent, ContextActionKind::UndoPrune);
    agent.apply_pending_context_actions();

    let restored_results: Vec<(String, String)> = agent
        .session
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => Some((tool_use_id.clone(), content.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        restored_results,
        vec![
            ("call-1".to_string(), "payload 1".to_string()),
            ("call-2".to_string(), "payload 2".to_string()),
        ],
        "undo must restore pruned tool-result content"
    );
}
