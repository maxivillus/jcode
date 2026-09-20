use super::{
    ContentBlock, ContextRetention, Message, Role, WorkflowContextProjector,
    provider_semantic_state::BOUNDED_SEMANTIC_STATE_MARKER,
};

impl WorkflowContextProjector {
    fn semantic_state_facts(&self) -> Option<Vec<String>> {
        self.semantic_state.facts_for_test()
    }
}

fn user(text: &str) -> Message {
    Message::user(text)
}

fn assistant(text: &str) -> Message {
    Message::assistant_text(text)
}

fn tool_call(id: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: "read".to_string(),
            input: serde_json::json!({"path":"file"}),
            thought_signature: None,
        }],
        timestamp: None,
        tool_duration_ms: None,
    }
}

fn tool_result(id: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: "tool result".to_string(),
            is_error: None,
        }],
        timestamp: None,
        tool_duration_ms: None,
    }
}

fn text_contains(messages: &[Message], needle: &str) -> bool {
    messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text, .. } if text.contains(needle)))
    })
}

fn weather_messages(step: usize) -> Vec<Message> {
    let mut messages = Vec::new();
    if step >= 1 {
        messages.push(user("Какая сейчас погода?"));
        messages.push(assistant("Тепло."));
    }
    if step >= 2 {
        messages.push(user("Какая сейчас погода? А в Амстердаме?"));
        messages.push(assistant("В Амстердаме тепло, +24."));
    }
    if step >= 3 {
        messages.push(user("Какая сейчас погода? А в Амстердаме? А завтра?"));
        messages.push(assistant("Тепло. В Амстердаме тепло, +24. Завтра +26."));
    }
    messages
}

fn weather_messages_through(step: usize) -> Vec<Message> {
    let mut messages = weather_messages(step.min(3));
    for current in 4..=step {
        messages.push(user(&format!("Какая погода? Уточнение {current}")));
        messages.push(assistant(&format!(
            "Тепло. fact:day{current}=+{}",
            20 + current
        )));
    }
    messages
}

#[test]
fn low_semantic_state_updates_each_step_and_compresses_at_n_three() {
    let mut projector = WorkflowContextProjector::default();

    let first = projector
        .project_workflow_context_with_retention(&weather_messages(1), ContextRetention::Low);
    assert_eq!(first.reason, "state_first_latest_turn");
    assert_eq!(
        projector.semantic_state_facts(),
        Some(vec!["current=warm".to_string()])
    );

    let second = projector
        .project_workflow_context_with_retention(&weather_messages(2), ContextRetention::Low);
    assert_eq!(second.reason, "state_first_latest_turn");
    assert_eq!(
        projector.semantic_state_facts(),
        Some(vec![
            "Amsterdam.today=+24".to_string(),
            "current=warm".to_string()
        ])
    );

    let third = projector
        .project_workflow_context_with_retention(&weather_messages(3), ContextRetention::Low);
    assert_eq!(third.reason, "state_first_semantic_low_rebuild");
    assert_eq!(third.after_turn_groups, 1);
    assert!(third.semantic_compressed);
    assert!(third.semantic_state_bytes < third.semantic_replaced_bytes);
    assert!(text_contains(&third.messages, "current=warm"));
    assert!(text_contains(&third.messages, "Amsterdam.today=+24"));
    assert!(text_contains(&third.messages, "Amsterdam.tomorrow=+26"));
    assert!(text_contains(
        &third.messages,
        BOUNDED_SEMANTIC_STATE_MARKER
    ));
}

#[test]
fn low_semantic_state_replaces_contradictory_weather_fact() {
    let mut projector = WorkflowContextProjector::default();
    let initial = weather_messages(3);
    let _ = projector.project_workflow_context_with_retention(&initial, ContextRetention::Low);

    let mut refreshed = initial;
    refreshed.push(user("Какая погода в Амстердаме сегодня?"));
    refreshed.push(assistant("В Амстердаме сегодня +18."));
    let result =
        projector.project_workflow_context_with_retention(&refreshed, ContextRetention::Low);

    assert!(result.reason.starts_with("state_first_semantic_low_"));
    assert!(text_contains(&result.messages, "Amsterdam.today=+18"));
    assert!(!text_contains(&result.messages, "Amsterdam.today=+24"));
}

#[test]
fn low_semantic_state_waits_for_n_three_before_projecting() {
    let messages = vec![user("q"), assistant("a"), user("q2"), assistant("a2")];
    let mut projector = WorkflowContextProjector::default();
    let result =
        projector.project_workflow_context_with_retention(&messages, ContextRetention::Low);

    assert_eq!(result.reason, "state_first_latest_turn");
    assert_eq!(result.after_turn_groups, 1);
    assert!(!text_contains(
        &result.messages,
        BOUNDED_SEMANTIC_STATE_MARKER
    ));
}

#[test]
fn retention_modes_keep_the_planned_rebuild_intervals() {
    assert_eq!(
        super::retention_settings(ContextRetention::Low).semantic_rebuild_interval,
        Some(3)
    );
    assert_eq!(
        super::retention_settings(ContextRetention::Low).semantic_tail_groups,
        1
    );
    assert_eq!(
        super::retention_settings(ContextRetention::Mid).semantic_rebuild_interval,
        Some(6)
    );
    assert_eq!(
        super::retention_settings(ContextRetention::Mid).semantic_tail_groups,
        2
    );
    assert_eq!(
        super::retention_settings(ContextRetention::High).semantic_rebuild_interval,
        Some(9)
    );
    assert_eq!(
        super::retention_settings(ContextRetention::High).semantic_tail_groups,
        4
    );
    assert_eq!(
        super::retention_settings(ContextRetention::Disabled).semantic_rebuild_interval,
        None
    );
}

#[test]
fn mid_semantic_state_rebuilds_at_n_six_and_keeps_two_recent_turns() {
    let mut projector = WorkflowContextProjector::default();
    let result = projector.project_workflow_context_with_retention(
        &weather_messages_through(6),
        ContextRetention::Mid,
    );

    assert_eq!(result.reason, "state_first_semantic_mid_rebuild");
    assert_eq!(result.after_turn_groups, 2);
    assert!(result.semantic_compressed);
    assert!(result.semantic_state_bytes < result.semantic_replaced_bytes);
    assert!(text_contains(&result.messages, "current=warm"));
    assert!(text_contains(&result.messages, "day6=+26"));
}

#[test]
fn mid_semantic_state_applies_delta_between_rebuilds() {
    let mut projector = WorkflowContextProjector::default();
    let _ = projector.project_workflow_context_with_retention(
        &weather_messages_through(6),
        ContextRetention::Mid,
    );
    let result = projector.project_workflow_context_with_retention(
        &weather_messages_through(7),
        ContextRetention::Mid,
    );

    assert_eq!(result.reason, "state_first_semantic_mid_delta");
    assert_eq!(result.after_turn_groups, 2);
    assert!(text_contains(&result.messages, "day7=+27"));
}

#[test]
fn high_semantic_state_rebuilds_at_n_nine_and_keeps_four_recent_turns() {
    let mut projector = WorkflowContextProjector::default();
    let result = projector.project_workflow_context_with_retention(
        &weather_messages_through(9),
        ContextRetention::High,
    );

    assert_eq!(result.reason, "state_first_semantic_high_rebuild");
    assert_eq!(result.after_turn_groups, 4);
    assert!(result.semantic_compressed);
    assert!(result.semantic_state_bytes < result.semantic_replaced_bytes);
    assert!(text_contains(&result.messages, "Amsterdam.today=+24"));
    assert!(text_contains(&result.messages, "day9=+29"));
}

#[test]
fn low_semantic_projection_keeps_the_recent_tool_call_result_pair() {
    let mut messages = weather_messages(3);
    messages.push(user("Прочитай файл."));
    messages.push(tool_call("tool-1"));
    messages.push(tool_result("tool-1"));
    messages.push(assistant("Файл прочитан."));

    let mut projector = WorkflowContextProjector::default();
    let result =
        projector.project_workflow_context_with_retention(&messages, ContextRetention::Low);

    assert!(result.semantic_compressed);
    assert!(result.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse { id, .. } if id == "tool-1"))
    }));
    assert!(result.messages.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "tool-1")
        })
    }));
}

#[test]
fn mid_semantic_projection_keeps_multiple_recent_tool_call_result_pairs() {
    let mut messages = weather_messages_through(6);
    messages.push(user("Прочитай конфигурацию."));
    messages.push(tool_call("tool-1"));
    messages.push(tool_result("tool-1"));
    messages.push(assistant("Конфигурация прочитана."));
    messages.push(user("Прочитай README."));
    messages.push(tool_call("tool-2"));
    messages.push(tool_result("tool-2"));
    messages.push(assistant("README прочитан."));

    let mut projector = WorkflowContextProjector::default();
    let _ = projector.project_workflow_context_with_retention(
        &weather_messages_through(6),
        ContextRetention::Mid,
    );
    let result =
        projector.project_workflow_context_with_retention(&messages, ContextRetention::Mid);

    assert_eq!(result.reason, "state_first_semantic_mid_delta");
    assert!(result.semantic_compressed);
    for id in ["tool-1", "tool-2"] {
        assert!(result.messages.iter().any(|message| {
            message.content.iter().any(
                |block| matches!(block, ContentBlock::ToolUse { id: actual, .. } if actual == id),
            )
        }));
        assert!(result.messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id)
            })
        }));
    }
}

#[test]
fn low_semantic_state_handles_non_weather_fact_correction() {
    let messages = vec![
        user("Какой проект мы собираем?"),
        assistant("fact:project=orion"),
        user("На каком языке он написан?"),
        assistant("fact:language=rust"),
        user("Исправление названия проекта."),
        assistant("fact:project=nebula"),
    ];

    let mut projector = WorkflowContextProjector::default();
    let result =
        projector.project_workflow_context_with_retention(&messages, ContextRetention::Low);

    assert!(result.semantic_compressed);
    assert!(text_contains(&result.messages, "project=nebula"));
    assert!(text_contains(&result.messages, "language=rust"));
    assert!(!text_contains(&result.messages, "project=orion"));
}
