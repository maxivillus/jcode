//! Ограниченное состояние долгого procedural skill и validated state patch.
//!
//! `ExecutionState` отделяет текущее структурированное состояние выполнения от
//! полного transcript. Runtime применяет `ExecutionStatePatch` только после
//! проверки schema, state schema и ожидаемой revision. Поля patch с JSON
//! значением `null` очищают соответствующее поле состояния.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;

#[path = "execution_state_error.rs"]
mod execution_state_error;
#[path = "execution_state_patch.rs"]
mod execution_state_patch;

pub use execution_state_error::ExecutionStateError;
pub use execution_state_patch::ExecutionStatePatch;
use execution_state_patch::{validate_list_with_limits, validate_text};

pub const EXECUTION_STATE_SCHEMA_VERSION: u32 = 1;
pub const EXECUTION_STATE_PATCH_SCHEMA_VERSION: u32 = 1;
pub const MAX_EXECUTION_STATE_SCHEMA_CHARS: usize = 128;
pub const MAX_EXECUTION_STATE_TEXT_CHARS: usize = 512;
pub const MAX_EXECUTION_STATE_LIST_ITEMS: usize = 128;
pub const MAX_WORKFLOW_OBSERVATION_SOURCE_CHARS: usize = 128;
pub const MAX_WORKFLOW_OBSERVATION_REVISION_CHARS: usize = 128;
pub const MAX_WORKFLOW_OBSERVATION_STATUS_CHARS: usize = 32;
pub const MAX_WORKFLOW_OBSERVED_AT_CHARS: usize = 64;
pub const MAX_WORKFLOW_SUMMARY_CHARS: usize = MAX_EXECUTION_STATE_TEXT_CHARS;
pub const MAX_WORKFLOW_ACTION_CHARS: usize = 128;
pub const MAX_WORKFLOW_ACTION_RESULT_CHARS: usize = 512;
/// Верхняя граница текста state, который можно добавить в prompt модели.
///
/// Проекция намеренно меньше полного machine-readable state: она сохраняет
/// рабочие поля workflow run, но не переносит в prompt owner, lease и raw evidence
/// references. Лимит держит dynamic-часть предсказуемой и не затрагивает
/// cacheable static prefix.
pub const MAX_EXECUTION_STATE_PROMPT_CHARS: usize = 2048;
const MAX_EXECUTION_STATE_PROMPT_ITEM_CHARS: usize = 192;
const MAX_EXECUTION_STATE_PROMPT_ITEMS: usize = 8;

const EXECUTION_STATE_ALLOWED_ACTIONS: [&str; 6] = [
    "get_state",
    "propose_patch",
    "record_observation",
    "retrieve_evidence",
    "reconcile",
    "commit_round",
];
const EXECUTION_STATE_TEXT_FIELDS: [&str; 17] = [
    "state_schema",
    "goal",
    "phase",
    "next_action",
    "source_revision",
    "context_summary",
    "summary_source_revision",
    "owner",
    "lease",
    "last_observation",
    "observation_source",
    "last_observation_revision",
    "observation_status",
    "observed_at",
    "last_action",
    "last_action_status",
    "last_action_result",
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
const EXECUTION_STATE_FIELDS: [&str; 27] = [
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
    "context_summary",
    "summary_source_revision",
    "last_observation",
    "observation_source",
    "last_observation_revision",
    "observation_status",
    "observed_at",
    "last_action",
    "last_action_status",
    "last_action_result",
    "evidence_refs",
    "owner",
    "lease",
];

pub const OBSERVATION_STATUS_CURRENT: &str = "current";
pub const OBSERVATION_STATUS_STALE: &str = "stale";
pub const OBSERVATION_STATUS_CONTRADICTED: &str = "contradicted";
pub const ACTION_STATUS_PLANNED: &str = "planned";
pub const ACTION_STATUS_COMPLETED: &str = "completed";
pub const ACTION_STATUS_FAILED: &str = "failed";

/// Revision структурированного состояния workflow run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExecutionStateRevision(pub u64);

impl ExecutionStateRevision {
    pub const INITIAL: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

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

/// Ограниченное состояние текущего procedural workflow run.
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
    pub context_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_source_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_observation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_observation_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_action_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_action_result: Option<String>,
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
        self.validate_optional_text("context_summary", &self.context_summary)?;
        self.validate_optional_text("summary_source_revision", &self.summary_source_revision)?;
        self.validate_optional_text("last_observation", &self.last_observation)?;
        self.validate_optional_text("observation_source", &self.observation_source)?;
        self.validate_optional_text("last_observation_revision", &self.last_observation_revision)?;
        self.validate_optional_text("observation_status", &self.observation_status)?;
        self.validate_optional_text("observed_at", &self.observed_at)?;
        self.validate_optional_text("last_action", &self.last_action)?;
        self.validate_optional_text("last_action_status", &self.last_action_status)?;
        self.validate_optional_text("last_action_result", &self.last_action_result)?;
        if let Some(status) = self.observation_status.as_deref()
            && !matches!(
                status,
                OBSERVATION_STATUS_CURRENT
                    | OBSERVATION_STATUS_STALE
                    | OBSERVATION_STATUS_CONTRADICTED
            )
        {
            return Err(ExecutionStateError::InvalidObservationStatus {
                actual: status.to_string(),
            });
        }
        if let Some(status) = self.last_action_status.as_deref()
            && !matches!(
                status,
                ACTION_STATUS_PLANNED | ACTION_STATUS_COMPLETED | ACTION_STATUS_FAILED
            )
        {
            return Err(ExecutionStateError::InvalidActionStatus {
                actual: status.to_string(),
            });
        }
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
                "context_summary" => self.context_summary.is_some(),
                "summary_source_revision" => self.summary_source_revision.is_some(),
                "last_observation" => self.last_observation.is_some(),
                "observation_source" => self.observation_source.is_some(),
                "last_observation_revision" => self.last_observation_revision.is_some(),
                "observation_status" => self.observation_status.is_some(),
                "observed_at" => self.observed_at.is_some(),
                "last_action" => self.last_action.is_some(),
                "last_action_status" => self.last_action_status.is_some(),
                "last_action_result" => self.last_action_result.is_some(),
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

    /// Строит bounded human-readable projection для активного workflow prompt.
    ///
    /// Это не замена полному state или `workflow_state`: machine-readable contract
    /// остаётся доступен через tool. В prompt попадают только operational fields,
    /// которые нужны для продолжения процедуры. `owner`, `lease` и значения
    /// `evidence_refs` не копируются в provider-facing текст.
    pub fn prompt_summary(&self) -> String {
        let mut summary = String::from("# Workflow Run State\n");
        append_prompt_line(
            &mut summary,
            "schema_version",
            &self.schema_version.to_string(),
        );
        append_prompt_line(&mut summary, "state_schema", &self.state_schema);
        append_prompt_line(&mut summary, "revision", &self.revision.0.to_string());
        append_prompt_optional_line(&mut summary, "goal", self.goal.as_deref());
        append_prompt_optional_line(&mut summary, "phase", self.phase.as_deref());
        append_prompt_optional_line(&mut summary, "next_action", self.next_action.as_deref());
        append_prompt_list(
            &mut summary,
            "acceptance_criteria",
            self.acceptance_criteria.as_deref(),
        );
        append_prompt_list(&mut summary, "pending", self.pending.as_deref());
        append_prompt_list(&mut summary, "blockers", self.blockers.as_deref());
        append_prompt_list(&mut summary, "completed", self.completed.as_deref());
        append_prompt_list(&mut summary, "decisions", self.decisions.as_deref());
        append_prompt_list(&mut summary, "changed_files", self.changed_files.as_deref());
        append_prompt_list(&mut summary, "tests", self.tests.as_deref());
        if let Some(count) = self.evidence_refs.as_ref().map(Vec::len) {
            append_prompt_line(&mut summary, "evidence_count", &count.to_string());
        }
        append_prompt_optional_line(
            &mut summary,
            "source_revision",
            self.source_revision.as_deref(),
        );
        append_prompt_optional_line(
            &mut summary,
            "summary_source_revision",
            self.summary_source_revision.as_deref(),
        );
        match (
            self.context_summary.as_deref(),
            self.summary_source_revision.as_deref(),
            self.source_revision.as_deref(),
        ) {
            (Some(value), Some(summary_revision), Some(source_revision))
                if summary_revision == source_revision =>
            {
                append_prompt_line(&mut summary, "context_summary", value);
            }
            (Some(value), None, None) => {
                append_prompt_line(&mut summary, "context_summary", value);
            }
            (Some(_), _, _) => append_prompt_line(
                &mut summary,
                "context_summary",
                "[value omitted because summary revision is stale or unknown]",
            ),
            _ => {}
        }
        append_prompt_optional_line(
            &mut summary,
            "observation_status",
            self.observation_status.as_deref(),
        );
        append_prompt_optional_line(
            &mut summary,
            "observation_source",
            self.observation_source.as_deref(),
        );
        append_prompt_optional_line(
            &mut summary,
            "last_observation_revision",
            self.last_observation_revision.as_deref(),
        );
        append_prompt_optional_line(&mut summary, "observed_at", self.observed_at.as_deref());
        match self.observation_status.as_deref() {
            Some(OBSERVATION_STATUS_STALE) | Some(OBSERVATION_STATUS_CONTRADICTED) => {
                append_prompt_line(
                    &mut summary,
                    "last_observation",
                    "[value omitted because observation is not current]",
                );
            }
            _ => append_prompt_optional_line(
                &mut summary,
                "last_observation",
                self.last_observation.as_deref(),
            ),
        }
        append_prompt_optional_line(&mut summary, "last_action", self.last_action.as_deref());
        append_prompt_optional_line(
            &mut summary,
            "last_action_status",
            self.last_action_status.as_deref(),
        );
        append_prompt_optional_line(
            &mut summary,
            "last_action_result",
            self.last_action_result.as_deref(),
        );
        summary
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

fn append_prompt_optional_line(output: &mut String, label: &str, value: Option<&str>) {
    if let Some(value) = value {
        append_prompt_line(output, label, value);
    }
}

fn append_prompt_list(output: &mut String, label: &str, values: Option<&[String]>) {
    let Some(values) = values else {
        return;
    };

    for (index, value) in values
        .iter()
        .take(MAX_EXECUTION_STATE_PROMPT_ITEMS)
        .enumerate()
    {
        append_prompt_line(output, &format!("{label}[{}]", index + 1), value);
    }
}

fn append_prompt_line(output: &mut String, label: &str, value: &str) {
    let value = collapse_prompt_text(value, MAX_EXECUTION_STATE_PROMPT_ITEM_CHARS);
    if value.is_empty() {
        return;
    }

    let line = format!("{label}: {value}\n");
    let current_chars = output.chars().count();
    let line_chars = line.chars().count();
    if current_chars.saturating_add(line_chars) <= MAX_EXECUTION_STATE_PROMPT_CHARS {
        output.push_str(&line);
    }
}

fn collapse_prompt_text(value: &str, max_chars: usize) -> String {
    let content_limit = max_chars.saturating_sub(1);
    let mut output = String::new();
    let mut truncated = false;

    for word in value.split_whitespace() {
        let separator = usize::from(!output.is_empty());
        let current_chars = output.chars().count();
        let available = content_limit.saturating_sub(current_chars);
        if available <= separator {
            truncated = true;
            break;
        }

        let word_chars = word.chars().count();
        if word_chars <= available - separator {
            if separator == 1 {
                output.push(' ');
            }
            output.push_str(word);
            continue;
        }

        if separator == 1 {
            output.push(' ');
        }
        output.extend(word.chars().take(available - separator));
        truncated = true;
        break;
    }

    if truncated {
        output.push('…');
    }
    output
}

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
        "context_summary",
        "summary_source_revision",
        "last_observation",
        "observation_source",
        "last_observation_revision",
        "observation_status",
        "observed_at",
        "last_action",
        "last_action_status",
        "last_action_result",
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
    limits.insert(
        "observation_source".to_string(),
        ExecutionStateFieldLimit::text(MAX_WORKFLOW_OBSERVATION_SOURCE_CHARS),
    );
    limits.insert(
        "last_observation_revision".to_string(),
        ExecutionStateFieldLimit::text(MAX_WORKFLOW_OBSERVATION_REVISION_CHARS),
    );
    limits.insert(
        "observation_status".to_string(),
        ExecutionStateFieldLimit::text(MAX_WORKFLOW_OBSERVATION_STATUS_CHARS),
    );
    limits.insert(
        "observed_at".to_string(),
        ExecutionStateFieldLimit::text(MAX_WORKFLOW_OBSERVED_AT_CHARS),
    );
    limits.insert(
        "context_summary".to_string(),
        ExecutionStateFieldLimit::text(MAX_WORKFLOW_SUMMARY_CHARS),
    );
    limits.insert(
        "summary_source_revision".to_string(),
        ExecutionStateFieldLimit::text(MAX_WORKFLOW_OBSERVATION_REVISION_CHARS),
    );
    limits.insert(
        "last_action".to_string(),
        ExecutionStateFieldLimit::text(MAX_WORKFLOW_ACTION_CHARS),
    );
    limits.insert(
        "last_action_status".to_string(),
        ExecutionStateFieldLimit::text(MAX_WORKFLOW_OBSERVATION_STATUS_CHARS),
    );
    limits.insert(
        "last_action_result".to_string(),
        ExecutionStateFieldLimit::text(MAX_WORKFLOW_ACTION_RESULT_CHARS),
    );
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

/// Canonical workflow vocabulary for the bounded state carried by one run.
///
/// The `ExecutionState*` names remain available as compatibility aliases for
/// downstream users and older serialized integrations.
pub type WorkflowRunState = ExecutionState;
pub type WorkflowRunRevision = ExecutionStateRevision;
pub type WorkflowRunFieldLimit = ExecutionStateFieldLimit;
pub type WorkflowStateContract = ExecutionStateContract;
pub type WorkflowStatePatch = ExecutionStatePatch;
pub type WorkflowStateError = ExecutionStateError;

#[cfg(test)]
#[path = "execution_state_tests.rs"]
mod tests;
