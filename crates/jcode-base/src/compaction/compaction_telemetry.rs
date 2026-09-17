use jcode_compaction_core::CompactionEvent;

fn optional_metric<T: ToString>(value: Option<T>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "none".to_string())
}

pub(super) fn event(mode: &str, event: Option<&CompactionEvent>) {
    let Some(event) = event else {
        return;
    };
    crate::logging::event_info(
        "CONTEXT_COMPACTION_APPLIED",
        vec![
            ("mode".to_string(), mode.to_string()),
            (
                "trigger".to_string(),
                crate::logging::truncate_for_log(&event.trigger, 80),
            ),
            ("pre_tokens".to_string(), optional_metric(event.pre_tokens)),
            (
                "post_tokens".to_string(),
                optional_metric(event.post_tokens),
            ),
            (
                "tokens_saved".to_string(),
                optional_metric(event.tokens_saved),
            ),
            (
                "duration_ms".to_string(),
                optional_metric(event.duration_ms),
            ),
            (
                "messages_dropped".to_string(),
                optional_metric(event.messages_dropped),
            ),
            (
                "messages_compacted".to_string(),
                optional_metric(event.messages_compacted),
            ),
            (
                "summary_chars".to_string(),
                optional_metric(event.summary_chars),
            ),
            (
                "active_messages".to_string(),
                optional_metric(event.active_messages),
            ),
        ],
    );
}
