use super::ExecutionStateRevision;
use std::fmt;

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
    InvalidObservationStatus {
        actual: String,
    },
    InvalidActionStatus {
        actual: String,
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
            Self::InvalidObservationStatus { actual } => write!(
                formatter,
                "workflow observation has invalid status {actual:?}"
            ),
            Self::InvalidActionStatus { actual } => {
                write!(formatter, "workflow action has invalid status {actual:?}")
            }
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
