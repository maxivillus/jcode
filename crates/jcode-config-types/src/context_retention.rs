use serde::{Deserialize, Serialize};

/// How much historical context automatic projection and soft compaction retain.
///
/// This policy does not disable emergency recovery at the provider limit.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ContextRetention {
    /// Preserve the largest automatic history tail.
    #[serde(alias = "HIGH", alias = "High")]
    High,
    /// Balance historical recall and prompt size (default).
    #[default]
    #[serde(alias = "MID", alias = "Mid", alias = "medium", alias = "MEDIUM")]
    Mid,
    /// Prefer smaller prompts and the existing state-first behavior.
    #[serde(alias = "LOW", alias = "Low")]
    Low,
    /// Disable soft automatic projection and compaction only.
    #[serde(alias = "DISABLED", alias = "Disabled", alias = "OFF", alias = "off")]
    Disabled,
}

impl ContextRetention {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Mid => "mid",
            Self::Low => "low",
            Self::Disabled => "disabled",
        }
    }

    pub fn parse(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "high" => Some(Self::High),
            "mid" | "medium" => Some(Self::Mid),
            "low" => Some(Self::Low),
            "disabled" | "off" => Some(Self::Disabled),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ContextRetention;

    #[test]
    fn context_retention_parses_case_insensitively() {
        assert_eq!(
            ContextRetention::parse("HIGH"),
            Some(ContextRetention::High)
        );
        assert_eq!(
            ContextRetention::parse("medium"),
            Some(ContextRetention::Mid)
        );
        assert_eq!(ContextRetention::parse("low"), Some(ContextRetention::Low));
        assert_eq!(
            ContextRetention::parse("DISABLED"),
            Some(ContextRetention::Disabled)
        );
        assert_eq!(ContextRetention::parse("unknown"), None);
    }
}
