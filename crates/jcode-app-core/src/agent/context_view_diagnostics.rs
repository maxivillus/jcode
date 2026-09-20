use super::provider_context_view::WorkflowContextView;

pub(super) fn event_fields(
    result: &WorkflowContextView,
    provider_context_generation: u64,
) -> Vec<(String, String)> {
    vec![
        ("mode".to_string(), result.mode.to_string()),
        (
            "retention".to_string(),
            result.retention.as_str().to_string(),
        ),
        ("reason".to_string(), result.reason.clone()),
        (
            "source_version".to_string(),
            format!("{:016x}", result.source_version),
        ),
        (
            "view_version".to_string(),
            format!("{:016x}", result.view_version),
        ),
        (
            "provider_context_generation".to_string(),
            provider_context_generation.to_string(),
        ),
        ("active".to_string(), result.active.to_string()),
        (
            "before_tokens".to_string(),
            result.before_tokens.to_string(),
        ),
        ("after_tokens".to_string(), result.after_tokens.to_string()),
        (
            "before_turn_groups".to_string(),
            result.before_turn_groups.to_string(),
        ),
        (
            "after_turn_groups".to_string(),
            result.after_turn_groups.to_string(),
        ),
        (
            "excluded_messages".to_string(),
            result.excluded_messages.to_string(),
        ),
        (
            "excluded_turn_groups".to_string(),
            result.excluded_turn_groups.to_string(),
        ),
        (
            "summary_messages".to_string(),
            result.summary_messages.to_string(),
        ),
        (
            "summary_source_version".to_string(),
            result
                .summary_source_version
                .map(|version| format!("{version:016x}"))
                .unwrap_or_else(|| "none".to_string()),
        ),
        (
            "semantic_state_bytes".to_string(),
            result.semantic_state_bytes.to_string(),
        ),
        (
            "semantic_replaced_bytes".to_string(),
            result.semantic_replaced_bytes.to_string(),
        ),
        (
            "semantic_compressed".to_string(),
            result.semantic_compressed.to_string(),
        ),
        ("unknown_relevance".to_string(), "not_proven".to_string()),
    ]
}
