//! Ограниченное состояние долгого procedural skill и validated state patch.
//!
//! `ExecutionState` отделяет текущее структурированное состояние выполнения от
//! полного transcript. Runtime применяет `ExecutionStatePatch` только после
//! проверки schema, state schema и ожидаемой revision. Поля patch с JSON
//! значением `null` очищают соответствующее поле состояния.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;

pub const EXECUTION_STATE_SCHEMA_VERSION: u32 = 1;
pub const EXECUTION_STATE_PATCH_SCHEMA_VERSION: u32 = 1;
pub const MAX_EXECUTION_STATE_SCHEMA_CHARS: usize = 128;
pub const MAX_EXECUTION_STATE_TEXT_CHARS: usize = 512;
pub const MAX_EXECUTION_STATE_LIST_ITEMS: usize = 128;

const EXECUTION_STATE_ALLOWED_ACTIONS: [&str; 5] = [
    "get_state",
    "propose_patch",
    "record_observation",
    "retrieve_evidence",
    "reconcile",
];
const EXECUTION_STATE_TEXT_FIELDS: [&str; 7] = [
    "state_schema",
    "goal",
    "phase",
    "next_action",
    "source_revision",
    "owner",
    "lease",
];
const EXECUTION_STATE_LIST_FIELDS: [&str; 8] = [
    "acceptance_criteria",
    "completed",
    "pending",
    "decisions",
    "changed_files",
    "tests",
    "blockers",
    "evidence_refs",
];
const EXECUTION_STATE_NUMERIC_FIELDS: [&str; 2] = ["schema_version", "revision"];
const EXECUTION_STATE_FIELDS: [&str; 17] = [
    "schema_version",
    "state_schema",
    "revision",
    "goal",
    "acceptance_criteria",
    "phase",
    "completed",
    "pending",
    "decisions",
    "changed_files",
    "tests",
    "blockers",
    "next_action",
    "source_revision",
    "evidence_refs",
    "owner",
    "lease",
];

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

/// Ограничения одного поля в machine-readable contract состояния.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionStateFieldLimit {
    #[serde(default)]
    pub max_chars: Option<usize>,
    #[serde(default)]
    pub max_items: Option<usize>,
    #[serde(default)]
    pub max_item_chars: Option<usize>,
}

impl ExecutionStateFieldLimit {
    fn text(max_chars: usize) -> Self {
        Self {
            max_chars: Some(max_chars),
            max_items: None,
            max_item_chars: None,
        }
    }

    fn list(max_items: usize, max_item_chars: usize) -> Self {
        Self {
            max_chars: None,
            max_items: Some(max_items),
            max_item_chars: Some(max_item_chars),
        }
    }
}

/// Полный machine-readable contract для `ExecutionState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionStateContract {
    #[serde(default = "default_state_schema_version")]
    pub schema_version: u32,
    #[serde(default = "default_state_schema")]
    pub state_schema: String,
    #[serde(default)]
    pub required_fields: Vec<String>,
    #[serde(default = "default_field_limits")]
    pub field_limits: BTreeMap<String, ExecutionStateFieldLimit>,
    #[serde(default)]
    pub observation_sources: Vec<String>,
    #[serde(default = "default_allowed_actions")]
    pub allowed_actions: Vec<String>,
    #[serde(default = "default_state_retention_policy")]
    pub state_retention_policy: String,
    #[serde(default = "default_conflict_policy")]
    pub conflict_policy: String,
}

impl Default for ExecutionStateContract {
    fn default() -> Self {
        Self::new(default_state_schema())
    }
}

impl ExecutionStateContract {
    /// Создаёт contract для указанного state schema с поддерживаемыми runtime
    /// ограничениями и действиями.
    pub fn new(state_schema: impl Into<String>) -> Self {
        Self {
            schema_version: EXECUTION_STATE_SCHEMA_VERSION,
            state_schema: state_schema.into(),
            required_fields: Vec::new(),
            field_limits: default_field_limits(),
            observation_sources: Vec::new(),
            allowed_actions: default_allowed_actions(),
            state_retention_policy: default_state_retention_policy(),
            conflict_policy: default_conflict_policy(),
        }
    }

    /// Проверяет сам contract до его использования как описания состояния.
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
        validate_list_with_limits(
            "required_fields",
            &self.required_fields,
            MAX_EXECUTION_STATE_LIST_ITEMS,
            MAX_EXECUTION_STATE_SCHEMA_CHARS,
        )?;
        for field in &self.required_fields {
            if !EXECUTION_STATE_FIELDS.contains(&field.as_str()) {
                return Err(ExecutionStateError::UnknownContractField {
                    field: field.clone(),
                });
            }
        }

        if self.field_limits.len() > MAX_EXECUTION_STATE_LIST_ITEMS {
            return Err(ExecutionStateError::TooManyItems {
                field: "field_limits",
                max_items: MAX_EXECUTION_STATE_LIST_ITEMS,
                actual_items: self.field_limits.len(),
            });
        }
        for (field, limit) in &self.field_limits {
            validate_text("field_limits", field, MAX_EXECUTION_STATE_SCHEMA_CHARS)?;
            if !EXECUTION_STATE_FIELDS.contains(&field.as_str()) {
                return Err(ExecutionStateError::UnknownContractField {
                    field: field.clone(),
                });
            }
            validate_field_limit(field, limit)?;
        }
        for field in EXECUTION_STATE_FIELDS {
            if !self.field_limits.contains_key(field) {
                return Err(ExecutionStateError::MissingFieldLimit { field });
            }
        }

        validate_list_with_limits(
            "observation_sources",
            &self.observation_sources,
            MAX_EXECUTION_STATE_LIST_ITEMS,
            MAX_EXECUTION_STATE_TEXT_CHARS,
        )?;
        validate_list_with_limits(
            "allowed_actions",
            &self.allowed_actions,
            MAX_EXECUTION_STATE_LIST_ITEMS,
            MAX_EXECUTION_STATE_TEXT_CHARS,
        )?;
        validate_text(
            "state_retention_policy",
            &self.state_retention_policy,
            MAX_EXECUTION_STATE_TEXT_CHARS,
        )?;
        validate_text(
            "conflict_policy",
            &self.conflict_policy,
            MAX_EXECUTION_STATE_TEXT_CHARS,
        )?;
        Ok(())
    }
}

/// Ограниченное состояние текущего procedural skill-run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionState {
    #[serde(default = "default_state_schema_version")]
    pub schema_version: u32,
    #[serde(default = "default_state_schema")]
    pub state_schema: String,
    #[serde(default)]
    pub revision: ExecutionStateRevision,
    #[serde(default)]
    pub required_fields: Vec<String>,
    #[serde(default = "default_field_limits")]
    pub field_limits: BTreeMap<String, ExecutionStateFieldLimit>,
    #[serde(default)]
    pub observation_sources: Vec<String>,
    #[serde(default = "default_allowed_actions")]
    pub allowed_actions: Vec<String>,
    #[serde(default = "default_state_retention_policy")]
    pub state_retention_policy: String,
    #[serde(default = "default_conflict_policy")]
    pub conflict_policy: String,
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
        let contract = ExecutionStateContract::default();
        Self {
            schema_version: contract.schema_version,
            state_schema: contract.state_schema,
            revision: ExecutionStateRevision::INITIAL,
            required_fields: contract.required_fields,
            field_limits: contract.field_limits,
            observation_sources: contract.observation_sources,
            allowed_actions: contract.allowed_actions,
            state_retention_policy: contract.state_retention_policy,
            conflict_policy: contract.conflict_policy,
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

    /// Возвращает полный machine-readable contract текущего state.
    pub fn contract(&self) -> ExecutionStateContract {
        ExecutionStateContract {
            schema_version: self.schema_version,
            state_schema: self.state_schema.clone(),
            required_fields: self.required_fields.clone(),
            field_limits: self.field_limits.clone(),
            observation_sources: self.observation_sources.clone(),
            allowed_actions: self.allowed_actions.clone(),
            state_retention_policy: self.state_retention_policy.clone(),
            conflict_policy: self.conflict_policy.clone(),
        }
    }

    /// Проверяет границы state перед сохранением или передачей модели.
    pub fn validate(&self) -> Result<(), ExecutionStateError> {
        if self.schema_version != EXECUTION_STATE_SCHEMA_VERSION {
            return Err(ExecutionStateError::UnsupportedStateSchemaVersion {
                expected: EXECUTION_STATE_SCHEMA_VERSION,
                actual: self.schema_version,
            });
        }

        let contract = self.contract();
        contract.validate()?;

        validate_text(
            "state_schema",
            &self.state_schema,
            self.text_limit("state_schema", MAX_EXECUTION_STATE_SCHEMA_CHARS),
        )?;
        self.validate_required_fields()?;
        self.validate_optional_text("goal", &self.goal)?;
        self.validate_optional_list("acceptance_criteria", &self.acceptance_criteria)?;
        self.validate_optional_text("phase", &self.phase)?;
        self.validate_optional_list("completed", &self.completed)?;
        self.validate_optional_list("pending", &self.pending)?;
        self.validate_optional_list("decisions", &self.decisions)?;
        self.validate_optional_list("changed_files", &self.changed_files)?;
        self.validate_optional_list("tests", &self.tests)?;
        self.validate_optional_list("blockers", &self.blockers)?;
        self.validate_optional_text("next_action", &self.next_action)?;
        self.validate_optional_text("source_revision", &self.source_revision)?;
        self.validate_optional_list("evidence_refs", &self.evidence_refs)?;
        self.validate_optional_text("owner", &self.owner)?;
        self.validate_optional_text("lease", &self.lease)?;
        Ok(())
    }

    fn text_limit(&self, field: &'static str, fallback: usize) -> usize {
        self.field_limits
            .get(field)
            .and_then(|limit| limit.max_chars)
            .unwrap_or(fallback)
    }

    fn list_limits(&self, field: &'static str) -> (usize, usize) {
        self.field_limits
            .get(field)
            .map(|limit| {
                (
                    limit.max_items.unwrap_or(MAX_EXECUTION_STATE_LIST_ITEMS),
                    limit
                        .max_item_chars
                        .unwrap_or(MAX_EXECUTION_STATE_TEXT_CHARS),
                )
            })
            .unwrap_or((
                MAX_EXECUTION_STATE_LIST_ITEMS,
                MAX_EXECUTION_STATE_TEXT_CHARS,
            ))
    }

    fn validate_optional_text(
        &self,
        field: &'static str,
        value: &Option<String>,
    ) -> Result<(), ExecutionStateError> {
        if let Some(value) = value {
            validate_text(
                field,
                value,
                self.text_limit(field, MAX_EXECUTION_STATE_TEXT_CHARS),
            )?;
        }
        Ok(())
    }

    fn validate_optional_list(
        &self,
        field: &'static str,
        value: &Option<Vec<String>>,
    ) -> Result<(), ExecutionStateError> {
        if let Some(value) = value {
            let (max_items, max_item_chars) = self.list_limits(field);
            validate_list_with_limits(field, value, max_items, max_item_chars)?;
        }
        Ok(())
    }

    fn validate_required_fields(&self) -> Result<(), ExecutionStateError> {
        for field in &self.required_fields {
            let present = match field.as_str() {
                "schema_version" | "revision" => true,
                "state_schema" => !self.state_schema.is_empty(),
                "goal" => self.goal.is_some(),
                "acceptance_criteria" => self.acceptance_criteria.is_some(),
                "phase" => self.phase.is_some(),
                "completed" => self.completed.is_some(),
                "pending" => self.pending.is_some(),
                "decisions" => self.decisions.is_some(),
                "changed_files" => self.changed_files.is_some(),
                "tests" => self.tests.is_some(),
                "blockers" => self.blockers.is_some(),
                "next_action" => self.next_action.is_some(),
                "source_revision" => self.source_revision.is_some(),
                "evidence_refs" => self.evidence_refs.is_some(),
                "owner" => self.owner.is_some(),
                "lease" => self.lease.is_some(),
                _ => false,
            };
            if !present {
                return Err(ExecutionStateError::RequiredFieldMissing {
                    field: field.clone(),
                });
            }
        }
        Ok(())
    }

    /// Возвращает deterministic fingerprint текущего state.
    pub fn fingerprint(&self) -> String {
        let encoded = match serde_json::to_vec(self) {
            Ok(encoded) => encoded,
            Err(error) => {
                crate::logging::warn(&format!(
                    "execution state fingerprint serialization failed: {error}"
                ));
                Vec::new()
            }
        };
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
    UnknownContractField {
        field: String,
    },
    MissingFieldLimit {
        field: &'static str,
    },
    InvalidFieldLimit {
        field: String,
    },
    RequiredFieldMissing {
        field: String,
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
            Self::UnknownContractField { field } => {
                write!(
                    formatter,
                    "execution state contract has unknown field {field}"
                )
            }
            Self::MissingFieldLimit { field } => write!(
                formatter,
                "execution state contract has no limit for field {field}"
            ),
            Self::InvalidFieldLimit { field } => write!(
                formatter,
                "execution state contract has invalid limits for field {field}"
            ),
            Self::RequiredFieldMissing { field } => write!(
                formatter,
                "execution state required field {field} is missing"
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

fn default_field_limits() -> BTreeMap<String, ExecutionStateFieldLimit> {
    let mut limits = BTreeMap::new();
    for field in EXECUTION_STATE_NUMERIC_FIELDS {
        limits.insert(field.to_string(), ExecutionStateFieldLimit::default());
    }
    limits.insert(
        "state_schema".to_string(),
        ExecutionStateFieldLimit::text(MAX_EXECUTION_STATE_SCHEMA_CHARS),
    );
    for field in [
        "goal",
        "phase",
        "next_action",
        "source_revision",
        "owner",
        "lease",
    ] {
        limits.insert(
            field.to_string(),
            ExecutionStateFieldLimit::text(MAX_EXECUTION_STATE_TEXT_CHARS),
        );
    }
    for field in EXECUTION_STATE_LIST_FIELDS {
        limits.insert(
            field.to_string(),
            ExecutionStateFieldLimit::list(
                MAX_EXECUTION_STATE_LIST_ITEMS,
                MAX_EXECUTION_STATE_TEXT_CHARS,
            ),
        );
    }
    limits
}

fn default_allowed_actions() -> Vec<String> {
    EXECUTION_STATE_ALLOWED_ACTIONS
        .iter()
        .map(|action| (*action).to_string())
        .collect()
}

fn default_state_retention_policy() -> String {
    "retain_until_session_end".to_string()
}

fn default_conflict_policy() -> String {
    "reject_stale_revision".to_string()
}

fn validate_field_limit(
    field: &str,
    limit: &ExecutionStateFieldLimit,
) -> Result<(), ExecutionStateError> {
    let valid = if EXECUTION_STATE_NUMERIC_FIELDS.contains(&field) {
        limit.max_chars.is_none() && limit.max_items.is_none() && limit.max_item_chars.is_none()
    } else if field == "state_schema" {
        limit
            .max_chars
            .is_some_and(|value| value > 0 && value <= MAX_EXECUTION_STATE_SCHEMA_CHARS)
            && limit.max_items.is_none()
            && limit.max_item_chars.is_none()
    } else if EXECUTION_STATE_TEXT_FIELDS.contains(&field) {
        limit
            .max_chars
            .is_some_and(|value| value > 0 && value <= MAX_EXECUTION_STATE_TEXT_CHARS)
            && limit.max_items.is_none()
            && limit.max_item_chars.is_none()
    } else if EXECUTION_STATE_LIST_FIELDS.contains(&field) {
        limit.max_chars.is_none()
            && limit
                .max_items
                .is_some_and(|value| value > 0 && value <= MAX_EXECUTION_STATE_LIST_ITEMS)
            && limit
                .max_item_chars
                .is_some_and(|value| value > 0 && value <= MAX_EXECUTION_STATE_TEXT_CHARS)
    } else {
        false
    };

    if valid {
        Ok(())
    } else {
        Err(ExecutionStateError::InvalidFieldLimit {
            field: field.to_string(),
        })
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

fn validate_list_with_limits<T: AsRef<str>>(
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

#[cfg(test)]
#[path = "execution_state_tests.rs"]
mod tests;
