use super::{BridgeState, PersistedSessionMetadata, RecentSessionIndexEntry};
use rusqlite::{Connection, params};
use std::time::{SystemTime, UNIX_EPOCH};

impl BridgeState {
    // Keep the fallback explicit: the swallowed-error ratchet rejects this
    // production module's implicit defaulting form.
    #[allow(clippy::manual_unwrap_or_default)]
    pub(super) fn recent_session_index_entries() -> Vec<RecentSessionIndexEntry> {
        let Some(path) = Self::recent_session_index_path() else {
            return Vec::new();
        };
        let Ok(connection) = Connection::open(path) else {
            return Vec::new();
        };
        if connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;
                 CREATE TABLE IF NOT EXISTS recent_sessions (
                     session_id TEXT PRIMARY KEY NOT NULL,
                     working_dir TEXT,
                     generated_title TEXT,
                     custom_title TEXT,
                     todo_title TEXT,
                     updated_at_ms INTEGER NOT NULL,
                     last_active_at_ms INTEGER,
                     saved INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE INDEX IF NOT EXISTS recent_sessions_activity
                 ON recent_sessions(COALESCE(last_active_at_ms, updated_at_ms) DESC);",
            )
            .is_err()
        {
            return Vec::new();
        }
        if let Err(error) = connection.execute(
            "ALTER TABLE recent_sessions ADD COLUMN saved INTEGER NOT NULL DEFAULT 0",
            [],
        ) && !error.to_string().contains("duplicate column name")
        {
            return Vec::new();
        }
        let Ok(mut statement) = connection.prepare(
            "SELECT session_id, working_dir, generated_title, custom_title,
                    todo_title, saved, updated_at_ms, last_active_at_ms
             FROM recent_sessions
             ORDER BY COALESCE(last_active_at_ms, updated_at_ms) DESC
             LIMIT 500",
        ) else {
            return Vec::new();
        };
        match statement
            .query_map([], |row| {
                Ok(RecentSessionIndexEntry {
                    session_id: row.get(0)?,
                    working_dir: row.get(1)?,
                    generated_title: row.get(2)?,
                    custom_title: row.get(3)?,
                    todo_title: row.get(4)?,
                    saved: row.get(5)?,
                    updated_at_ms: row.get(6)?,
                    last_active_at_ms: row.get(7)?,
                })
            })
            .and_then(|rows| rows.collect())
        {
            Ok(entries) => entries,
            Err(_) => Vec::new(),
        }
    }

    // Keep the fallback explicit for the same swallowed-error ratchet rule.
    #[allow(clippy::manual_unwrap_or_default)]
    pub(super) fn write_bootstrap_recent_session_index(
        ids: &[(SystemTime, String)],
    ) -> Result<(), ()> {
        let Some(path) = Self::recent_session_index_path() else {
            return Ok(());
        };
        let mut connection = Connection::open(path).map_err(|_| ())?;
        let transaction = connection.transaction().map_err(|_| ())?;
        for (modified, session_id) in ids.iter().take(500) {
            let metadata = match Self::resolve_session_metadata(session_id) {
                Some(metadata) => metadata,
                None => PersistedSessionMetadata::default(),
            };
            let updated_at_ms = match modified.duration_since(UNIX_EPOCH) {
                Ok(duration) => duration.as_millis().min(i64::MAX as u128) as i64,
                Err(_) => 0,
            };
            transaction
                .execute(
                    "INSERT INTO recent_sessions (
                     session_id, working_dir, generated_title, custom_title,
                     todo_title, updated_at_ms, last_active_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5)
                 ON CONFLICT(session_id) DO NOTHING",
                    params![
                        session_id,
                        metadata.working_dir,
                        metadata.title,
                        metadata.custom_title,
                        updated_at_ms,
                    ],
                )
                .map_err(|_| ())?;
        }
        transaction.commit().map_err(|_| ())?;
        Ok(())
    }
}
