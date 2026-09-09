//! Ограниченное состояние долгого procedural skill и validated state patch.
//!
//! `ExecutionState` отделяет текущее структурированное состояние выполнения от
//! полного transcript. Runtime применяет `ExecutionStatePatch` только после
//! проверки schema, state schema и ожидаемой revision. Поля patch с JSON
//! значением `null` очищают соответствующее поле состояния.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

pub const EXECUTION_STATE_SCHEMA_VERSION: u32 = 1;
pub const EXECUTION_STATE_PATCH_SCHEMA_VERSION: u32 = 1;
pub const MAX_EXECUTION_STATE_SCHEMA_CHARS: usize = 128;
pub const MAX_EXECUTION_STATE_TEXT_CHARS: usize = 512;
pub const MAX_EXECUTION_STATE_LIST_ITEMS: usize = 128;

/// Revision структурированного состояния skill-run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExecutionStateRevision(pub u64);

impl ExecutionStateRevision {
    pub const INITIAL: Self = Self(0);

    fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// Значение одного поля patch: `Clear` удаляет поле, `Set` устанавливает его.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchValue<T> {
    Clear,
    Set(T),
}

impl<T: Serialize> Serialize for PatchValue<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Clear => serializer.serialize_none(),
            Self::Set(value) => value.serialize(serializer),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for PatchValue<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match Option::<T>::deserialize(deserializer)? {
            Some(value) => Self::Set(value),
            None => Self::Clear,
        })
    }
}

/// Patch-семантика поля: `None` не меняет поле, `Some(Clear)` удаляет поле,
/// а `Some(Set(value))` устанавливает новое значение.
pub type OptionalPatch<T> = Option<PatchValue<T>>;

/// Ограниченное состояние текущего procedural skill-run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionState {
    #[serde(default = "default_state_schema_version")]
    pub schema_version: u32,
    #[serde(default = "default_state_schema")]
    pub state_schema: String,
    #[serde(default)]
    pub revision: ExecutionStateRevision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_criteria: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decisions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_files: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blockers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_refs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<String>,
}

impl Default for ExecutionState {
    fn default() -> Self {
        Self {
            schema_version: EXECUTION_STATE_SCHEMA_VERSION,
            state_schema: default_state_schema(),
            revision: ExecutionStateRevision::INITIAL,
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
            evidence_refs: None,
            owner: None,
            lease: None,
        }
    }
}

impl ExecutionState {
    /// Создаёт пустое состояние для указанного machine-readable schema.
    pub fn new(state_schema: impl Into<String>) -> Result<Self, ExecutionStateError> {
        let state = Self {
            state_schema: state_schema.into(),
            ..Self::default()
        };
        state.validate()?;
        Ok(state)
    }

    /// Проверяет границы state перед сохранением или передачей модели.
    pub fn validate(&self) -> Result<(), ExecutionStateError> {
        if self.schema_version != EXECUTION_STATE_SCHEMA_VERSION {
            return Err(ExecutionStateError::UnsupportedStateSchemaVersion {
                expected: EXECUTION_STATE_SCHEMA_VERSION,
                actual: self.schema_version,
            });
        }

        validate_text(
            "state_schema",
            &self.state_schema,
            MAX_EXECUTION_STATE_SCHEMA_CHARS,
        )?;
        validate_optional_text("goal", &self.goal)?;
        validate_optional_list("acceptance_criteria", &self.acceptance_criteria)?;
        validate_optional_text("phase", &self.phase)?;
        validate_optional_list("completed", &self.completed)?;
        validate_optional_list("pending", &self.pending)?;
        validate_optional_list("decisions", &self.decisions)?;
        validate_optional_list("changed_files", &self.changed_files)?;
        validate_optional_list("tests", &self.tests)?;
        validate_optional_list("blockers", &self.blockers)?;
        validate_optional_text("next_action", &self.next_action)?;
        validate_optional_text("source_revision", &self.source_revision)?;
        validate_optional_list("evidence_refs", &self.evidence_refs)?;
        validate_optional_text("owner", &self.owner)?;
        validate_optional_text("lease", &self.lease)?;
        Ok(())
    }

    /// Возвращает deterministic fingerprint текущего state.
    pub fn fingerprint(&self) -> String {
        let encoded = serde_json::to_vec(self).unwrap_or_default();
        crate::context::sha256_hex(encoded)
    }

    /// Строит следующий state без изменения текущего значения.
    pub fn preview_patch(&self, patch: &ExecutionStatePatch) -> Result<Self, ExecutionStateError> {
        self.validate()?;
        patch.validate()?;

        if patch.state_schema != self.state_schema {
            return Err(ExecutionStateError::StateSchemaMismatch {
                expected: self.state_schema.clone(),
                actual: patch.state_schema.clone(),
            });
        }
        if patch.expected_revision != self.revision {
            return Err(ExecutionStateError::RevisionMismatch {
                expected: patch.expected_revision,
                actual: self.revision,
            });
        }

        let revision = self
            .revision
            .next()
            .ok_or(ExecutionStateError::RevisionExhausted)?;
        let mut next = self.clone();
        patch.apply_fields(&mut next);
        next.revision = revision;
        next.validate()?;
        Ok(next)
    }

    /// Применяет patch атомарно: при ошибке state остаётся неизменным.
    pub fn apply_patch(&mut self, patch: &ExecutionStatePatch) -> Result<(), ExecutionStateError> {
        let next = self.preview_patch(patch)?;
        *self = next;
        Ok(())
    }
}

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
        validate_patch_list("evidence_refs", &self.evidence_refs)?;
        validate_patch_text("owner", &self.owner)?;
        validate_patch_text("lease", &self.lease)?;

        if self.is_empty() {
            return Err(ExecutionStateError::EmptyPatch);
        }
        Ok(())
    }

    fn apply_fields(&self, state: &mut ExecutionState) {
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
        apply_list_patch(&mut state.evidence_refs, &self.evidence_refs);
        apply_text_patch(&mut state.owner, &self.owner);
        apply_text_patch(&mut state.lease, &self.lease);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionStateError {
    UnsupportedStateSchemaVersion {
        expected: u32,
        actual: u32,
    },
    UnsupportedPatchSchemaVersion {
        expected: u32,
        actual: u32,
    },
    EmptyField {
        field: &'static str,
    },
    FieldTooLong {
        field: &'static str,
        max_chars: usize,
        actual_chars: usize,
    },
    TooManyItems {
        field: &'static str,
        max_items: usize,
        actual_items: usize,
    },
    ItemTooLong {
        field: &'static str,
        index: usize,
        max_chars: usize,
        actual_chars: usize,
    },
    StateSchemaMismatch {
        expected: String,
        actual: String,
    },
    RevisionMismatch {
        expected: ExecutionStateRevision,
        actual: ExecutionStateRevision,
    },
    EmptyPatch,
    RevisionExhausted,
}

impl fmt::Display for ExecutionStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedStateSchemaVersion { expected, actual } => write!(
                formatter,
                "unsupported execution state schema version {actual}, expected {expected}"
            ),
            Self::UnsupportedPatchSchemaVersion { expected, actual } => write!(
                formatter,
                "unsupported execution state patch schema version {actual}, expected {expected}"
            ),
            Self::EmptyField { field } => {
                write!(formatter, "execution state field {field} is empty")
            }
            Self::FieldTooLong {
                field,
                max_chars,
                actual_chars,
            } => write!(
                formatter,
                "execution state field {field} has {actual_chars} characters, maximum is {max_chars}"
            ),
            Self::TooManyItems {
                field,
                max_items,
                actual_items,
            } => write!(
                formatter,
                "execution state field {field} has {actual_items} items, maximum is {max_items}"
            ),
            Self::ItemTooLong {
                field,
                index,
                max_chars,
                actual_chars,
            } => write!(
                formatter,
                "execution state field {field}[{index}] has {actual_chars} characters, maximum is {max_chars}"
            ),
            Self::StateSchemaMismatch { expected, actual } => write!(
                formatter,
                "execution state schema mismatch: patch has {actual}, state has {expected}"
            ),
            Self::RevisionMismatch { expected, actual } => write!(
                formatter,
                "execution state revision mismatch: patch expects {}, state is {}",
                expected.0, actual.0
            ),
            Self::EmptyPatch => write!(formatter, "execution state patch has no changes"),
            Self::RevisionExhausted => write!(formatter, "execution state revision is exhausted"),
        }
    }
}

impl std::error::Error for ExecutionStateError {}

fn default_state_schema_version() -> u32 {
    EXECUTION_STATE_SCHEMA_VERSION
}

fn default_state_schema() -> String {
    "default".to_string()
}

fn validate_optional_text(
    field: &'static str,
    value: &Option<String>,
) -> Result<(), ExecutionStateError> {
    if let Some(value) = value {
        validate_text(field, value, MAX_EXECUTION_STATE_TEXT_CHARS)?;
    }
    Ok(())
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

fn validate_text(
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

fn validate_optional_list(
    field: &'static str,
    value: &Option<Vec<String>>,
) -> Result<(), ExecutionStateError> {
    if let Some(value) = value {
        validate_list(field, value)?;
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
    if values.len() > MAX_EXECUTION_STATE_LIST_ITEMS {
        return Err(ExecutionStateError::TooManyItems {
            field,
            max_items: MAX_EXECUTION_STATE_LIST_ITEMS,
            actual_items: values.len(),
        });
    }

    for (index, value) in values.iter().enumerate() {
        let value = value.as_ref();
        if value.trim().is_empty() {
            return Err(ExecutionStateError::EmptyField { field });
        }
        let actual_chars = value.chars().count();
        if actual_chars > MAX_EXECUTION_STATE_TEXT_CHARS {
            return Err(ExecutionStateError::ItemTooLong {
                field,
                index,
                max_chars: MAX_EXECUTION_STATE_TEXT_CHARS,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_state_has_default_schema_and_revision() {
        let state = ExecutionState::new("code-review").unwrap();

        assert_eq!(state.schema_version, EXECUTION_STATE_SCHEMA_VERSION);
        assert_eq!(state.state_schema, "code-review");
        assert_eq!(state.revision, ExecutionStateRevision::INITIAL);
        assert!(state.validate().is_ok());
    }

    #[test]
    fn patch_updates_fields_and_advances_revision() {
        let mut state = ExecutionState::new("code-review").unwrap();
        let mut patch = ExecutionStatePatch::new("code-review", state.revision);
        patch.goal = Some(PatchValue::Set("Review the current diff".to_string()));
        patch.pending = Some(PatchValue::Set(vec!["Inspect changed files".to_string()]));
        patch.next_action = Some(PatchValue::Set("Run focused tests".to_string()));

        state.apply_patch(&patch).unwrap();

        assert_eq!(state.revision, ExecutionStateRevision(1));
        assert_eq!(state.goal.as_deref(), Some("Review the current diff"));
        assert_eq!(
            state.pending,
            Some(vec!["Inspect changed files".to_string()])
        );
        assert_eq!(state.next_action.as_deref(), Some("Run focused tests"));
    }

    #[test]
    fn json_null_clears_a_field() {
        let mut state = ExecutionState::new("code-review").unwrap();
        state.goal = Some("Old goal".to_string());
        let patch_json = r#"
        {
            "patch_schema_version": 1,
            "state_schema": "code-review",
            "expected_revision": 0,
            "goal": null
        }
        "#;
        let patch: ExecutionStatePatch = serde_json::from_str(patch_json).unwrap();

        assert_eq!(patch.goal, Some(PatchValue::Clear));

        state.apply_patch(&patch).unwrap();

        assert_eq!(state.goal, None);
        assert_eq!(state.revision, ExecutionStateRevision(1));
    }

    #[test]
    fn patch_serialization_preserves_omitted_and_clear_fields() {
        let mut patch = ExecutionStatePatch::new("code-review", ExecutionStateRevision(3));
        patch.goal = Some(PatchValue::Clear);
        patch.phase = Some(PatchValue::Set("review".to_string()));

        let encoded = serde_json::to_value(&patch).unwrap();
        let object = encoded.as_object().unwrap();
        assert_eq!(object.get("goal"), Some(&serde_json::Value::Null));
        assert_eq!(
            object.get("phase").and_then(|value| value.as_str()),
            Some("review")
        );
        assert!(!object.contains_key("pending"));

        let decoded: ExecutionStatePatch = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.goal, Some(PatchValue::Clear));
        assert_eq!(decoded.phase, Some(PatchValue::Set("review".to_string())));
        assert_eq!(decoded.pending, None);
    }

    #[test]
    fn unknown_patch_fields_are_rejected() {
        let patch_json = r#"
        {
            "patch_schema_version": 1,
            "state_schema": "code-review",
            "expected_revision": 0,
            "goal": "Review",
            "unexpected": true
        }
        "#;

        let error = serde_json::from_str::<ExecutionStatePatch>(patch_json).unwrap_err();

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn stale_patch_is_rejected_without_mutating_state() {
        let mut state = ExecutionState::new("code-review").unwrap();
        state.goal = Some("Current goal".to_string());
        let before = state.clone();
        let mut patch = ExecutionStatePatch::new("code-review", ExecutionStateRevision(4));
        patch.goal = Some(PatchValue::Set("Stale goal".to_string()));

        let error = state.apply_patch(&patch).unwrap_err();

        assert_eq!(
            error,
            ExecutionStateError::RevisionMismatch {
                expected: ExecutionStateRevision(4),
                actual: ExecutionStateRevision::INITIAL,
            }
        );
        assert_eq!(state, before);
    }

    #[test]
    fn schema_mismatch_is_rejected() {
        let mut state = ExecutionState::new("code-review").unwrap();
        let mut patch = ExecutionStatePatch::new("release-plan", state.revision);
        patch.phase = Some(PatchValue::Set("review".to_string()));

        let error = state.apply_patch(&patch).unwrap_err();

        assert_eq!(
            error,
            ExecutionStateError::StateSchemaMismatch {
                expected: "code-review".to_string(),
                actual: "release-plan".to_string(),
            }
        );
        assert_eq!(state.revision, ExecutionStateRevision::INITIAL);
    }

    #[test]
    fn invalid_bounds_are_rejected() {
        let mut state = ExecutionState::new("code-review").unwrap();
        let mut patch = ExecutionStatePatch::new("code-review", state.revision);
        patch.goal = Some(PatchValue::Set(
            "x".repeat(MAX_EXECUTION_STATE_TEXT_CHARS + 1),
        ));

        assert!(matches!(
            state.apply_patch(&patch),
            Err(ExecutionStateError::FieldTooLong { field: "goal", .. })
        ));
    }

    #[test]
    fn empty_patch_is_rejected() {
        let mut state = ExecutionState::new("code-review").unwrap();
        let patch = ExecutionStatePatch::new("code-review", state.revision);

        assert_eq!(
            state.apply_patch(&patch),
            Err(ExecutionStateError::EmptyPatch)
        );
    }

    #[test]
    fn revision_exhaustion_is_rejected() {
        let mut state = ExecutionState::new("code-review").unwrap();
        state.revision = ExecutionStateRevision(u64::MAX);
        let mut patch = ExecutionStatePatch::new("code-review", state.revision);
        patch.phase = Some(PatchValue::Set("done".to_string()));

        assert_eq!(
            state.apply_patch(&patch),
            Err(ExecutionStateError::RevisionExhausted)
        );
    }

    #[test]
    fn fingerprint_is_deterministic_and_changes_with_state() {
        let mut first = ExecutionState::new("code-review").unwrap();
        let second = first.clone();
        assert_eq!(first.fingerprint(), second.fingerprint());

        first.goal = Some("changed".to_string());
        assert_ne!(first.fingerprint(), second.fingerprint());
    }
}
