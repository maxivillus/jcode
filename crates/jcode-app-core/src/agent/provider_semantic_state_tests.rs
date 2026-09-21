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

fn state_messages(step: usize) -> Vec<Message> {
    let mut messages = Vec::new();
    if step >= 1 {
        messages.push(user("What is the current project status?"));
        messages.push(assistant("fact:status=ready"));
    }
    if step >= 2 {
        messages.push(user("Who owns the project?"));
        messages.push(assistant("fact:owner=mira"));
    }
    if step >= 3 {
        messages.push(user("What is the project deadline?"));
        messages.push(assistant("fact:deadline=friday"));
    }
    messages
}

fn state_messages_through(step: usize) -> Vec<Message> {
    let mut messages = state_messages(step.min(3));
    for current in 4..=step {
        messages.push(user(&format!("Record the project update {current}.")));
        messages.push(assistant(&format!("fact:day{current}=value-{current}")));
    }
    messages
}

#[test]
fn semantic_state_ignores_unstructured_domain_examples() {
    let messages = vec![
        user("Какая сейчас погода?"),
        assistant("Тепло. В Амстердаме +24. Завтра +26."),
        user("Повтори наблюдение."),
        assistant("Всё ещё тепло. В Амстердаме +24. Завтра +26."),
        user("Подтверди последнее наблюдение."),
        assistant("Последнее наблюдение: тепло, в Амстердаме +24, завтра +26."),
    ];

    let mut projector = WorkflowContextProjector::default();
    let result =
        projector.project_workflow_context_with_retention(&messages, ContextRetention::Low);

    assert!(result.semantic_compressed);
    assert_eq!(projector.semantic_state_facts(), Some(Vec::new()));
}

#[test]
fn low_semantic_state_updates_each_step_and_compresses_at_n_three() {
    let mut projector = WorkflowContextProjector::default();

    let first = projector
        .project_workflow_context_with_retention(&state_messages(1), ContextRetention::Low);
    assert_eq!(first.reason, "state_first_latest_turn");
    assert_eq!(
        projector.semantic_state_facts(),
        Some(vec!["status=ready".to_string()])
    );

    let second = projector
        .project_workflow_context_with_retention(&state_messages(2), ContextRetention::Low);
    assert_eq!(second.reason, "state_first_latest_turn");
    assert_eq!(
        projector.semantic_state_facts(),
        Some(vec!["owner=mira".to_string(), "status=ready".to_string()])
    );

    let third = projector
        .project_workflow_context_with_retention(&state_messages(3), ContextRetention::Low);
    assert_eq!(third.reason, "state_first_semantic_low_rebuild");
    assert_eq!(third.after_turn_groups, 1);
    assert!(third.semantic_compressed);
    assert!(third.semantic_state_bytes < third.semantic_replaced_bytes);
    assert!(text_contains(&third.messages, "status=ready"));
    assert!(text_contains(&third.messages, "owner=mira"));
    assert!(text_contains(&third.messages, "deadline=friday"));
    assert!(text_contains(
        &third.messages,
        BOUNDED_SEMANTIC_STATE_MARKER
    ));
}

#[test]
fn low_semantic_state_replaces_contradictory_structured_fact() {
    let mut projector = WorkflowContextProjector::default();
    let initial = state_messages(3);
    let _ = projector.project_workflow_context_with_retention(&initial, ContextRetention::Low);

    let mut refreshed = initial;
    refreshed.push(user("What is the current project status?"));
    refreshed.push(assistant("fact:status=blocked"));
    let result =
        projector.project_workflow_context_with_retention(&refreshed, ContextRetention::Low);

    assert!(result.reason.starts_with("state_first_semantic_low_"));
    assert!(text_contains(&result.messages, "status=blocked"));
    assert!(!text_contains(&result.messages, "status=ready"));
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
        super::retention_settings_with_interval(ContextRetention::Low, None)
            .semantic_rebuild_interval,
        Some(3)
    );
    assert_eq!(
        super::retention_settings_with_interval(ContextRetention::Low, None).semantic_tail_groups,
        1
    );
    assert_eq!(
        super::retention_settings_with_interval(ContextRetention::Mid, None)
            .semantic_rebuild_interval,
        Some(6)
    );
    assert_eq!(
        super::retention_settings_with_interval(ContextRetention::Mid, None).semantic_tail_groups,
        2
    );
    assert_eq!(
        super::retention_settings_with_interval(ContextRetention::High, None)
            .semantic_rebuild_interval,
        Some(9)
    );
    assert_eq!(
        super::retention_settings_with_interval(ContextRetention::High, None).semantic_tail_groups,
        4
    );
    assert_eq!(
        super::retention_settings_with_interval(ContextRetention::Disabled, None)
            .semantic_rebuild_interval,
        None
    );
    assert_eq!(
        super::retention_settings_with_interval(ContextRetention::Mid, Some(0))
            .semantic_rebuild_interval,
        None
    );
}

#[test]
fn configured_rebuild_interval_overrides_the_mode_default() {
    let mut projector = WorkflowContextProjector::default();
    let result = projector.project_workflow_context_with_retention_and_interval(
        &state_messages_through(4),
        ContextRetention::Mid,
        Some(4),
    );

    assert_eq!(result.reason, "state_first_semantic_mid_rebuild");
    assert_eq!(result.after_turn_groups, 2);
    assert!(result.semantic_compressed);
}

#[test]
fn mid_semantic_state_rebuilds_at_n_six_and_keeps_two_recent_turns() {
    let mut projector = WorkflowContextProjector::default();
    let result = projector
        .project_workflow_context_with_retention(&state_messages_through(6), ContextRetention::Mid);

    assert_eq!(result.reason, "state_first_semantic_mid_rebuild");
    assert_eq!(result.after_turn_groups, 2);
    assert!(result.semantic_compressed);
    assert!(result.semantic_state_bytes < result.semantic_replaced_bytes);
    assert!(text_contains(&result.messages, "status=ready"));
    assert!(text_contains(&result.messages, "day6=value-6"));
}

#[test]
fn mid_semantic_state_applies_delta_between_rebuilds() {
    let mut projector = WorkflowContextProjector::default();
    let _ = projector
        .project_workflow_context_with_retention(&state_messages_through(6), ContextRetention::Mid);
    let result = projector
        .project_workflow_context_with_retention(&state_messages_through(7), ContextRetention::Mid);

    assert_eq!(result.reason, "state_first_semantic_mid_delta");
    assert_eq!(result.after_turn_groups, 2);
    assert!(text_contains(&result.messages, "day7=value-7"));
}

#[test]
fn high_semantic_state_rebuilds_at_n_nine_and_keeps_four_recent_turns() {
    let mut projector = WorkflowContextProjector::default();
    let result = projector.project_workflow_context_with_retention(
        &state_messages_through(9),
        ContextRetention::High,
    );

    assert_eq!(result.reason, "state_first_semantic_high_rebuild");
    assert_eq!(result.after_turn_groups, 4);
    assert!(result.semantic_compressed);
    assert!(result.semantic_state_bytes < result.semantic_replaced_bytes);
    assert!(text_contains(&result.messages, "status=ready"));
    assert!(text_contains(&result.messages, "day9=value-9"));
}

#[test]
fn low_semantic_projection_keeps_the_recent_tool_call_result_pair() {
    let mut messages = state_messages(3);
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
    let mut messages = state_messages_through(6);
    messages.push(user("Прочитай конфигурацию."));
    messages.push(tool_call("tool-1"));
    messages.push(tool_result("tool-1"));
    messages.push(assistant("Конфигурация прочитана."));
    messages.push(user("Прочитай README."));
    messages.push(tool_call("tool-2"));
    messages.push(tool_result("tool-2"));
    messages.push(assistant("README прочитан."));

    let mut projector = WorkflowContextProjector::default();
    let _ = projector
        .project_workflow_context_with_retention(&state_messages_through(6), ContextRetention::Mid);
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
fn low_semantic_state_handles_structured_fact_correction() {
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

#[test]
fn low_semantic_state_preserves_structured_deployment_constraint() {
    let marker = "PROJECT=ORION OWNER=MIRA DEADLINE=FRIDAY STATUS=AMBER LOCATION=REMOTE APPROVAL=GRANTED DEPLOY=READY ACTION=PROCEED";
    let mut messages = Vec::new();
    for turn in 1..=3 {
        messages.push(user(&format!("Проверь текущий план на ходу {turn}.")));
        messages.push(assistant(marker));
    }

    let mut projector = WorkflowContextProjector::default();
    let result =
        projector.project_workflow_context_with_retention(&messages, ContextRetention::Low);

    assert!(result.semantic_compressed);
    assert!(text_contains(&result.messages, "approval=GRANTED"));
    assert!(text_contains(&result.messages, "deploy=READY"));
    assert!(text_contains(&result.messages, "action=PROCEED"));
}

fn long_fact_messages(turn_count: usize) -> Vec<Message> {
    let mut messages = Vec::new();
    for turn in 1..=turn_count {
        messages.push(user(&format!("What is the current phase at turn {turn}?")));
        messages.push(assistant(&format!(
            "fact:phase=phase-{turn}; fact:owner=team"
        )));
    }
    messages
}

#[test]
fn retention_modes_keep_latest_fact_across_a_long_incremental_history() {
    for (retention, expected_tail_groups) in [
        (ContextRetention::Low, 1),
        (ContextRetention::Mid, 2),
        (ContextRetention::High, 4),
    ] {
        let mut projector = WorkflowContextProjector::default();
        let mut result = None;
        for turn_count in 1..=100 {
            result = Some(projector.project_workflow_context_with_retention(
                &long_fact_messages(turn_count),
                retention,
            ));
        }

        let result = result.expect("long history must produce a final projection");
        assert!(result.semantic_compressed);
        assert_eq!(result.after_turn_groups, expected_tail_groups);
        assert!(text_contains(&result.messages, "phase=phase-100"));
        assert!(text_contains(&result.messages, "owner=team"));
        assert!(result.semantic_state_bytes < result.semantic_replaced_bytes);
    }
}
