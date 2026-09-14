use super::*;

#[tokio::test]
async fn user_context_prune_is_deferred_while_agent_turn_lock_is_busy() {
    let provider: Arc<dyn Provider> = Arc::new(PanicOnForkProvider {
        forked: Arc::new(AtomicBool::new(false)),
    });
    let registry = Registry::new(Arc::clone(&provider)).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel::<ServerEvent>();

    let busy_agent_lock = agent.lock().await;
    let message_count_before = busy_agent_lock.message_count();
    super::queue_prune_request(
        Request::ContextPrune {
            id: 31,
            kind: "turns".to_string(),
            keep_recent: Some(1),
            after_message_id: None,
        },
        &agent,
        &client_event_tx,
    );

    tokio::task::yield_now().await;
    assert!(
        client_event_rx.try_recv().is_err(),
        "a prune request must not be acknowledged while the turn owns the Agent lock"
    );
    assert_eq!(
        busy_agent_lock.message_count(),
        message_count_before,
        "a user prune must not mutate the transcript mid-turn"
    );
    drop(busy_agent_lock);

    let event = tokio::time::timeout(Duration::from_secs(1), client_event_rx.recv())
        .await
        .expect("queued prune should complete after the Agent boundary")
        .expect("the prune result event should be delivered");
    assert!(matches!(
        event,
        ServerEvent::ContextPruneResult {
            id: 31,
            success: true,
            ..
        }
    ));

    assert_eq!(
        agent.lock().await.message_count(),
        message_count_before,
        "acknowledging the request must not apply the prune before a turn boundary"
    );
}
