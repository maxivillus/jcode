use super::Agent;
use crate::context::ContextRevision;
use crate::context_controller::ContextPreflightPlan;
use crate::logging;
use crate::message::{Message, ToolDefinition};
use std::time::Instant;

pub(super) fn log_agent_provider_stream_lifecycle(
    level: logging::LogLevel,
    agent: &Agent,
    phase: &str,
    api_start: Instant,
    fields: Vec<(&str, String)>,
) {
    let mut owned = vec![
        ("phase".to_string(), phase.to_string()),
        ("provider".to_string(), agent.provider.name().to_string()),
        ("model".to_string(), agent.provider.model()),
        ("session_id".to_string(), agent.session.id.clone()),
        (
            "provider_session_id".to_string(),
            agent
                .provider_session_id
                .clone()
                .unwrap_or_else(|| "none".to_string()),
        ),
        (
            "connection_type".to_string(),
            agent
                .last_connection_type
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
        ),
        (
            "elapsed_ms".to_string(),
            api_start.elapsed().as_millis().to_string(),
        ),
    ];
    owned.extend(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_string(), value)),
    );
    logging::event(level, "AGENT_PROVIDER_STREAM_LIFECYCLE", owned);
}

pub(super) fn log_request(
    mode: &str,
    agent: &Agent,
    plan: ContextPreflightPlan,
    messages: &[Message],
    tools: &[ToolDefinition],
) {
    logging::event_debug(
        "CONTEXT_PROVIDER_REQUEST",
        vec![
            ("mode".to_string(), mode.to_string()),
            ("revision".to_string(), plan.revision.0.to_string()),
            (
                "estimated_input_tokens".to_string(),
                plan.estimated_input_tokens.to_string(),
            ),
            (
                "max_input_tokens".to_string(),
                plan.max_input_tokens.to_string(),
            ),
            ("message_count".to_string(), messages.len().to_string()),
            ("tool_count".to_string(), tools.len().to_string()),
            (
                "provider_session_present".to_string(),
                agent.provider_session_id.is_some().to_string(),
            ),
        ],
    );
}

pub(super) fn log_usage(
    agent: &Agent,
    mode: &str,
    revision: ContextRevision,
    revision_accepted: bool,
) {
    logging::event_debug(
        "CONTEXT_PROVIDER_USAGE",
        vec![
            ("mode".to_string(), mode.to_string()),
            ("revision".to_string(), revision.0.to_string()),
            (
                "input_tokens".to_string(),
                agent.last_usage.input_tokens.to_string(),
            ),
            (
                "output_tokens".to_string(),
                agent.last_usage.output_tokens.to_string(),
            ),
            (
                "cache_read_input_tokens".to_string(),
                agent
                    .last_usage
                    .cache_read_input_tokens
                    .unwrap_or(0)
                    .to_string(),
            ),
            (
                "cache_creation_input_tokens".to_string(),
                agent
                    .last_usage
                    .cache_creation_input_tokens
                    .unwrap_or(0)
                    .to_string(),
            ),
            (
                "context_revision_accepted".to_string(),
                revision_accepted.to_string(),
            ),
        ],
    );
}

pub(super) fn record_and_log_usage(
    agent: &mut Agent,
    mode: &str,
    revision: ContextRevision,
    input: Option<u64>,
) -> bool {
    let accepted = agent.record_context_usage(revision, input);
    log_usage(agent, mode, revision, accepted);
    accepted
}
