use super::*;
use crate::agent::provider_context_view::WorkflowContextProjector;
use crate::context_controller::{ContextActionOutcome, ContextPruneKind, ContextPruneSpec};
use crate::message::{CacheControl, ContentBlock, Message, Role, StreamEvent, ToolDefinition};
use crate::provider::{EventStream, Provider};
use anyhow::Result;
use async_trait::async_trait;
use futures::{StreamExt, stream};
use std::sync::{Arc, Mutex};

const BENCHMARK_ITERATIONS: usize = 3;
const SYSTEM_STATIC: &str = "You are a deterministic context-control benchmark provider.";
const SYSTEM_DYNAMIC: &str = "Keep the retained image and the latest tool result unchanged.";
const STATE_FIRST_DYNAMIC: &str =
    "Keep the bounded workflow state and latest observation unchanged.";

#[derive(Clone, Debug)]
struct ProviderRequestMetrics {
    message_count: usize,
    tool_count: usize,
    image_count: usize,
    cache_control_count: usize,
    tool_result_chars: usize,
    system_chars: usize,
    serialized_request_bytes: usize,
    proxy_input_tokens: usize,
    resume_session_present: bool,
}

impl ProviderRequestMetrics {
    fn from_inputs(
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
        resume_session_id: Option<&str>,
    ) -> Self {
        let serialized_request_bytes = serde_json::to_vec(&(messages, tools, system))
            .expect("benchmark request inputs must be serializable")
            .len();
        let mut image_count = 0;
        let mut cache_control_count = 0;
        let mut tool_result_chars = 0;
        for message in messages {
            for block in &message.content {
                match block {
                    ContentBlock::Image { .. } => image_count += 1,
                    ContentBlock::Text {
                        cache_control: Some(_),
                        ..
                    } => cache_control_count += 1,
                    ContentBlock::ToolResult { content, .. } => {
                        tool_result_chars += content.len();
                    }
                    _ => {}
                }
            }
        }
        Self {
            message_count: messages.len(),
            tool_count: tools.len(),
            image_count,
            cache_control_count,
            tool_result_chars,
            system_chars: system.len(),
            serialized_request_bytes,
            proxy_input_tokens: serialized_request_bytes.div_ceil(4),
            resume_session_present: resume_session_id.is_some(),
        }
    }
}

#[derive(Clone, Default)]
struct CountingProvider {
    requests: Arc<Mutex<Vec<ProviderRequestMetrics>>>,
}

#[async_trait]
impl Provider for CountingProvider {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        system: &str,
        resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let metrics =
            ProviderRequestMetrics::from_inputs(messages, tools, system, resume_session_id);
        let input_tokens = metrics.proxy_input_tokens as u64;
        self.requests
            .lock()
            .expect("benchmark provider request lock")
            .push(metrics);
        Ok(Box::pin(stream::iter([
            Ok(StreamEvent::TokenUsage {
                input_tokens: Some(input_tokens),
                output_tokens: Some(1),
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            }),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("benchmark_complete".to_string()),
            }),
        ])))
    }

    fn name(&self) -> &str {
        "controlled-benchmark-provider"
    }

    fn model(&self) -> String {
        "controlled-benchmark-model".to_string()
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

#[derive(Clone, Debug)]
struct ProviderBoundaryObservation {
    message_count: usize,
    old_raw_marker_present: bool,
    current_marker_present: bool,
}

#[derive(Clone, Default)]
struct ProviderBoundaryProbe {
    observations: Arc<Mutex<Vec<ProviderBoundaryObservation>>>,
}

#[async_trait]
impl Provider for ProviderBoundaryProbe {
    async fn complete(
        &self,
        messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let encoded = serde_json::to_string(messages)
            .expect("provider boundary messages must be serializable");
        self.observations
            .lock()
            .expect("provider boundary observation lock")
            .push(ProviderBoundaryObservation {
                message_count: messages.len(),
                old_raw_marker_present: encoded.contains("OLD_RAW_BOUNDARY_MARKER"),
                current_marker_present: encoded.contains("CURRENT_BOUNDARY_MARKER"),
            });
        Ok(Box::pin(stream::iter([
            Ok(StreamEvent::TextDelta("provider-boundary-ok".to_string())),
            Ok(StreamEvent::MessageEnd {
                stop_reason: Some("boundary_complete".to_string()),
            }),
        ])))
    }

    fn name(&self) -> &str {
        "provider-boundary-probe"
    }

    fn model(&self) -> String {
        "provider-boundary-probe-model".to_string()
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

fn add_fixture_turn(agent: &mut Agent, index: usize, include_image: bool) {
    let mut user_content = Vec::new();
    if include_image {
        user_content.push(ContentBlock::Image {
            media_type: "image/png".to_string(),
            data: "controlled-image-fixture".to_string(),
        });
    }
    user_content.push(ContentBlock::Text {
        text: format!("benchmark user request {index}"),
        cache_control: Some(CacheControl::ephemeral(None)),
    });
    agent.add_message(Role::User, user_content);
    agent.add_message(
        Role::Assistant,
        vec![
            ContentBlock::Text {
                text: format!("benchmark assistant tool call {index}"),
                cache_control: Some(CacheControl::ephemeral(None)),
            },
            ContentBlock::ToolUse {
                id: format!("benchmark-tool-{index}"),
                name: "fixture_tool".to_string(),
                input: serde_json::json!({
                    "index": index,
                    "command": "return controlled fixture",
                }),
                thought_signature: None,
            },
        ],
    );
    agent.add_message(
        Role::User,
        vec![ContentBlock::ToolResult {
            tool_use_id: format!("benchmark-tool-{index}"),
            content: format!(
                "tool result {index}: {}",
                "controlled result payload ".repeat(32 + index)
            ),
            is_error: None,
        }],
    );
}

fn fixture_tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "fixture_tool".to_string(),
        description: "A fixed tool definition held constant across both paired requests."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"index": {"type": "integer"}},
            "required": ["index"]
        }),
    }]
}

async fn measure_request(
    provider: &CountingProvider,
    messages: &[Message],
    tools: &[ToolDefinition],
    dynamic_system: &str,
) -> ProviderRequestMetrics {
    let mut events = provider
        .complete_split(
            messages,
            tools,
            SYSTEM_STATIC,
            dynamic_system,
            Some("controlled-benchmark-session"),
        )
        .await
        .expect("controlled provider request must open");
    let mut reported_input_tokens = None;
    while let Some(event) = events.next().await {
        match event.expect("controlled provider stream must be successful") {
            StreamEvent::TokenUsage { input_tokens, .. } => {
                reported_input_tokens = input_tokens;
            }
            StreamEvent::MessageEnd { .. } => {}
            other => panic!("unexpected benchmark provider event: {other:?}"),
        }
    }
    let metrics = provider
        .requests
        .lock()
        .expect("benchmark provider request lock")
        .last()
        .cloned()
        .expect("provider must record each benchmark request");
    assert_eq!(
        reported_input_tokens,
        Some(metrics.proxy_input_tokens as u64),
        "provider-reported proxy must match the recorded aggregate"
    );
    metrics
}

fn summarize(runs: &[ProviderRequestMetrics]) -> serde_json::Value {
    let bytes: Vec<usize> = runs
        .iter()
        .map(|metrics| metrics.serialized_request_bytes)
        .collect();
    let proxy_tokens: Vec<usize> = runs
        .iter()
        .map(|metrics| metrics.proxy_input_tokens)
        .collect();
    let mean = |values: &[usize]| values.iter().sum::<usize>() / values.len();
    serde_json::json!({
        "runs": runs.len(),
        "serialized_request_bytes": {
            "min": bytes.iter().copied().min().unwrap_or(0),
            "max": bytes.iter().copied().max().unwrap_or(0),
            "mean": mean(&bytes),
        },
        "serialized_input_4char_proxy_tokens": {
            "min": proxy_tokens.iter().copied().min().unwrap_or(0),
            "max": proxy_tokens.iter().copied().max().unwrap_or(0),
            "mean": mean(&proxy_tokens),
        },
        "message_count": runs[0].message_count,
        "tool_count": runs[0].tool_count,
        "image_count": runs[0].image_count,
        "cache_control_count": runs[0].cache_control_count,
        "tool_result_chars_mean": mean(&runs.iter().map(|m| m.tool_result_chars).collect::<Vec<_>>()),
        "system_chars": runs[0].system_chars,
        "resume_session_present": runs[0].resume_session_present,
    })
}

#[tokio::test]
async fn controlled_provider_benchmark_reports_paired_proxy_metrics() {
    let _guard = crate::storage::lock_test_env();
    let provider = CountingProvider::default();
    let provider_arc: Arc<dyn Provider> = Arc::new(provider.clone());
    let registry = Registry::new(provider_arc.clone()).await;
    let mut agent = Agent::new(provider_arc, registry);
    for index in 0..6 {
        add_fixture_turn(&mut agent, index, index == 5);
    }

    let baseline_messages = agent.session.messages_for_provider_uncached();
    let outcome = agent.prune_for_model_request(
        ContextPruneSpec::new(ContextPruneKind::ToolResults).keep_recent(1),
    );
    assert!(matches!(outcome, ContextActionOutcome::Completed { .. }));
    let candidate_messages = agent.session.messages_for_provider_uncached();
    let tools = fixture_tools();

    let mut baseline_runs = Vec::with_capacity(BENCHMARK_ITERATIONS);
    let mut candidate_runs = Vec::with_capacity(BENCHMARK_ITERATIONS);
    for _ in 0..BENCHMARK_ITERATIONS {
        baseline_runs
            .push(measure_request(&provider, &baseline_messages, &tools, SYSTEM_DYNAMIC).await);
        candidate_runs
            .push(measure_request(&provider, &candidate_messages, &tools, SYSTEM_DYNAMIC).await);
    }

    for (baseline, candidate) in baseline_runs.iter().zip(&candidate_runs) {
        assert_eq!(baseline.message_count, candidate.message_count);
        assert_eq!(baseline.tool_count, candidate.tool_count);
        assert_eq!(baseline.image_count, candidate.image_count);
        assert_eq!(baseline.cache_control_count, candidate.cache_control_count);
        assert_eq!(baseline.system_chars, candidate.system_chars);
        assert_eq!(
            baseline.resume_session_present,
            candidate.resume_session_present
        );
        assert!(candidate.tool_result_chars < baseline.tool_result_chars);
        assert!(candidate.serialized_request_bytes < baseline.serialized_request_bytes);
    }

    let baseline_summary = summarize(&baseline_runs);
    let candidate_summary = summarize(&candidate_runs);
    let baseline_bytes = baseline_runs[0].serialized_request_bytes;
    let candidate_bytes = candidate_runs[0].serialized_request_bytes;
    let payload = serde_json::json!({
        "benchmark": "context_control_provider_pair",
        "provider": "controlled-benchmark-provider",
        "model": "controlled-benchmark-model",
        "iterations": BENCHMARK_ITERATIONS,
        "conditions": {
            "same_provider": true,
            "same_tools": true,
            "same_static_and_dynamic_system": true,
            "same_cache_control_metadata": true,
            "same_image_count_and_fixture": true,
            "same_resume_session_presence": true,
            "only_candidate_delta": "older tool-result blocks replaced by context-control notes",
        },
        "baseline": baseline_summary,
        "candidate": candidate_summary,
        "paired_delta": {
            "serialized_request_bytes": candidate_bytes as isize - baseline_bytes as isize,
            "serialized_input_4char_proxy_tokens": candidate_runs[0].proxy_input_tokens as isize
                - baseline_runs[0].proxy_input_tokens as isize,
        },
        "interpretation": "Synthetic deterministic provider proxy only; this is not evidence of real provider-token or cost savings.",
    });
    println!(
        "CONTROLLED_PROVIDER_BENCHMARK {}",
        serde_json::to_string(&payload).expect("benchmark aggregate must serialize")
    );
}

fn metric_json(mode: &str, horizon: usize, metrics: &ProviderRequestMetrics) -> serde_json::Value {
    serde_json::json!({
        "mode": mode,
        "horizon": horizon,
        "message_count": metrics.message_count,
        "tool_count": metrics.tool_count,
        "image_count": metrics.image_count,
        "cache_control_count": metrics.cache_control_count,
        "tool_result_chars": metrics.tool_result_chars,
        "system_chars": metrics.system_chars,
        "serialized_request_bytes": metrics.serialized_request_bytes,
        "proxy_input_tokens": metrics.proxy_input_tokens,
        "resume_session_present": metrics.resume_session_present,
    })
}

async fn matched_fixture_messages(horizon: usize) -> Vec<Message> {
    let provider: Arc<dyn Provider> = Arc::new(CountingProvider::default());
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    for index in 0..horizon {
        add_fixture_turn(&mut agent, index, index + 1 == horizon);
    }
    agent.session.messages_for_provider_uncached()
}

#[tokio::test]
async fn matched_context_view_benchmark_reports_three_modes_and_horizons() {
    let _guard = crate::storage::lock_test_env();
    let provider = CountingProvider::default();
    let tools = fixture_tools();
    let horizons = [10usize, 50, 100, 200];
    let mut records = Vec::new();

    for horizon in horizons {
        let messages = matched_fixture_messages(horizon).await;
        let transcript = messages.clone();

        let mut bounded_projector = WorkflowContextProjector::default();
        let bounded = bounded_projector.project(&messages, 20_000, 0, 0).messages;

        let mut state_first_projector = WorkflowContextProjector::default();
        let state_first = state_first_projector
            .project_workflow_context(&messages)
            .messages;

        let transcript_metrics =
            measure_request(&provider, &transcript, &tools, SYSTEM_DYNAMIC).await;
        let bounded_metrics = measure_request(&provider, &bounded, &tools, SYSTEM_DYNAMIC).await;
        let state_first_metrics =
            measure_request(&provider, &state_first, &tools, STATE_FIRST_DYNAMIC).await;

        assert_eq!(
            transcript_metrics.tool_count, bounded_metrics.tool_count,
            "tools must remain paired at horizon {horizon}"
        );
        assert_eq!(
            transcript_metrics.tool_count, state_first_metrics.tool_count,
            "tools must remain paired at horizon {horizon}"
        );
        assert_eq!(
            transcript_metrics.image_count, bounded_metrics.image_count,
            "images must remain paired at horizon {horizon}"
        );
        assert_eq!(
            transcript_metrics.image_count, state_first_metrics.image_count,
            "images must remain paired at horizon {horizon}"
        );
        assert!(
            state_first_metrics.serialized_request_bytes
                < transcript_metrics.serialized_request_bytes,
            "state-first must reduce the request at horizon {horizon}"
        );
        if horizon >= 50 {
            assert!(
                bounded_metrics.serialized_request_bytes
                    < transcript_metrics.serialized_request_bytes,
                "bounded projection must reduce the long request at horizon {horizon}"
            );
        }

        records.push(metric_json("transcript", horizon, &transcript_metrics));
        records.push(metric_json("bounded_projection", horizon, &bounded_metrics));
        records.push(metric_json("state_first", horizon, &state_first_metrics));
    }

    let payload = serde_json::json!({
        "benchmark": "matched_context_view_modes",
        "provider": "controlled-benchmark-provider",
        "model": "controlled-benchmark-model",
        "horizons": horizons,
        "conditions": {
            "same_fixture": true,
            "same_tools": true,
            "same_image_position": true,
            "same_resume_session_presence": true,
            "proxy_only": true,
        },
        "records": records,
        "interpretation": "Synthetic deterministic provider proxy only; this is not evidence of real provider-token or cost savings.",
    });
    println!(
        "MATCHED_CONTEXT_VIEW_BENCHMARK {}",
        serde_json::to_string(&payload).expect("matched benchmark aggregate must serialize")
    );
}

#[tokio::test]
async fn state_first_view_reaches_provider_boundary_without_old_raw_turn() {
    let _guard = crate::storage::lock_test_env();
    let provider = ProviderBoundaryProbe::default();
    let provider_arc: Arc<dyn Provider> = Arc::new(provider.clone());
    let registry = Registry::new(provider_arc.clone()).await;
    let mut agent = Agent::new(provider_arc, registry);
    agent.active_skill = Some("synthetic-workflow".to_string());

    for index in 0..20 {
        let user_text = if index == 0 {
            format!("historical request {index}: OLD_RAW_BOUNDARY_MARKER")
        } else {
            format!("historical request {index}")
        };
        agent.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: user_text,
                cache_control: None,
            }],
        );
        agent.add_message(
            Role::Assistant,
            vec![ContentBlock::Text {
                text: format!("historical answer {index}"),
                cache_control: None,
            }],
        );
    }

    let response = agent
        .run_once_capture("CURRENT_BOUNDARY_MARKER")
        .await
        .expect("provider boundary probe turn must complete");
    assert_eq!(response, "provider-boundary-ok");

    let observations = provider
        .observations
        .lock()
        .expect("provider boundary observation lock")
        .clone();
    assert_eq!(observations.len(), 1);
    for observation in observations {
        assert!(!observation.old_raw_marker_present);
        assert!(observation.current_marker_present);
        assert!(observation.message_count < agent.session.messages.len());
    }
}
