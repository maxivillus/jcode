use super::{
    ACTION_STATUS_COMPLETED, ACTION_STATUS_FAILED, ACTION_STATUS_PLANNED,
    EXECUTION_STATE_PATCH_SCHEMA_VERSION, ExecutionState, ExecutionStateError,
    ExecutionStateRevision, MAX_EXECUTION_STATE_LIST_ITEMS, MAX_EXECUTION_STATE_SCHEMA_CHARS,
    MAX_EXECUTION_STATE_TEXT_CHARS, OBSERVATION_STATUS_CONTRADICTED, OBSERVATION_STATUS_CURRENT,
    OBSERVATION_STATUS_STALE, OptionalPatch, PatchValue, default_state_schema,
};
use serde::{Deserialize, Deserializer, Serialize};

/// Patch, который модель или другой runtime-клиент может предложить для
/// изменения `ExecutionState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionStatePatch {
    pub patch_schema_version: u32,
    pub state_schema: String,
    pub expected_revision: ExecutionStateRevision,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub goal: OptionalPatch<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub acceptance_criteria: OptionalPatch<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub phase: OptionalPatch<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub completed: OptionalPatch<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub pending: OptionalPatch<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub decisions: OptionalPatch<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub changed_files: OptionalPatch<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub tests: OptionalPatch<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub blockers: OptionalPatch<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub next_action: OptionalPatch<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub source_revision: OptionalPatch<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub context_summary: OptionalPatch<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub summary_source_revision: OptionalPatch<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_observation: OptionalPatch<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub observation_source: OptionalPatch<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_observation_revision: OptionalPatch<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub observation_status: OptionalPatch<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub observed_at: OptionalPatch<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_action: OptionalPatch<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_action_status: OptionalPatch<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_action_result: OptionalPatch<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub evidence_refs: OptionalPatch<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub owner: OptionalPatch<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_patch",
        skip_serializing_if = "Option::is_none"
    )]
    pub lease: OptionalPatch<String>,
}

impl Default for ExecutionStatePatch {
    fn default() -> Self {
        Self {
            patch_schema_version: EXECUTION_STATE_PATCH_SCHEMA_VERSION,
            state_schema: default_state_schema(),
            expected_revision: ExecutionStateRevision::INITIAL,
            goal: None,
            acceptance_criteria: None,
            phase: None,
            completed: None,
            pending: None,
            decisions: None,
            changed_files: None,
            tests: None,
            blockers: None,
            next_action: None,
            source_revision: None,
            context_summary: None,
            summary_source_revision: None,
            last_observation: None,
            observation_source: None,
            last_observation_revision: None,
            observation_status: None,
            observed_at: None,
            last_action: None,
            last_action_status: None,
            last_action_result: None,
            evidence_refs: None,
            owner: None,
            lease: None,
        }
    }
}

impl ExecutionStatePatch {
    pub fn new(state_schema: impl Into<String>, expected_revision: ExecutionStateRevision) -> Self {
        Self {
            state_schema: state_schema.into(),
            expected_revision,
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.goal.is_none()
            && self.acceptance_criteria.is_none()
            && self.phase.is_none()
            && self.completed.is_none()
            && self.pending.is_none()
            && self.decisions.is_none()
            && self.changed_files.is_none()
            && self.tests.is_none()
            && self.blockers.is_none()
            && self.next_action.is_none()
            && self.source_revision.is_none()
            && self.context_summary.is_none()
            && self.summary_source_revision.is_none()
            && self.last_observation.is_none()
            && self.observation_source.is_none()
            && self.last_observation_revision.is_none()
            && self.observation_status.is_none()
            && self.observed_at.is_none()
            && self.last_action.is_none()
            && self.last_action_status.is_none()
            && self.last_action_result.is_none()
            && self.evidence_refs.is_none()
            && self.owner.is_none()
            && self.lease.is_none()
    }

    pub fn validate(&self) -> Result<(), ExecutionStateError> {
        if self.patch_schema_version != EXECUTION_STATE_PATCH_SCHEMA_VERSION {
            return Err(ExecutionStateError::UnsupportedPatchSchemaVersion {
                expected: EXECUTION_STATE_PATCH_SCHEMA_VERSION,
                actual: self.patch_schema_version,
            });
        }

        validate_text(
            "state_schema",
            &self.state_schema,
            MAX_EXECUTION_STATE_SCHEMA_CHARS,
        )?;
        validate_patch_text("goal", &self.goal)?;
        validate_patch_list("acceptance_criteria", &self.acceptance_criteria)?;
        validate_patch_text("phase", &self.phase)?;
        validate_patch_list("completed", &self.completed)?;
        validate_patch_list("pending", &self.pending)?;
        validate_patch_list("decisions", &self.decisions)?;
        validate_patch_list("changed_files", &self.changed_files)?;
        validate_patch_list("tests", &self.tests)?;
        validate_patch_list("blockers", &self.blockers)?;
        validate_patch_text("next_action", &self.next_action)?;
        validate_patch_text("source_revision", &self.source_revision)?;
        validate_patch_text("context_summary", &self.context_summary)?;
        validate_patch_text("summary_source_revision", &self.summary_source_revision)?;
        validate_patch_text("last_observation", &self.last_observation)?;
        validate_patch_text("observation_source", &self.observation_source)?;
        validate_patch_text("last_observation_revision", &self.last_observation_revision)?;
        validate_patch_text("observation_status", &self.observation_status)?;
        validate_patch_text("observed_at", &self.observed_at)?;
        validate_patch_text("last_action", &self.last_action)?;
        validate_patch_text("last_action_status", &self.last_action_status)?;
        validate_patch_text("last_action_result", &self.last_action_result)?;
        if let Some(PatchValue::Set(status)) = &self.observation_status
            && !matches!(
                status.as_str(),
                OBSERVATION_STATUS_CURRENT
                    | OBSERVATION_STATUS_STALE
                    | OBSERVATION_STATUS_CONTRADICTED
            )
        {
            return Err(ExecutionStateError::InvalidObservationStatus {
                actual: status.clone(),
            });
        }
        if let Some(PatchValue::Set(status)) = &self.last_action_status
            && !matches!(
                status.as_str(),
                ACTION_STATUS_PLANNED | ACTION_STATUS_COMPLETED | ACTION_STATUS_FAILED
            )
        {
            return Err(ExecutionStateError::InvalidActionStatus {
                actual: status.clone(),
            });
        }
        validate_patch_list("evidence_refs", &self.evidence_refs)?;
        validate_patch_text("owner", &self.owner)?;
        validate_patch_text("lease", &self.lease)?;

        if self.is_empty() {
            return Err(ExecutionStateError::EmptyPatch);
        }
        Ok(())
    }

    pub(super) fn apply_fields(&self, state: &mut ExecutionState) {
        apply_text_patch(&mut state.goal, &self.goal);
        apply_list_patch(&mut state.acceptance_criteria, &self.acceptance_criteria);
        apply_text_patch(&mut state.phase, &self.phase);
        apply_list_patch(&mut state.completed, &self.completed);
        apply_list_patch(&mut state.pending, &self.pending);
        apply_list_patch(&mut state.decisions, &self.decisions);
        apply_list_patch(&mut state.changed_files, &self.changed_files);
        apply_list_patch(&mut state.tests, &self.tests);
        apply_list_patch(&mut state.blockers, &self.blockers);
        apply_text_patch(&mut state.next_action, &self.next_action);
        apply_text_patch(&mut state.source_revision, &self.source_revision);
        apply_text_patch(&mut state.context_summary, &self.context_summary);
        apply_text_patch(
            &mut state.summary_source_revision,
            &self.summary_source_revision,
        );
        apply_text_patch(&mut state.last_observation, &self.last_observation);
        apply_text_patch(&mut state.observation_source, &self.observation_source);
        apply_text_patch(
            &mut state.last_observation_revision,
            &self.last_observation_revision,
        );
        apply_text_patch(&mut state.observation_status, &self.observation_status);
        apply_text_patch(&mut state.observed_at, &self.observed_at);
        apply_text_patch(&mut state.last_action, &self.last_action);
        apply_text_patch(&mut state.last_action_status, &self.last_action_status);
        apply_text_patch(&mut state.last_action_result, &self.last_action_result);
        apply_list_patch(&mut state.evidence_refs, &self.evidence_refs);
        apply_text_patch(&mut state.owner, &self.owner);
        apply_text_patch(&mut state.lease, &self.lease);
    }
}

fn validate_patch_text(
    field: &'static str,
    value: &OptionalPatch<String>,
) -> Result<(), ExecutionStateError> {
    if let Some(PatchValue::Set(value)) = value {
        validate_text(field, value, MAX_EXECUTION_STATE_TEXT_CHARS)?;
    }
    Ok(())
}

fn deserialize_optional_patch<'de, D, T>(deserializer: D) -> Result<OptionalPatch<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(match Option::<T>::deserialize(deserializer)? {
        Some(value) => PatchValue::Set(value),
        None => PatchValue::Clear,
    }))
}

pub(super) fn validate_text(
    field: &'static str,
    value: &str,
    max_chars: usize,
) -> Result<(), ExecutionStateError> {
    if value.trim().is_empty() {
        return Err(ExecutionStateError::EmptyField { field });
    }
    let actual_chars = value.chars().count();
    if actual_chars > max_chars {
        return Err(ExecutionStateError::FieldTooLong {
            field,
            max_chars,
            actual_chars,
        });
    }
    Ok(())
}

fn validate_patch_list<T: AsRef<str>>(
    field: &'static str,
    value: &OptionalPatch<Vec<T>>,
) -> Result<(), ExecutionStateError> {
    if let Some(PatchValue::Set(value)) = value {
        validate_list(field, value)?;
    }
    Ok(())
}

fn validate_list<T: AsRef<str>>(
    field: &'static str,
    values: &[T],
) -> Result<(), ExecutionStateError> {
    validate_list_with_limits(
        field,
        values,
        MAX_EXECUTION_STATE_LIST_ITEMS,
        MAX_EXECUTION_STATE_TEXT_CHARS,
    )
}

pub(super) fn validate_list_with_limits<T: AsRef<str>>(
    field: &'static str,
    values: &[T],
    max_items: usize,
    max_item_chars: usize,
) -> Result<(), ExecutionStateError> {
    if values.len() > max_items {
        return Err(ExecutionStateError::TooManyItems {
            field,
            max_items,
            actual_items: values.len(),
        });
    }

    for (index, value) in values.iter().enumerate() {
        let value = value.as_ref();
        if value.trim().is_empty() {
            return Err(ExecutionStateError::EmptyField { field });
        }
        let actual_chars = value.chars().count();
        if actual_chars > max_item_chars {
            return Err(ExecutionStateError::ItemTooLong {
                field,
                index,
                max_chars: max_item_chars,
                actual_chars,
            });
        }
    }
    Ok(())
}

fn apply_text_patch(target: &mut Option<String>, patch: &OptionalPatch<String>) {
    match patch {
        None => {}
        Some(PatchValue::Clear) => *target = None,
        Some(PatchValue::Set(value)) => *target = Some(value.clone()),
    }
}

fn apply_list_patch(target: &mut Option<Vec<String>>, patch: &OptionalPatch<Vec<String>>) {
    match patch {
        None => {}
        Some(PatchValue::Clear) => *target = None,
        Some(PatchValue::Set(value)) => *target = Some(value.clone()),
    }
}
