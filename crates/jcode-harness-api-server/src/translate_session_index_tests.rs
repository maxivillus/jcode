use super::*;
use rusqlite::{Connection, params};

#[test]
fn limited_session_list_reads_compact_index_without_transcript_records() {
    let home = ScopedJcodeHome::new("metadata-index");
    assert!(BridgeState::recent_session_index_entries().is_empty());
    let mut connection = Connection::open(home.path.join("session-metadata-v1.sqlite3")).unwrap();
    let transaction = connection.transaction().unwrap();
    for index in 0..100 {
        transaction
            .execute(
                "INSERT INTO recent_sessions (
                     session_id, working_dir, todo_title, saved, updated_at_ms, last_active_at_ms
                 ) VALUES (?1, '/indexed/project', ?2, ?4, ?3, ?3)",
                params![
                    format!("indexed_{index:03}"),
                    format!("Indexed goal {index}"),
                    index,
                    index == 99,
                ],
            )
            .unwrap();
    }
    transaction.commit().unwrap();

    let event = only_reply_event(
        BridgeState::default()
            .api_request_to_legacy(&json!({"req": "list_sessions", "id": 1, "limit": 100})),
    );
    let ApiEvent::Sessions { sessions } = event else {
        panic!("expected sessions reply, got {event:?}");
    };
    assert_eq!(sessions.len(), 100);
    let newest = sessions
        .iter()
        .find(|session| session.session_id == "indexed_099")
        .expect("indexed newest session");
    assert!(newest.saved);
    assert_eq!(newest.updated_at_ms, Some(99));
    assert_eq!(newest.last_active_at_ms, Some(99));
    assert!(sessions.iter().all(|session| {
        session
            .title
            .as_deref()
            .is_some_and(|title| title.starts_with("Indexed goal "))
    }));
}
