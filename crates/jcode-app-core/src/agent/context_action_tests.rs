use super::Agent;
use crate::context::{ContextBudget, ContextComponentHashes, ContextRevision};
use crate::context_controller::{
    ContextActionKind, ContextActionOutcome, ContextPruneForecast, ContextPruneKind,
    ContextPruneSpec,
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

pub(super) async fn test_agent() -> Agent {
    let provider: Arc<dyn Provider> = Arc::new(StubProvider);
    let registry = Registry::new(Arc::clone(&provider)).await;
    Agent::new(provider, registry)
}

pub(super) fn request_action(agent: &Agent, action: ContextActionKind) {
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

pub(super) fn last_outcome(agent: &Agent) -> ContextActionOutcome {
    agent
        .context_controller
        .lock()
        .expect("controller lock")
        .last_action()
        .expect("outcome should be recorded")
        .outcome
        .clone()
}

pub(super) fn request_prune(agent: &Agent, kind: ContextPruneKind, keep_recent: Option<usize>) {
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

/// Заявка на срез хвоста после указанного сообщения.
fn request_tail_prune(agent: &Agent, after: &str) {
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
        .request_prune(
            ContextPruneSpec::new(ContextPruneKind::Tail).after(after),
            revision,
        )
        .expect("tail prune request should be accepted");
}

/// Расчёт среза хвоста из снимка, который снимает preflight на границе turn-а.
fn tail_forecast(agent: &Agent, after: &str) -> Option<ContextPruneForecast> {
    agent
        .context_controller
        .lock()
        .expect("controller lock")
        .prune_forecast(ContextPruneSpec::new(ContextPruneKind::Tail).after(after))
}

/// Поднимает revision контекста так же, как preflight перед запросом:
/// меняются hashes компонентов, поэтому revision увеличивается.
fn bump_context_revision(agent: &Agent, skills: &str) -> ContextRevision {
    let budget = ContextBudget {
        provider_context_limit: 100_000,
        reserved_output_tokens: 1_000,
        safety_margin_tokens: 1_000,
        estimated_input_tokens: 0,
    };
    let components = ContextComponentHashes::from_texts(None, None, Some(skills), None, None, None);
    agent
        .context_controller
        .lock()
        .expect("controller lock")
        .prepare(&budget, components, 1)
        .revision
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

/// Разбирает оценку размера из результата обрезки: `... estimated tokens A -> B`.
pub(super) fn estimated_tokens(detail: &str) -> (usize, usize) {
    let tail = detail
        .split("estimated tokens ")
        .nth(1)
        .unwrap_or_else(|| panic!("no token estimate in outcome: {detail}"));
    let (before, after) = tail
        .split_once(" -> ")
        .unwrap_or_else(|| panic!("no token arrow in outcome: {detail}"));
    (
        before.trim().parse().expect("before tokens"),
        after.trim().parse().expect("after tokens"),
    )
}

pub(super) fn completed_detail(agent: &Agent) -> String {
    match last_outcome(agent) {
        ContextActionOutcome::Completed { detail } => detail,
        other => panic!("expected Completed outcome, got {other:?}"),
    }
}

/// Расчёт обрезки из снимка, который снимает preflight на границе turn-а.
pub(super) fn prune_forecast(
    agent: &Agent,
    kind: ContextPruneKind,
    keep_recent: Option<usize>,
) -> crate::context_controller::ContextPruneForecast {
    let mut spec = ContextPruneSpec::new(kind);
    if let Some(keep_recent) = keep_recent {
        spec = spec.keep_recent(keep_recent);
    }
    agent
        .context_controller
        .lock()
        .expect("controller lock")
        .prune_forecast(spec)
        .expect("the snapshot must cover this prune kind")
}

/// Снимает проекцию так же, как preflight перед запросом.
pub(super) fn record_preview_snapshot(agent: &mut Agent) {
    let messages = agent.session.messages_for_provider_uncached();
    let prompt = agent.build_system_prompt_split(None);
    agent.prepare_context_preflight(&messages, &[], &prompt);
}

#[tokio::test]
async fn preview_forecast_matches_applied_image_prune() {
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
    record_preview_snapshot(&mut agent);

    let forecast = prune_forecast(&agent, ContextPruneKind::Images, Some(1));
    assert_eq!(forecast.removable_items, 2);
    assert!(forecast.tokens_exact);
    assert!(!forecast.is_no_op());

    request_prune(&agent, ContextPruneKind::Images, Some(1));
    agent.apply_pending_context_actions();

    let detail = completed_detail(&agent);
    let (before, after) = estimated_tokens(&detail);
    assert!(
        detail.contains(&format!("pruned {} item(s)", forecast.removable_items)),
        "applied prune must match the forecast, got: {detail}"
    );
    assert_eq!(
        before - after,
        forecast.removable_tokens,
        "preview must predict the tokens the prune frees: {detail}"
    );

    assert_eq!(
        count_images(&agent),
        1,
        "the next request must carry only the newest kept image"
    );
    assert!(
        agent
            .session
            .messages_for_provider()
            .iter()
            .flat_map(|message| message.content.iter())
            .filter(|block| matches!(block, ContentBlock::Image { .. }))
            .count()
            == 1,
        "the pruned images must also leave the cached provider view"
    );
    assert!(
        prune_forecast(&agent, ContextPruneKind::Images, Some(1)).is_no_op(),
        "the snapshot must follow the pruned transcript"
    );

    request_action(&agent, ContextActionKind::UndoPrune);
    agent.apply_pending_context_actions();

    let restored = prune_forecast(&agent, ContextPruneKind::Images, Some(1));
    assert_eq!(restored.removable_items, forecast.removable_items);
    assert_eq!(restored.removable_tokens, forecast.removable_tokens);
}

#[tokio::test]
async fn preview_forecast_matches_applied_turn_prune() {
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
    record_preview_snapshot(&mut agent);

    let forecast = prune_forecast(&agent, ContextPruneKind::Turns, Some(4));
    assert!(!forecast.is_no_op());
    assert!(
        !forecast.sample_removed.is_empty(),
        "the forecast must name the messages that leave"
    );

    request_prune(&agent, ContextPruneKind::Turns, Some(4));
    agent.apply_pending_context_actions();

    let detail = completed_detail(&agent);
    let (before, after) = estimated_tokens(&detail);
    assert!(
        detail.contains(&format!("pruned {} item(s)", forecast.removable_items)),
        "applied prune must match the forecast ({} item(s), {} tokens); got: {detail}",
        forecast.removable_items,
        forecast.removable_tokens
    );
    assert_eq!(
        before - after,
        forecast.removable_tokens,
        "preview must predict the tokens the prune frees: {detail}"
    );
}

#[tokio::test]
async fn preview_forecast_matches_applied_memory_injection_prune() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = test_agent().await;
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "real turn".to_string(),
            cache_control: None,
        }],
    );
    for index in 0..3 {
        agent.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: format!(
                    "<system-reminder>\n# Memory\n{index}. remembered fact {index}\n</system-reminder>"
                ),
                cache_control: None,
            }],
        );
    }
    record_preview_snapshot(&mut agent);

    let forecast = prune_forecast(&agent, ContextPruneKind::MemoryInjections, Some(1));
    assert_eq!(forecast.removable_items, 2);

    request_prune(&agent, ContextPruneKind::MemoryInjections, Some(1));
    agent.apply_pending_context_actions();

    let detail = completed_detail(&agent);
    let (before, after) = estimated_tokens(&detail);
    assert!(
        detail.contains(&format!("pruned {} item(s)", forecast.removable_items)),
        "applied prune must match the forecast, got: {detail}"
    );
    assert_eq!(
        before - after,
        forecast.removable_tokens,
        "preview must predict the tokens the prune frees: {detail}"
    );
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

/// Ответы инструментов, у которых в транскрипте нет своего вызова.
///
/// Провайдер отвергает такие блоки, поэтому после любой обрезки их быть не
/// должно.
fn orphan_tool_results(agent: &Agent) -> Vec<String> {
    let mut calls: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut orphans = Vec::new();
    for message in &agent.session.messages {
        for block in &message.content {
            match block {
                ContentBlock::ToolUse { id, .. } => {
                    calls.insert(id.clone());
                }
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    if !calls.contains(tool_use_id) {
                        orphans.push(tool_use_id.clone());
                    }
                }
                _ => {}
            }
        }
    }
    orphans
}

#[tokio::test]
async fn turns_prune_never_leaves_a_tool_result_without_its_call() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = test_agent().await;
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "turn 0".to_string(),
            cache_control: None,
        }],
    );
    agent.add_message(
        Role::Assistant,
        vec![ContentBlock::ToolUse {
            id: "call-1".to_string(),
            name: "read".to_string(),
            input: serde_json::json!({"file_path": "Cargo.toml"}),
            thought_signature: None,
        }],
    );
    agent.add_message(
        Role::User,
        vec![ContentBlock::ToolResult {
            tool_use_id: "call-1".to_string(),
            content: "payload".to_string(),
            is_error: None,
        }],
    );
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "turn 1".to_string(),
            cache_control: None,
        }],
    );

    request_prune(&agent, ContextPruneKind::Turns, Some(2));
    agent.apply_pending_context_actions();

    assert!(
        orphan_tool_results(&agent).is_empty(),
        "a kept tool result must keep its call; kept messages: {:?}",
        agent
            .session
            .messages
            .iter()
            .map(|message| message.id.clone())
            .collect::<Vec<String>>()
    );
}

#[tokio::test]
async fn turns_forecast_matches_the_applied_prune_across_a_tool_pair() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = test_agent().await;
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "turn 0".to_string(),
            cache_control: None,
        }],
    );
    agent.add_message(
        Role::Assistant,
        vec![ContentBlock::ToolUse {
            id: "call-1".to_string(),
            name: "read".to_string(),
            input: serde_json::json!({"file_path": "Cargo.toml"}),
            thought_signature: None,
        }],
    );
    agent.add_message(
        Role::User,
        vec![ContentBlock::ToolResult {
            tool_use_id: "call-1".to_string(),
            content: "payload".to_string(),
            is_error: None,
        }],
    );
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "turn 1".to_string(),
            cache_control: None,
        }],
    );
    record_preview_snapshot(&mut agent);

    let forecast = prune_forecast(&agent, ContextPruneKind::Turns, Some(1));
    assert!(!forecast.is_no_op());

    request_prune(&agent, ContextPruneKind::Turns, Some(1));
    agent.apply_pending_context_actions();

    let detail = completed_detail(&agent);
    let (before, after) = estimated_tokens(&detail);
    assert!(
        detail.contains(&format!("pruned {} item(s)", forecast.removable_items)),
        "applied prune must match the forecast, got: {detail}"
    );
    assert_eq!(
        before - after,
        forecast.removable_tokens,
        "preview must predict the tokens the prune frees: {detail}"
    );
    assert!(orphan_tool_results(&agent).is_empty());
}

#[tokio::test]
async fn tail_prune_refuses_a_cut_between_a_call_and_its_result() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = test_agent().await;
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "turn 0".to_string(),
            cache_control: None,
        }],
    );
    agent.add_message(
        Role::Assistant,
        vec![ContentBlock::ToolUse {
            id: "call-1".to_string(),
            name: "read".to_string(),
            input: serde_json::json!({"file_path": "Cargo.toml"}),
            thought_signature: None,
        }],
    );
    // Сообщение пользователя приходит до ответа инструмента.
    let between = agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "заметка".to_string(),
            cache_control: None,
        }],
    );
    agent.add_message(
        Role::User,
        vec![ContentBlock::ToolResult {
            tool_use_id: "call-1".to_string(),
            content: "payload".to_string(),
            is_error: None,
        }],
    );
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "turn 1".to_string(),
            cache_control: None,
        }],
    );
    record_preview_snapshot(&mut agent);

    assert!(
        tail_forecast(&agent, &between).is_none(),
        "срез между вызовом и ответом не предлагается: префикс оставил бы вызов без ответа"
    );

    request_tail_prune(&agent, &between);
    agent.apply_pending_context_actions();

    assert!(
        matches!(last_outcome(&agent), ContextActionOutcome::Skipped { .. }),
        "срез между вызовом и ответом не должен применяться: {:?}",
        last_outcome(&agent)
    );
    assert!(orphan_tool_results(&agent).is_empty());
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

/// Поздний provider-ответ, собранный на прежней revision, не должен
/// продолжать устаревшую upstream-сессию и не должен попадать в usage.
#[tokio::test]
async fn stale_provider_response_does_not_resume_a_stale_provider_session() {
    let mut agent = test_agent().await;
    let request_revision = bump_context_revision(&agent, "skills-a");

    // Пока запрос был в полёте, контекст изменился.
    let current_revision = bump_context_revision(&agent, "skills-b");
    assert!(current_revision > request_revision);

    // Ответ приходит от session, которая всё ещё собрана из прежней revision.
    agent.provider_session_id = Some("provider-session".to_string());
    agent.session.provider_session_id = Some("provider-session".to_string());

    agent.record_context_usage(request_revision, Some(1_234));

    assert!(agent.provider_session_id.is_none());
    assert!(agent.session.provider_session_id.is_none());
    assert_eq!(
        agent.last_stale_provider_revision(),
        Some(request_revision.0)
    );
    assert_eq!(
        agent
            .context_controller
            .lock()
            .expect("controller lock")
            .manifest()
            .observed_input_tokens,
        None,
        "usage from a stale revision must not be recorded"
    );
}

/// Ответ без usage тоже проверяется: устаревшая revision сбрасывает
/// resumable provider session, актуальная не трогает её.
#[tokio::test]
async fn stale_provider_response_is_detected_without_usage() {
    let mut agent = test_agent().await;
    let request_revision = bump_context_revision(&agent, "skills-a");
    let current_revision = bump_context_revision(&agent, "skills-b");

    agent.provider_session_id = Some("provider-session".to_string());

    assert!(
        !agent.note_provider_response_revision(request_revision),
        "late response for a stale revision must be reported"
    );
    assert!(agent.provider_session_id.is_none());
    assert_eq!(
        agent.last_stale_provider_revision(),
        Some(request_revision.0)
    );

    agent.provider_session_id = Some("provider-session".to_string());

    assert!(agent.note_provider_response_revision(current_revision));
    assert!(agent.provider_session_id.is_some());
    assert_eq!(
        agent.last_stale_provider_revision(),
        Some(request_revision.0)
    );
}

#[tokio::test]
async fn prune_memory_injections_removes_stale_payloads_and_undo_restores() {
    let mut agent = test_agent().await;
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "real turn".to_string(),
            cache_control: None,
        }],
    );
    for index in 0..3 {
        agent.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: format!(
                    "<system-reminder>\n# Memory\n{index}. remembered fact {index}\n</system-reminder>"
                ),
                cache_control: None,
            }],
        );
    }
    let before = agent.session.messages.len();
    request_prune(&agent, ContextPruneKind::MemoryInjections, Some(0));

    agent.apply_pending_context_actions();

    assert!(matches!(
        last_outcome(&agent),
        ContextActionOutcome::Completed { .. }
    ));
    assert_eq!(agent.session.messages.len(), before - 3);
    assert!(
        agent.session.messages.iter().all(|message| {
            !message.content.iter().any(|block| {
                matches!(block, ContentBlock::Text { text, .. } if text.contains("# Memory"))
            })
        }),
        "memory injections must be gone"
    );

    request_action(&agent, ContextActionKind::UndoPrune);
    agent.apply_pending_context_actions();

    assert_eq!(agent.session.messages.len(), before);
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
#[tokio::test]
async fn preview_forecast_matches_applied_tail_prune() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = test_agent().await;
    let mut ids = Vec::new();
    for index in 0..4 {
        ids.push(agent.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: format!("turn {index}"),
                cache_control: None,
            }],
        ));
    }
    record_preview_snapshot(&mut agent);
    let stored_before = agent.session.messages.len();

    let forecast = tail_forecast(&agent, &ids[1]).expect("the snapshot knows this cut");
    assert_eq!(forecast.removable_items, 2);
    assert!(forecast.tokens_exact);
    assert!(!forecast.is_no_op());
    assert_eq!(
        forecast.sample_removed.len(),
        2,
        "the forecast must show the removed tail"
    );

    request_tail_prune(&agent, &ids[1]);
    agent.apply_pending_context_actions();

    let detail = completed_detail(&agent);
    let (before, after) = estimated_tokens(&detail);
    assert!(
        detail.contains(&format!("pruned {} item(s)", forecast.removable_items)),
        "applied prune must match the forecast, got: {detail}"
    );
    assert_eq!(
        before - after,
        forecast.removable_tokens,
        "preview must predict the tokens the tail prune frees: {detail}"
    );
    assert_eq!(
        agent.session.messages.len(),
        stored_before - 2,
        "only the kept prefix must remain"
    );
    assert_eq!(
        agent.session.messages.last().expect("kept prefix").id,
        ids[1],
        "the cut keeps the message named by after"
    );

    request_action(&agent, ContextActionKind::UndoPrune);
    agent.apply_pending_context_actions();

    assert_eq!(
        agent.session.messages.len(),
        stored_before,
        "undo must restore the removed tail"
    );
    assert_eq!(
        agent.session.messages.last().expect("restored tail").id,
        ids[3],
        "undo must bring back every removed message"
    );
}

#[tokio::test]
async fn tail_prune_skips_a_cut_that_leaves_an_unanswered_tool_call() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = test_agent().await;
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "start".to_string(),
            cache_control: None,
        }],
    );
    let call = agent.add_message(
        Role::Assistant,
        vec![ContentBlock::ToolUse {
            id: "call-1".to_string(),
            name: "read".to_string(),
            input: serde_json::json!({"file_path": "Cargo.toml"}),
            thought_signature: None,
        }],
    );
    agent.add_message(
        Role::User,
        vec![ContentBlock::ToolResult {
            tool_use_id: "call-1".to_string(),
            content: "payload".to_string(),
            is_error: None,
        }],
    );
    record_preview_snapshot(&mut agent);
    let stored_before = agent.session.messages.len();

    assert!(
        tail_forecast(&agent, &call).is_none(),
        "a cut right after a tool call must not be offered"
    );

    request_tail_prune(&agent, &call);
    agent.apply_pending_context_actions();

    match last_outcome(&agent) {
        ContextActionOutcome::Skipped { reason } => assert!(
            reason.contains("unanswered tool call"),
            "the skip reason must explain the unsafe cut: {reason}"
        ),
        other => panic!("expected Skipped for an unsafe cut, got {other:?}"),
    }
    assert_eq!(
        agent.session.messages.len(),
        stored_before,
        "the transcript must stay intact"
    );
}
