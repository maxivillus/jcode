use super::{JcodeClient, unexpected};
use crate::errors::Result;
use jcode_harness_api::{ApiEvent, ApiRequest, ContextPruneKind, ContextStatusSnapshot};

impl JcodeClient {
    /// Schedule compaction of the transcript so far, freeing context. Not
    /// synchronous: returning means the request was accepted.
    pub fn compact(&self, session_id: &str) -> Result<String> {
        match self
            .request_ok(ApiRequest::Compact {
                session_id: session_id.to_string(),
            })?
            .event
        {
            ApiEvent::Compacted { message, .. } => Ok(message),
            other => Err(unexpected("compacted", &other)),
        }
    }

    /// Queue a user-authorized structural context prune.
    ///
    /// `turns` may keep a requested number of recent turn groups, `tail`
    /// requires `after` and keeps that message plus everything before it, and
    /// `undo` restores the last reversible prune. The bridge validates which
    /// optional arguments are valid for the selected kind.
    pub fn context_prune(
        &self,
        session_id: &str,
        kind: ContextPruneKind,
        keep_recent: Option<usize>,
        after: Option<String>,
    ) -> Result<String> {
        match self
            .request_ok(ApiRequest::ContextPrune {
                session_id: session_id.to_string(),
                kind,
                keep_recent,
                after,
            })?
            .event
        {
            ApiEvent::ContextPruned { message, .. } => Ok(message),
            other => Err(unexpected("context_pruned", &other)),
        }
    }

    /// Reset the provider session and cache baseline without changing the
    /// persisted transcript.
    pub fn reset_provider(&self, session_id: &str) -> Result<String> {
        match self
            .request_ok(ApiRequest::ResetProvider {
                session_id: session_id.to_string(),
            })?
            .event
        {
            ApiEvent::ProviderReset { message, .. } => Ok(message),
            other => Err(unexpected("provider_reset", &other)),
        }
    }

    /// Read aggregate context metadata without returning transcript content.
    pub fn get_context_status(&self, session_id: &str) -> Result<ContextStatusSnapshot> {
        match self
            .request_ok(ApiRequest::GetContextStatus {
                session_id: session_id.to_string(),
            })?
            .event
        {
            ApiEvent::ContextStatus { status, .. } => Ok(status),
            other => Err(unexpected("context_status", &other)),
        }
    }
}
