use super::*;

struct IsolatedSessionPersistenceEnv {
    previous_home: Option<std::ffi::OsString>,
    previous_runtime: Option<std::ffi::OsString>,
    _home: tempfile::TempDir,
    _runtime: tempfile::TempDir,
}

impl IsolatedSessionPersistenceEnv {
    fn new() -> Self {
        let home = tempfile::TempDir::new().expect("jcode home");
        let runtime = tempfile::TempDir::new().expect("runtime dir");
        let previous_home = std::env::var_os("JCODE_HOME");
        let previous_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
        crate::env::set_var("JCODE_HOME", home.path());
        crate::env::set_var("JCODE_RUNTIME_DIR", runtime.path());
        Self {
            previous_home,
            previous_runtime,
            _home: home,
            _runtime: runtime,
        }
    }
}

impl Drop for IsolatedSessionPersistenceEnv {
    fn drop(&mut self) {
        if let Some(previous_home) = self.previous_home.take() {
            crate::env::set_var("JCODE_HOME", previous_home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
        if let Some(previous_runtime) = self.previous_runtime.take() {
            crate::env::set_var("JCODE_RUNTIME_DIR", previous_runtime);
        } else {
            crate::env::remove_var("JCODE_RUNTIME_DIR");
        }
    }
}

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

#[tokio::test]
async fn context_prune_undo_survives_real_process_restart() {
    const CHILD_MARKER: &str = "JCODE_CONTEXT_PRUNE_RESTART_CHILD";
    const SESSION_ID: &str = "JCODE_CONTEXT_PRUNE_RESTART_SESSION_ID";

    if std::env::var_os(CHILD_MARKER).is_some() {
        let _guard = crate::storage::lock_test_env();
        let session_id = std::env::var(SESSION_ID).expect("child session id");
        let persisted = crate::session::Session::load(&session_id).expect("load after restart");
        let before_ids = persisted
            .prune_undo_snapshot
            .as_ref()
            .expect("prune undo snapshot after restart")
            .messages
            .iter()
            .map(|message| message.id.clone())
            .collect::<Vec<_>>();
        assert!(
            persisted.messages.len() < before_ids.len(),
            "restart child must see the pruned transcript"
        );

        let provider = std::sync::Arc::new(StubProvider);
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
        assert!(
            crate::session::Session::load(&session_id)
                .expect("load after child undo")
                .prune_undo_snapshot
                .is_none(),
            "child undo must be durable"
        );
        return;
    }

    let _guard = crate::storage::lock_test_env();
    let _env = IsolatedSessionPersistenceEnv::new();
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

    let session_id = agent.session_id().to_string();
    let persisted = crate::session::Session::load(&session_id).expect("prune should be durable");
    assert!(persisted.messages.len() < before_ids.len());
    assert!(persisted.prune_undo_snapshot.is_some());

    let status = tokio::process::Command::new(
        std::env::current_exe().expect("resolve current test executable"),
    )
    .arg("context_prune_undo_survives_real_process_restart")
    .arg("--nocapture")
    .env(CHILD_MARKER, "1")
    .env(SESSION_ID, &session_id)
    .status()
    .await
    .expect("spawn restart child");
    assert!(
        status.success(),
        "restart child must load, undo, and persist the session"
    );

    let restored = crate::session::Session::load(&session_id).expect("parent readback");
    let restored_ids = restored
        .messages
        .iter()
        .map(|message| message.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(restored_ids, before_ids);
    assert!(restored.prune_undo_snapshot.is_none());
}
