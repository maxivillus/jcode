use super::Agent;
use crate::context::ContextComponentHashes;
use crate::context_controller::{ContextActionKind, ContextActionOutcome};
use crate::message::{ContentBlock, Message, Role, ToolDefinition};
use crate::provider::{EventStream, Provider};
use crate::tool::Registry;
use async_trait::async_trait;
use std::sync::Arc;

struct StubProvider;

#[async_trait]
impl Provider for StubProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> anyhow::Result<EventStream> {
        Err(anyhow::anyhow!("stub provider does not complete requests"))
    }

    fn name(&self) -> &str {
        "stub"
    }

    fn supports_compaction(&self) -> bool {
        true
    }

    fn uses_jcode_compaction(&self) -> bool {
        false
    }

    fn context_window(&self) -> usize {
        1_000
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(StubProvider)
    }
}

async fn test_agent() -> Agent {
    let provider: Arc<dyn Provider> = Arc::new(StubProvider);
    let registry = Registry::new(Arc::clone(&provider)).await;
    Agent::new(provider, registry)
}

fn request_action(agent: &Agent, action: ContextActionKind) {
    let revision = agent
        .context_controller
        .lock()
        .expect("controller lock")
        .manifest()
        .revision;
    agent
        .context_controller
        .lock()
        .expect("controller lock")
        .request_action(action, revision)
        .expect("request should be accepted");
}

fn last_outcome(agent: &Agent) -> ContextActionOutcome {
    agent
        .context_controller
        .lock()
        .expect("controller lock")
        .last_action()
        .expect("outcome should be recorded")
        .outcome
        .clone()
}

#[tokio::test]
async fn reset_provider_action_clears_session_on_turn_boundary() {
    let mut agent = test_agent().await;
    request_action(&agent, ContextActionKind::ResetProvider);
    agent.provider_session_id = Some("provider-session".to_string());
    agent.session.provider_session_id = Some("provider-session".to_string());

    agent.apply_pending_context_actions();

    assert!(agent.provider_session_id.is_none());
    assert!(agent.session.provider_session_id.is_none());
    assert!(matches!(
        last_outcome(&agent),
        ContextActionOutcome::Completed { .. }
    ));
}

#[tokio::test]
async fn stale_action_is_rejected_on_turn_boundary() {
    let mut agent = test_agent().await;
    request_action(&agent, ContextActionKind::ResetProvider);
    agent
        .context_controller
        .lock()
        .expect("controller lock")
        .update_sources(ContextComponentHashes::default(), 42);
    agent.provider_session_id = Some("provider-session".to_string());
    agent.session.provider_session_id = Some("provider-session".to_string());

    agent.apply_pending_context_actions();

    assert!(agent.provider_session_id.is_some());
    assert!(agent.session.provider_session_id.is_some());
    assert!(matches!(
        last_outcome(&agent),
        ContextActionOutcome::Rejected { .. }
    ));
}

#[tokio::test]
async fn refresh_action_keeps_transcript_untouched() {
    let mut agent = test_agent().await;
    for index in 0..3 {
        agent.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: format!("turn {index}"),
                cache_control: None,
            }],
        );
    }
    let messages_before = agent.session.messages.len();
    request_action(&agent, ContextActionKind::Refresh);

    agent.apply_pending_context_actions();

    assert_eq!(agent.session.messages.len(), messages_before);
    assert!(matches!(
        last_outcome(&agent),
        ContextActionOutcome::Completed { .. }
    ));
}

#[tokio::test]
async fn compact_action_drops_history_and_resets_provider_session() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = test_agent().await;
    for index in 0..30 {
        agent.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: format!("turn {index} {}", "x".repeat(120)),
                cache_control: None,
            }],
        );
    }
    let before = agent.messages_for_provider().0.len();
    agent.provider_session_id = Some("provider-session".to_string());
    agent.session.provider_session_id = Some("provider-session".to_string());
    request_action(&agent, ContextActionKind::Compact);

    agent.apply_pending_context_actions();

    match last_outcome(&agent) {
        ContextActionOutcome::Completed { detail } => {
            assert!(!detail.is_empty(), "compaction must report its effect");
        }
        other => panic!("expected Completed compaction, got {other:?}"),
    }
    assert!(
        agent.messages_for_provider().0.len() < before,
        "compaction must shrink the provider-facing history"
    );
    assert!(agent.provider_session_id.is_none());
    assert!(
        agent.session.compaction.is_some(),
        "compaction state must persist"
    );
}

#[tokio::test]
async fn compact_action_skips_short_history_with_reason() {
    let mut agent = test_agent().await;
    request_action(&agent, ContextActionKind::Compact);

    agent.apply_pending_context_actions();

    match last_outcome(&agent) {
        ContextActionOutcome::Skipped { reason } => {
            assert!(!reason.is_empty(), "skip reason must be explicit");
        }
        other => panic!("expected Skipped for a short history, got {other:?}"),
    }
}
