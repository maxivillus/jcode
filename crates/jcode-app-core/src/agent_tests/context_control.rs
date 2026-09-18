#[test]
fn kv_cache_request_event_carries_provider_context_generation() {
    let ServerEvent::KvCacheRequest {
        cache_generation, ..
    } = kv_cache_request_event(&[], &[], "system", &[], 9)
    else {
        panic!("expected KvCacheRequest event");
    };

    assert_eq!(cache_generation, Some(9));
}

#[tokio::test]
async fn provider_context_generation_changes_when_context_is_invalidated() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    assert_eq!(agent.provider_context_generation(), 0);
    assert!(!agent.invalidate_provider_context("test context invalidation"));
    assert_eq!(agent.provider_context_generation(), 1);
}

#[tokio::test]
async fn automatic_provider_view_changes_generation_only_when_view_changes() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(DelayedProvider {
        open_delay: Duration::ZERO,
        first_event_delay: Duration::ZERO,
    });
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let mut messages = Vec::new();
    for index in 0..30 {
        let question = format!("question {index} {}", "x".repeat(80));
        let answer = format!("answer {index} {}", "y".repeat(80));
        messages.push(Message::user(&question));
        messages.push(Message::assistant_text(&answer));
    }
    for message in &messages {
        agent.add_message(message.role.clone(), message.content.clone());
    }
    let history_before = serde_json::to_string(&agent.session.messages).unwrap();

    let first = agent.project_context(&messages, &Default::default(), &[]);
    assert!(first.len() < messages.len());
    assert_eq!(agent.provider_context_generation(), 0);

    messages.push(Message::user("new question"));
    messages.push(Message::assistant_text("new answer"));
    let append_only_len = agent
        .project_context(&messages, &Default::default(), &[])
        .len();
    assert!(append_only_len > first.len());
    assert_eq!(agent.provider_context_generation(), 0);

    messages[0] = Message::user("changed older question");
    let changed_len = agent
        .project_context(&messages, &Default::default(), &[])
        .len();
    assert!(changed_len > 0);
    assert_eq!(agent.provider_context_generation(), 1);
    assert_eq!(
        serde_json::to_string(&agent.session.messages).unwrap(),
        history_before
    );
}
