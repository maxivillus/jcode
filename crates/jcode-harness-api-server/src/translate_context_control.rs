use super::{BridgeState, Outbound, SimpleKind, context_prune_kind_name};
use jcode_harness_api::{
    ApiEvent, ContextPruneKind, ContextStatusSnapshot, ErrorCode, ServerFrame,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
struct ContextPruneRequest {
    kind: ContextPruneKind,
    #[serde(default)]
    keep_recent: Option<usize>,
    #[serde(default)]
    after: Option<String>,
}

fn parse_context_prune_request(request: &Value) -> Result<ContextPruneRequest, String> {
    let mut parsed: ContextPruneRequest = serde_json::from_value(request.clone())
        .map_err(|error| format!("invalid context_prune request: {error}"))?;

    if let Some(after) = parsed.after.as_mut() {
        *after = after.trim().to_string();
        if after.is_empty() {
            return Err("context_prune tail requires a non-empty `after` message id".into());
        }
    }

    match parsed.kind {
        ContextPruneKind::Turns if parsed.after.is_some() => {
            Err("context_prune turns does not accept `after`".into())
        }
        ContextPruneKind::Tail if parsed.keep_recent.is_some() => {
            Err("context_prune tail accepts only `after`".into())
        }
        ContextPruneKind::Tail if parsed.after.is_none() => {
            Err("context_prune tail requires `after`".into())
        }
        ContextPruneKind::Undo if parsed.keep_recent.is_some() || parsed.after.is_some() => {
            Err("context_prune undo does not accept `keep_recent` or `after`".into())
        }
        _ => Ok(parsed),
    }
}

impl BridgeState {
    pub(super) fn context_control_request(
        &mut self,
        req: &str,
        api_id: u64,
        request: &Value,
    ) -> Vec<Outbound> {
        match req {
            "context_prune" => {
                let parsed = match parse_context_prune_request(request) {
                    Ok(parsed) => parsed,
                    Err(message) => {
                        return Self::error_reply(api_id, ErrorCode::InvalidRequest, &message);
                    }
                };
                let id = self.legacy_id();
                self.pending_simple.push((
                    id,
                    api_id,
                    SimpleKind::ContextPrune { kind: parsed.kind },
                ));
                let mut prune = json!({
                    "type": "context_prune",
                    "id": id,
                    "kind": context_prune_kind_name(parsed.kind),
                });
                if let Some(keep_recent) = parsed.keep_recent {
                    prune["keep_recent"] = json!(keep_recent);
                }
                if let Some(after) = parsed.after {
                    prune["after"] = json!(after);
                }
                vec![Outbound::Legacy(prune)]
            }
            "reset_provider" => {
                let id = self.legacy_id();
                self.pending_simple
                    .push((id, api_id, SimpleKind::ProviderReset));
                vec![Outbound::Legacy(
                    json!({"type": "reset_provider", "id": id}),
                )]
            }
            "get_context_status" => {
                let id = self.legacy_id();
                self.pending_simple
                    .push((id, api_id, SimpleKind::ContextStatus));
                vec![Outbound::Legacy(json!({"type": "state", "id": id}))]
            }
            _ => Self::error_reply(
                api_id,
                ErrorCode::UnknownRequest,
                &format!("unknown context-control request: {req}"),
            ),
        }
    }

    pub(super) fn context_control_state_event(
        &mut self,
        event: &Value,
        session_id: &str,
    ) -> Option<Vec<ServerFrame>> {
        let id = event["id"].as_u64().unwrap_or(0);
        let api_id = self.take_simple(id, SimpleKind::ContextStatus)?;
        let context_session_id = if session_id.is_empty() {
            match &self.session_id {
                Some(session_id) => session_id.clone(),
                None => String::new(),
            }
        } else {
            session_id.to_string()
        };
        let Some(status) = event.get("context_status").cloned() else {
            return Some(vec![ServerFrame::reply(
                api_id,
                ApiEvent::Error {
                    code: ErrorCode::Internal,
                    message: "daemon state did not include context_status".into(),
                },
            )]);
        };
        let status = match serde_json::from_value::<ContextStatusSnapshot>(status) {
            Ok(status) => status,
            Err(error) => {
                return Some(vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::Error {
                        code: ErrorCode::Internal,
                        message: format!(
                            "daemon returned an invalid context_status snapshot: {error}"
                        ),
                    },
                )]);
            }
        };
        Some(vec![ServerFrame::reply(
            api_id,
            ApiEvent::ContextStatus {
                session_id: context_session_id,
                status,
            },
        )])
    }

    pub(super) fn context_control_event(
        &mut self,
        event: &Value,
        session_id: &str,
    ) -> Vec<ServerFrame> {
        let id = event["id"].as_u64().unwrap_or(0);
        match event["type"].as_str().unwrap_or("") {
            "context_prune_result" => {
                let Some((api_id, kind)) = self.take_context_prune(id) else {
                    return vec![];
                };
                let message = event["message"].as_str().unwrap_or("").to_string();
                if event["success"].as_bool() == Some(false) {
                    return vec![ServerFrame::reply(
                        api_id,
                        ApiEvent::Error {
                            code: ErrorCode::InvalidRequest,
                            message,
                        },
                    )];
                }
                vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::ContextPruned {
                        session_id: session_id.to_string(),
                        kind,
                        message,
                    },
                )]
            }
            "provider_reset_result" => {
                let Some(api_id) = self.take_simple(id, SimpleKind::ProviderReset) else {
                    return vec![];
                };
                let message = event["message"].as_str().unwrap_or("").to_string();
                if event["success"].as_bool() == Some(false) {
                    return vec![ServerFrame::reply(
                        api_id,
                        ApiEvent::Error {
                            code: ErrorCode::InvalidRequest,
                            message,
                        },
                    )];
                }
                vec![ServerFrame::reply(
                    api_id,
                    ApiEvent::ProviderReset {
                        session_id: session_id.to_string(),
                        message,
                    },
                )]
            }
            _ => vec![],
        }
    }
}
