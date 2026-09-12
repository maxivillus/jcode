#[derive(Clone)]
struct StaleResponseProvider {
    request_started: Arc<tokio::sync::Notify>,
    include_tool_call: bool,
}

impl StaleResponseProvider {
    fn with_tool_call(request_started: Arc<tokio::sync::Notify>) -> Self {
        Self {
            request_started,
            include_tool_call: true,
        }
    }
}

#[async_trait]
impl Provider for StaleResponseProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        self.request_started.notify_one();
        tokio::time::sleep(Duration::from_millis(25)).await;

        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(8);
        let _ = tx
            .send(Ok(StreamEvent::SessionId(
                "stale-provider-session".to_string(),
            )))
            .await;
        let _ = tx
            .send(Ok(StreamEvent::TextDelta("stale response".to_string())))
            .await;
        if self.include_tool_call {
            let _ = tx
                .send(Ok(StreamEvent::ToolUseStart {
                    id: "stale-tool-call".to_string(),
                    name: "stale_probe".to_string(),
                }))
                .await;
            let _ = tx
                .send(Ok(StreamEvent::ToolInputDelta(
                    r#"{"value":"stale"}"#.to_string(),
                )))
                .await;
            let _ = tx.send(Ok(StreamEvent::ToolUseEnd)).await;
        }
        let _ = tx
            .send(Ok(StreamEvent::TokenUsage {
                input_tokens: Some(1_234),
                output_tokens: Some(12),
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            }))
            .await;
        let _ = tx
            .send(Ok(StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".to_string()),
            }))
            .await;
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "stale-response"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

struct StaleProbeTool {
    executions: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl crate::tool::Tool for StaleProbeTool {
    fn name(&self) -> &str {
        "stale_probe"
    }

    fn description(&self) -> &str {
        "Тестовый инструмент для проверки, что stale-вызовы не исполняются."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: crate::tool::ToolContext,
    ) -> Result<crate::tool::ToolOutput> {
        self.executions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(crate::tool::ToolOutput::new("stale probe executed"))
    }
}

fn make_context_stale_for_test(
    controller: &Arc<std::sync::Mutex<crate::context_controller::ContextController>>,
) {
    let (components, provider_generation) = {
        let controller = controller.lock().expect("controller lock");
        (
            controller.manifest().components.clone(),
            controller.manifest().provider_generation.saturating_add(1),
        )
    };
    assert!(
        controller
            .lock()
            .expect("controller lock")
            .update_sources(components, provider_generation)
    );
}

#[tokio::test]
async fn stale_provider_response_is_not_persisted_by_blocking_turn() {
    let _guard = crate::storage::lock_test_env();
    let request_started = Arc::new(tokio::sync::Notify::new());
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(StaleResponseProvider::with_tool_call(
        request_started.clone(),
    ));
    let registry = Registry::new(provider.clone()).await;
    registry
        .register(
            "stale_probe".to_string(),
            Arc::new(StaleProbeTool {
                executions: executions.clone(),
            }),
        )
        .await;
    let mut agent = Agent::new(provider, registry);
    let controller = agent.context_controller.clone();

    let turn = agent.run_once_capture("discard this response");
    let invalidate = async move {
        request_started.notified().await;
        make_context_stale_for_test(&controller);
    };
    let (result, ()) = tokio::join!(turn, invalidate);

    assert!(
        result
            .expect("stale turn should finish without an error")
            .is_empty()
    );
    assert!(
        agent
            .session
            .messages
            .iter()
            .all(|message| message.role != Role::Assistant)
    );
    assert!(agent.provider_session_id.is_none());
    assert!(agent.session.provider_session_id.is_none());
    assert!(agent.last_stale_provider_revision().is_some());
    assert_eq!(agent.context_manifest().observed_input_tokens, None);
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn stale_provider_response_is_cleared_from_streaming_client() {
    let _guard = crate::storage::lock_test_env();
    let request_started = Arc::new(tokio::sync::Notify::new());
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(StaleResponseProvider::with_tool_call(
        request_started.clone(),
    ));
    let registry = Registry::new(provider.clone()).await;
    registry
        .register(
            "stale_probe".to_string(),
            Arc::new(StaleProbeTool {
                executions: executions.clone(),
            }),
        )
        .await;
    let mut agent = Agent::new(provider, registry);
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "discard this response".to_string(),
            cache_control: None,
        }],
    );
    let controller = agent.context_controller.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let turn = agent.run_turn_streaming_mpsc(tx);
    let invalidate = async move {
        request_started.notified().await;
        make_context_stale_for_test(&controller);
    };
    let (result, ()) = tokio::join!(turn, invalidate);

    result.expect("stale streaming turn should finish without an error");
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert!(
        events
            .iter()
            .any(|event| { matches!(event, ServerEvent::TextReplace { text } if text.is_empty()) })
    );
    assert!(
        agent
            .session
            .messages
            .iter()
            .all(|message| message.role != Role::Assistant)
    );
    assert!(agent.provider_session_id.is_none());
    assert!(agent.session.provider_session_id.is_none());
    assert!(agent.last_stale_provider_revision().is_some());
    assert_eq!(agent.context_manifest().observed_input_tokens, None);
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}
