use super::*;

#[tokio::test]
async fn context_prune_undo_survives_session_reload() {
    let _guard = crate::storage::lock_test_env();
    let mut agent = test_agent().await;
    for index in 0..8 {
        agent.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: format!("turn {index}"),
                cache_control: None,
            }],
        );
    }
    let before_ids = agent
        .session
        .messages
        .iter()
        .map(|message| message.id.clone())
        .collect::<Vec<_>>();

    agent
        .queue_user_prune(ContextPruneSpec::new(ContextPruneKind::Turns).keep_recent(2))
        .expect("a user prune request must be accepted");
    agent.apply_pending_context_actions();

    let persisted = crate::session::Session::load(agent.session_id())
        .expect("prune snapshot should be persisted");
    assert!(persisted.prune_undo_snapshot.is_some());

    let provider = agent.provider.fork();
    let registry = Registry::new(provider.clone()).await;
    let mut restored = Agent::new_with_session(provider, registry, persisted, None);
    restored
        .queue_user_prune_undo()
        .expect("reloaded prune should remain undoable");
    restored.apply_pending_context_actions();

    let after_ids = restored
        .session
        .messages
        .iter()
        .map(|message| message.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(after_ids, before_ids);

    let after_undo = crate::session::Session::load(restored.session_id())
        .expect("restored session should remain loadable");
    assert!(after_undo.prune_undo_snapshot.is_none());
}
