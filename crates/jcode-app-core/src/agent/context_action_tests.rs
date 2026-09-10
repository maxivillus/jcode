use super::Agent;
use crate::context::ContextComponentHashes;
use crate::context_controller::{
    ContextActionKind, ContextActionOutcome, ContextPruneKind, ContextPruneSpec,
};
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

fn request_prune(agent: &Agent, kind: ContextPruneKind, keep_recent: Option<usize>) {
    let revision = agent
        .context_controller
        .lock()
        .expect("controller lock")
        .manifest()
        .revision;
    let mut spec = ContextPruneSpec::new(kind);
    if let Some(keep_recent) = keep_recent {
        spec = spec.keep_recent(keep_recent);
    }
    agent
        .context_controller
        .lock()
        .expect("controller lock")
        .request_prune(spec, revision)
        .expect("prune request should be accepted");
}

fn count_images(agent: &Agent) -> usize {
    agent
        .session
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter(|block| matches!(block, ContentBlock::Image { .. }))
        .count()
}

fn pending_memory(computed_at: std::time::Instant) -> crate::memory::PendingMemory {
    crate::memory::PendingMemory {
        prompt: "# Memory".to_string(),
        display_prompt: None,
        computed_at,
        count: 1,
        memory_ids: vec!["mem-1".to_string()],
    }
}

#[tokio::test]
async fn memory_computed_before_a_transcript_mutation_is_rejected() {
    let mut agent = test_agent().await;
    let before_mutation = pending_memory(std::time::Instant::now());
    assert!(agent.memory_matches_transcript(&before_mutation));

    agent.note_transcript_mutation();

    assert!(
        !agent.memory_matches_transcript(&before_mutation),
        "memory computed before a transcript mutation must be rejected"
    );
    let after_mutation = pending_memory(std::time::Instant::now());
    assert!(agent.memory_matches_transcript(&after_mutation));
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

#[tokio::test]
async fn prune_images_replaces_stale_images_and_undo_restores() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = test_agent().await;
    for index in 0..3 {
        agent.add_message(
            Role::User,
            vec![
                ContentBlock::Text {
                    text: format!("turn {index}"),
                    cache_control: None,
                },
                ContentBlock::Image {
                    media_type: "image/png".to_string(),
                    data: "aGVsbG8=".to_string(),
                },
            ],
        );
    }
    assert_eq!(count_images(&agent), 3);

    request_prune(&agent, ContextPruneKind::Images, Some(1));
    agent.apply_pending_context_actions();

    match last_outcome(&agent) {
        ContextActionOutcome::Completed { detail } => {
            assert!(detail.contains("pruned 2 item(s)"), "got: {detail}");
        }
        other => panic!("expected Completed image prune, got {other:?}"),
    }
    assert_eq!(count_images(&agent), 1);

    request_action(&agent, ContextActionKind::UndoPrune);
    agent.apply_pending_context_actions();

    assert!(matches!(
        last_outcome(&agent),
        ContextActionOutcome::Completed { .. }
    ));
    assert_eq!(count_images(&agent), 3);
}

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
}

#[tokio::test]
async fn prune_turns_keeps_recent_history_and_undo_restores() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = test_agent().await;
    for index in 0..6 {
        agent.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: format!("user {index}"),
                cache_control: None,
            }],
        );
        agent.add_message(
            Role::Assistant,
            vec![ContentBlock::Text {
                text: format!("assistant {index}"),
                cache_control: None,
            }],
        );
    }
    let before = agent.session.messages.len();

    request_prune(&agent, ContextPruneKind::Turns, Some(4));
    agent.apply_pending_context_actions();

    assert!(matches!(
        last_outcome(&agent),
        ContextActionOutcome::Completed { .. }
    ));
    let after = agent.session.messages.len();
    assert!(
        after < before,
        "turn prune must drop history: {before} -> {after}"
    );
    assert_eq!(
        agent.session.messages[0].role,
        Role::User,
        "pruned transcript must start with a user message"
    );

    request_action(&agent, ContextActionKind::UndoPrune);
    agent.apply_pending_context_actions();

    assert_eq!(agent.session.messages.len(), before);
}

#[tokio::test]
async fn tools_change_invalidates_resumable_provider_session() {
    let mut agent = test_agent().await;
    let baseline =
        ContextComponentHashes::from_texts(None, None, None, None, Some("tools-a"), None);
    agent.provider_session_id = Some("provider-session".to_string());
    agent.session.provider_session_id = Some("provider-session".to_string());

    agent.refresh_components_binding(&baseline);
    assert!(
        agent.provider_session_id.is_some(),
        "the first binding must not drop an existing session"
    );

    agent.refresh_components_binding(&baseline);
    assert!(
        agent.provider_session_id.is_some(),
        "unchanged tools must keep the resumable session"
    );

    let changed = ContextComponentHashes::from_texts(None, None, None, None, Some("tools-b"), None);
    agent.refresh_components_binding(&changed);

    assert!(agent.provider_session_id.is_none());
    assert!(agent.session.provider_session_id.is_none());
}

#[tokio::test]
async fn skills_change_invalidates_resumable_provider_session() {
    let mut agent = test_agent().await;
    let baseline =
        ContextComponentHashes::from_texts(None, None, Some("skills-a"), None, None, None);
    agent.refresh_components_binding(&baseline);
    agent.provider_session_id = Some("provider-session".to_string());
    agent.session.provider_session_id = Some("provider-session".to_string());

    let changed =
        ContextComponentHashes::from_texts(None, None, Some("skills-b"), None, None, None);
    agent.refresh_components_binding(&changed);

    assert!(agent.provider_session_id.is_none());
    assert!(agent.session.provider_session_id.is_none());
}
