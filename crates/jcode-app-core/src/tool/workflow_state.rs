use super::{
    Tool, ToolContext, ToolOutput,
    context_control::{ContextControllerBindings, context_controller_for_session},
};
use crate::context::ContextPlane;
use crate::execution_state::{
    ACTION_STATUS_COMPLETED, ACTION_STATUS_FAILED, ACTION_STATUS_PLANNED,
    MAX_WORKFLOW_ACTION_CHARS, MAX_WORKFLOW_ACTION_RESULT_CHARS,
    MAX_WORKFLOW_OBSERVATION_REVISION_CHARS, MAX_WORKFLOW_OBSERVATION_SOURCE_CHARS,
    MAX_WORKFLOW_OBSERVED_AT_CHARS, MAX_WORKFLOW_SUMMARY_CHARS, OBSERVATION_STATUS_CONTRADICTED,
    OBSERVATION_STATUS_CURRENT, OBSERVATION_STATUS_STALE, PatchValue, WorkflowRunState,
    WorkflowStatePatch,
};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

/// Model surface for bounded, revision-checked workflow run state.
pub(crate) struct WorkflowStateTool {
    bindings: ContextControllerBindings,
}

impl WorkflowStateTool {
    pub(crate) fn new(bindings: ContextControllerBindings) -> Self {
        Self { bindings }
    }
}

#[derive(Debug, Deserialize)]
struct WorkflowStateInput {
    #[serde(default = "default_action")]
    action: String,
    #[serde(default)]
    patch: Option<Value>,
    #[serde(default)]
    observation: Option<ObservationInput>,
    #[serde(default)]
    expected: ReconcileExpectation,
    #[serde(default)]
    round: Option<WorkflowRoundInput>,
}

#[derive(Debug, Deserialize)]
struct WorkflowRoundInput {
    expected_revision: u64,
    action: WorkflowActionInput,
    #[serde(default)]
    state_patch: Option<Value>,
    #[serde(default)]
    summary_patch: Option<SummaryPatchInput>,
}

#[derive(Debug, Deserialize)]
struct WorkflowActionInput {
    name: String,
    status: String,
    #[serde(default)]
    result: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SummaryPatchInput {
    text: String,
    #[serde(default)]
    source_revision: Option<String>,
}

/// Наблюдение из актуального источника, добавляемое в evidence.
#[derive(Debug, Deserialize)]
struct ObservationInput {
    text: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    source_revision: Option<String>,
    #[serde(default)]
    observed_at: Option<String>,
}

/// Ожидаемые значения для сверки без мутации состояния.
#[derive(Debug, Default, Deserialize)]
struct ReconcileExpectation {
    #[serde(default)]
    phase: Option<String>,
    #[serde(default)]
    next_action: Option<String>,
    #[serde(default)]
    source_revision: Option<String>,
}

const MAX_OBSERVATION_CHARS: usize = 500;
const MAX_EVIDENCE_REFS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObservationDisposition {
    Current,
    Duplicate,
    Stale,
    Contradicted,
}

fn trailing_revision_number(value: &str) -> Option<u64> {
    let start = value
        .char_indices()
        .rev()
        .find(|(_, character)| !character.is_ascii_digit())
        .map_or(0, |(index, _)| index + 1);
    value.get(start..)?.parse().ok()
}

fn classify_observation(
    state: &WorkflowRunState,
    source: Option<&str>,
    source_revision: Option<&str>,
    text: &str,
) -> ObservationDisposition {
    let Some(previous_text) = state.last_observation.as_deref() else {
        return ObservationDisposition::Current;
    };

    if state.observation_source.as_deref() != source {
        return ObservationDisposition::Current;
    }

    if state.last_observation_revision.as_deref() == source_revision {
        return if previous_text == text
            && state.observation_status.as_deref() == Some(OBSERVATION_STATUS_CURRENT)
        {
            ObservationDisposition::Duplicate
        } else {
            ObservationDisposition::Contradicted
        };
    }

    match (
        state
            .last_observation_revision
            .as_deref()
            .and_then(trailing_revision_number),
        source_revision.and_then(trailing_revision_number),
    ) {
        (Some(previous), Some(incoming)) if incoming < previous => ObservationDisposition::Stale,
        (Some(previous), Some(incoming)) if incoming == previous => {
            ObservationDisposition::Contradicted
        }
        (Some(_), None) => ObservationDisposition::Stale,
        _ => ObservationDisposition::Current,
    }
}

fn format_observation_evidence(
    source: Option<&str>,
    source_revision: Option<&str>,
    status: &str,
    text: &str,
) -> String {
    if source_revision.is_none() && status == OBSERVATION_STATUS_CURRENT {
        return source.map_or_else(|| text.to_string(), |value| format!("{value}: {text}"));
    }

    let source = source.unwrap_or("observation");
    let revision = source_revision.map_or_else(String::new, |value| format!("#{value}"));
    format!("{source}{revision} [{status}]: {text}")
}

fn default_action() -> String {
    "get_state".to_string()
}

fn nullable_text_schema(description: &str) -> Value {
    json!({
        "type": ["string", "null"],
        "description": description,
    })
}

fn nullable_list_schema(description: &str) -> Value {
    json!({
        "type": ["array", "null"],
        "items": {"type": "string"},
        "description": description,
    })
}

fn patch_schema() -> Value {
    json!({
        "type": "object",
        "description": "Required for propose_patch. JSON null clears a patchable field.",
        "required": ["patch_schema_version", "state_schema", "expected_revision"],
        "properties": {
            "patch_schema_version": {"type": "integer", "minimum": 1},
            "state_schema": {"type": "string"},
            "expected_revision": {"type": "integer", "minimum": 0},
            "goal": nullable_text_schema("New goal, or null to clear it."),
            "acceptance_criteria": nullable_list_schema("Acceptance criteria, or null to clear them."),
            "phase": nullable_text_schema("Current phase, or null to clear it."),
            "completed": nullable_list_schema("Completed items, or null to clear them."),
            "pending": nullable_list_schema("Pending items, or null to clear them."),
            "decisions": nullable_list_schema("Decisions, or null to clear them."),
            "changed_files": nullable_list_schema("Changed file paths, or null to clear them."),
            "tests": nullable_list_schema("Checks and tests, or null to clear them."),
            "blockers": nullable_list_schema("Blockers, or null to clear them."),
            "next_action": nullable_text_schema("Next action, or null to clear it."),
            "source_revision": nullable_text_schema("Source revision, or null to clear it."),
            "context_summary": nullable_text_schema("Bounded workflow summary, or null to clear it."),
            "summary_source_revision": nullable_text_schema("Revision that produced the summary, or null to clear it."),
            "last_observation": nullable_text_schema("Last observation value, or null to clear it."),
            "observation_source": nullable_text_schema("Observation source, or null to clear it."),
            "last_observation_revision": nullable_text_schema("Revision attached to the last observation, or null to clear it."),
            "observation_status": nullable_text_schema("Observation status: current, stale, or contradicted."),
            "observed_at": nullable_text_schema("Observation timestamp, or null to clear it."),
            "last_action": nullable_text_schema("Last declared workflow action, or null to clear it."),
            "last_action_status": nullable_text_schema("Last action status: planned, completed, or failed."),
            "last_action_result": nullable_text_schema("Bounded last action result, or null to clear it."),
            "evidence_refs": nullable_list_schema("Bounded evidence references, or null to clear them."),
            "owner": nullable_text_schema("State owner, or null to clear it."),
            "lease": nullable_text_schema("Lease identifier, or null to clear it."),
        },
        "additionalProperties": false,
    })
}

fn output(title: &str, metadata: Value) -> ToolOutput {
    let body = serde_json::to_string_pretty(&metadata).unwrap_or_else(|_| metadata.to_string());
    ToolOutput::new(body)
        .with_title(title.to_string())
        .with_metadata(metadata)
}

fn round_refusal(state: &WorkflowRunState, reason: impl Into<String>) -> ToolOutput {
    output(
        "workflow_state",
        json!({
            "action": "commit_round",
            "applied": false,
            "mutated": false,
            "refused": true,
            "reason": reason.into(),
            "safe_fallback": "transcript",
            "revision": state.revision,
        }),
    )
}

#[async_trait]
impl Tool for WorkflowStateTool {
    fn name(&self) -> &str {
        "workflow_state"
    }

    fn description(&self) -> &str {
        "Read, patch, observe and reconcile bounded workflow run state."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": [
                        "get_state",
                        "propose_patch",
                        "record_observation",
                        "retrieve_evidence",
                        "reconcile",
                        "commit_round"
                    ],
                    "description": "Read state, patch it, record an observation, list evidence or reconcile expectations.",
                },
                "patch": patch_schema(),
                "observation": {
                    "type": "object",
                    "required": ["text"],
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "Observed fact taken from a current source.",
                        },
                        "source": {
                            "type": "string",
                            "description": "Where the observation came from.",
                        },
                        "source_revision": nullable_text_schema("Opaque source revision used for freshness checks."),
                        "observed_at": nullable_text_schema("Observation timestamp when available."),
                    },
                    "additionalProperties": false,
                },
                "expected": {
                    "type": "object",
                    "properties": {
                        "phase": nullable_text_schema("Expected phase."),
                        "next_action": nullable_text_schema("Expected next action."),
                        "source_revision": nullable_text_schema("Expected source revision."),
                    },
                    "additionalProperties": false,
                },
                "round": {
                    "type": "object",
                    "required": ["expected_revision", "action"],
                    "properties": {
                        "expected_revision": {"type": "integer", "minimum": 0},
                        "action": {
                            "type": "object",
                            "required": ["name", "status"],
                            "properties": {
                                "name": {"type": "string", "description": "Declared workflow action name. The state tool records metadata but does not execute arbitrary actions."},
                                "status": {"type": "string", "enum": ["planned", "completed", "failed"]},
                                "result": nullable_text_schema("Bounded action result or failure reason."),
                            },
                            "additionalProperties": false,
                        },
                        "state_patch": patch_schema(),
                        "summary_patch": {
                            "type": "object",
                            "required": ["text"],
                            "properties": {
                                "text": {"type": "string", "description": "Bounded summary produced in the same provider round."},
                                "source_revision": nullable_text_schema("Source revision covered by this summary."),
                            },
                            "additionalProperties": false,
                        },
                    },
                    "additionalProperties": false,
                },
            },
            "additionalProperties": false,
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: WorkflowStateInput = serde_json::from_value(input)?;
        let controller = context_controller_for_session(&self.bindings, &ctx.session_id)?;
        let mut controller = controller
            .lock()
            .map_err(|_| anyhow::anyhow!("context controller lock poisoned"))?;

        let metadata = match params.action.as_str() {
            "get_state" => {
                let state = controller.workflow_run_state().clone();
                json!({
                    "action": "get_state",
                    "plane": ContextPlane::Execution,
                    "mutated": false,
                    "revision": state.revision,
                    "fingerprint": state.fingerprint(),
                    "contract": state.contract(),
                    "state": state,
                })
            }
            "commit_round" => {
                let Some(round) = params.round else {
                    return Ok(round_refusal(
                        controller.workflow_run_state(),
                        "round is required for commit_round",
                    ));
                };
                let current = controller.workflow_run_state().clone();
                if round.expected_revision != current.revision.0 {
                    return Ok(round_refusal(
                        &current,
                        format!(
                            "round revision {} does not match current revision {}",
                            round.expected_revision, current.revision.0
                        ),
                    ));
                }

                let mut patch = match round.state_patch {
                    Some(value) => match serde_json::from_value::<WorkflowStatePatch>(value) {
                        Ok(patch) => patch,
                        Err(error) => {
                            return Ok(round_refusal(
                                &current,
                                format!("state patch is invalid: {error}"),
                            ));
                        }
                    },
                    None => WorkflowStatePatch::new(current.state_schema.clone(), current.revision),
                };
                if patch.expected_revision != current.revision
                    || patch.state_schema != current.state_schema
                {
                    return Ok(round_refusal(
                        &current,
                        "state patch schema or expected revision does not match the round",
                    ));
                }
                if patch.context_summary.is_some()
                    || patch.summary_source_revision.is_some()
                    || patch.last_action.is_some()
                    || patch.last_action_status.is_some()
                    || patch.last_action_result.is_some()
                {
                    return Ok(round_refusal(
                        &current,
                        "state patch cannot directly set summary or action fields in commit_round",
                    ));
                }

                let action_name = round.action.name.trim();
                if action_name.is_empty() {
                    return Ok(round_refusal(
                        &current,
                        "workflow action name must not be empty",
                    ));
                }
                if action_name.chars().count() > MAX_WORKFLOW_ACTION_CHARS {
                    return Ok(round_refusal(
                        &current,
                        format!(
                            "workflow action name exceeds {MAX_WORKFLOW_ACTION_CHARS} characters"
                        ),
                    ));
                }
                let action_status = round.action.status.trim();
                if !matches!(
                    action_status,
                    ACTION_STATUS_PLANNED | ACTION_STATUS_COMPLETED | ACTION_STATUS_FAILED
                ) {
                    return Ok(round_refusal(
                        &current,
                        format!("workflow action has invalid status {action_status:?}"),
                    ));
                }
                let action_result = round
                    .action
                    .result
                    .as_deref()
                    .map(str::trim)
                    .filter(|result| !result.is_empty());
                if action_result
                    .is_some_and(|result| result.chars().count() > MAX_WORKFLOW_ACTION_RESULT_CHARS)
                {
                    return Ok(round_refusal(
                        &current,
                        format!(
                            "workflow action result exceeds {MAX_WORKFLOW_ACTION_RESULT_CHARS} characters"
                        ),
                    ));
                }

                let summary_applied = round.summary_patch.is_some();
                if let Some(summary) = round.summary_patch {
                    let summary_text = summary.text.trim();
                    if summary_text.is_empty() {
                        return Ok(round_refusal(&current, "summary text must not be empty"));
                    }
                    if summary_text.chars().count() > MAX_WORKFLOW_SUMMARY_CHARS {
                        return Ok(round_refusal(
                            &current,
                            format!("summary text exceeds {MAX_WORKFLOW_SUMMARY_CHARS} characters"),
                        ));
                    }
                    let summary_revision = summary
                        .source_revision
                        .as_deref()
                        .map(str::trim)
                        .filter(|revision| !revision.is_empty())
                        .map(ToOwned::to_owned);
                    let source_revision_after_patch = match &patch.source_revision {
                        Some(PatchValue::Set(revision)) => Some(revision.clone()),
                        Some(PatchValue::Clear) => None,
                        None => current.source_revision.clone(),
                    };
                    if summary_revision.is_none() && source_revision_after_patch.is_some() {
                        return Ok(round_refusal(
                            &current,
                            "summary source_revision is required while workflow source_revision is set",
                        ));
                    }
                    patch.context_summary = Some(PatchValue::Set(summary_text.to_string()));
                    patch.summary_source_revision = Some(match summary_revision {
                        Some(revision) => PatchValue::Set(revision),
                        None => PatchValue::Clear,
                    });
                }
                patch.last_action = Some(PatchValue::Set(action_name.to_string()));
                patch.last_action_status = Some(PatchValue::Set(action_status.to_string()));
                patch.last_action_result = Some(match action_result {
                    Some(result) => PatchValue::Set(result.to_string()),
                    None => PatchValue::Clear,
                });

                let next = match controller.preview_execution_state_patch(&patch) {
                    Ok(next) => next,
                    Err(error) => {
                        return Ok(round_refusal(
                            &current,
                            format!("validated round patch was refused: {error}"),
                        ));
                    }
                };
                let revision = match controller.apply_workflow_state_patch(&patch) {
                    Ok(revision) => revision,
                    Err(error) => {
                        return Ok(round_refusal(
                            &current,
                            format!("round apply failed without state replacement: {error}"),
                        ));
                    }
                };
                debug_assert_eq!(revision, next.revision);
                json!({
                    "action": "commit_round",
                    "plane": ContextPlane::Execution,
                    "applied": true,
                    "mutated": true,
                    "previous_revision": current.revision,
                    "revision": revision,
                    "revision_increment": 1,
                    "workflow_action": {
                        "name": action_name,
                        "status": action_status,
                        "result": action_result,
                    },
                    "summary_applied": summary_applied,
                    "state": controller.workflow_run_state(),
                })
            }
            "propose_patch" => {
                let patch = params
                    .patch
                    .ok_or_else(|| anyhow::anyhow!("patch is required for propose_patch"))?;
                let patch: WorkflowStatePatch = serde_json::from_value(patch)?;
                let previous_revision = controller.workflow_run_state().revision;
                let revision = controller.apply_workflow_state_patch(&patch)?;
                let state = controller.workflow_run_state().clone();
                json!({
                    "action": "propose_patch",
                    "plane": ContextPlane::Execution,
                    "applied": true,
                    "mutated": true,
                    "previous_revision": previous_revision,
                    "revision": revision,
                    "fingerprint": state.fingerprint(),
                    "contract": state.contract(),
                    "state": state,
                })
            }
            "record_observation" => {
                let observation = params.observation.ok_or_else(|| {
                    anyhow::anyhow!("observation is required for record_observation")
                })?;
                let text = observation.text.trim();
                if text.is_empty() {
                    anyhow::bail!("observation text must not be empty");
                }
                if text.chars().count() > MAX_OBSERVATION_CHARS {
                    anyhow::bail!("observation text exceeds {MAX_OBSERVATION_CHARS} characters");
                }
                let source = observation
                    .source
                    .as_deref()
                    .map(str::trim)
                    .filter(|source| !source.is_empty());
                if source.is_some_and(|value| {
                    value.chars().count() > MAX_WORKFLOW_OBSERVATION_SOURCE_CHARS
                }) {
                    anyhow::bail!(
                        "observation source exceeds {MAX_WORKFLOW_OBSERVATION_SOURCE_CHARS} characters"
                    );
                }
                let source_revision = observation
                    .source_revision
                    .as_deref()
                    .map(str::trim)
                    .filter(|revision| !revision.is_empty());
                if source_revision.is_some_and(|value| {
                    value.chars().count() > MAX_WORKFLOW_OBSERVATION_REVISION_CHARS
                }) {
                    anyhow::bail!(
                        "observation source revision exceeds {MAX_WORKFLOW_OBSERVATION_REVISION_CHARS} characters"
                    );
                }
                let observed_at = observation
                    .observed_at
                    .as_deref()
                    .map(str::trim)
                    .filter(|timestamp| !timestamp.is_empty());
                if observed_at
                    .is_some_and(|value| value.chars().count() > MAX_WORKFLOW_OBSERVED_AT_CHARS)
                {
                    anyhow::bail!(
                        "observation timestamp exceeds {MAX_WORKFLOW_OBSERVED_AT_CHARS} characters"
                    );
                }

                let disposition = classify_observation(
                    controller.workflow_run_state(),
                    source,
                    source_revision,
                    text,
                );
                let status = match disposition {
                    ObservationDisposition::Current | ObservationDisposition::Duplicate => {
                        OBSERVATION_STATUS_CURRENT
                    }
                    ObservationDisposition::Stale => OBSERVATION_STATUS_STALE,
                    ObservationDisposition::Contradicted => OBSERVATION_STATUS_CONTRADICTED,
                };
                let entry = format_observation_evidence(source, source_revision, status, text);
                let (schema, revision, mut evidence) = {
                    let state = controller.workflow_run_state();
                    let evidence: Vec<String> =
                        state.evidence_refs.iter().flatten().cloned().collect();
                    (state.state_schema.clone(), state.revision, evidence)
                };

                if disposition == ObservationDisposition::Duplicate {
                    let state = controller.workflow_run_state();
                    return Ok(output(
                        "workflow_state",
                        json!({
                            "action": "record_observation",
                            "plane": ContextPlane::Evidence,
                            "applied": false,
                            "mutated": false,
                            "duplicate": true,
                            "freshness_status": OBSERVATION_STATUS_CURRENT,
                            "revision": state.revision,
                            "evidence_count": state.evidence_refs.as_ref().map_or(0, Vec::len),
                        }),
                    ));
                }

                if evidence.len() >= MAX_EVIDENCE_REFS {
                    anyhow::bail!(
                        "evidence list is full ({MAX_EVIDENCE_REFS}); reconcile it before adding more"
                    );
                }
                evidence.push(entry);
                let mut patch = WorkflowStatePatch::new(schema, revision);
                patch.evidence_refs = Some(PatchValue::Set(evidence));
                if disposition == ObservationDisposition::Current {
                    patch.last_observation = Some(PatchValue::Set(text.to_string()));
                    patch.observation_source = Some(match source {
                        Some(value) => PatchValue::Set(value.to_string()),
                        None => PatchValue::Clear,
                    });
                    patch.last_observation_revision = Some(match source_revision {
                        Some(value) => PatchValue::Set(value.to_string()),
                        None => PatchValue::Clear,
                    });
                    patch.observation_status =
                        Some(PatchValue::Set(OBSERVATION_STATUS_CURRENT.to_string()));
                    patch.context_summary = Some(PatchValue::Clear);
                    patch.summary_source_revision = Some(PatchValue::Clear);
                    patch.observed_at = Some(match observed_at {
                        Some(value) => PatchValue::Set(value.to_string()),
                        None => PatchValue::Clear,
                    });
                    if let Some(source_revision) = source_revision {
                        patch.source_revision = Some(PatchValue::Set(source_revision.to_string()));
                    }
                }
                let revision = controller.apply_workflow_state_patch(&patch)?;
                let state = controller.workflow_run_state();
                json!({
                    "action": "record_observation",
                    "plane": ContextPlane::Evidence,
                    "applied": true,
                    "mutated": true,
                    "freshness_status": status,
                    "duplicate": false,
                    "revision": revision,
                    "evidence_count": state.evidence_refs.as_ref().map_or(0, Vec::len),
                })
            }
            "retrieve_evidence" => {
                let state = controller.workflow_run_state();
                let evidence: Vec<String> = state.evidence_refs.iter().flatten().cloned().collect();
                json!({
                    "action": "retrieve_evidence",
                    "plane": ContextPlane::Evidence,
                    "mutated": false,
                    "revision": state.revision,
                    "source_revision": state.source_revision,
                    "context_summary": state.context_summary,
                    "summary_source_revision": state.summary_source_revision,
                    "last_observation": state.last_observation,
                    "observation_source": state.observation_source,
                    "last_observation_revision": state.last_observation_revision,
                    "observation_status": state.observation_status,
                    "observed_at": state.observed_at,
                    "last_action": state.last_action,
                    "last_action_status": state.last_action_status,
                    "last_action_result": state.last_action_result,
                    "owner": state.owner,
                    "lease": state.lease,
                    "evidence_refs": evidence,
                })
            }
            "reconcile" => {
                let expected = params.expected;
                let state = controller.workflow_run_state();
                let mut differences: Vec<String> = Vec::new();
                if let Some(phase) = expected.phase.as_deref()
                    && state.phase.as_deref() != Some(phase)
                {
                    differences.push(format!(
                        "phase: expected {phase:?}, actual {:?}",
                        state.phase
                    ));
                }
                if let Some(next_action) = expected.next_action.as_deref()
                    && state.next_action.as_deref() != Some(next_action)
                {
                    differences.push(format!(
                        "next_action: expected {next_action:?}, actual {:?}",
                        state.next_action
                    ));
                }
                if let Some(source_revision) = expected.source_revision.as_deref()
                    && state.source_revision.as_deref() != Some(source_revision)
                {
                    differences.push(format!(
                        "source_revision: expected {source_revision:?}, actual {:?}",
                        state.source_revision
                    ));
                }
                json!({
                    "action": "reconcile",
                    "plane": ContextPlane::Execution,
                    "mutated": false,
                    "revision": state.revision,
                    "matches": differences.is_empty(),
                    "differences": differences,
                })
            }
            action => anyhow::bail!("unsupported workflow_state action: {action}"),
        };

        Ok(output("workflow_state", metadata))
    }
}

#[cfg(test)]
#[path = "workflow_state_tests.rs"]
mod tests;
