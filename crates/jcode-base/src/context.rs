//! Базовые типы для контроля provider context.
//!
//! Модуль задаёт общий формат revision, component hashes и budget, который
//! используют agent controller и будущие TUI/SDK read-only interfaces.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const CONTEXT_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Хеш текста или другой сериализованной части context.
pub fn sha256_hex(value: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(value.as_ref()))
}

/// Монотонная ревизия состояния context.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContextRevision(pub u64);

impl ContextRevision {
    pub const INITIAL: Self = Self(0);

    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// Хеши компонентов, от которых зависит provider request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextComponentHashes {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_results: Option<String>,
}

impl ContextComponentHashes {
    /// Создаёт хеши непустых текстовых компонентов.
    pub fn from_texts(
        system_prompt: Option<&str>,
        agents: Option<&str>,
        skills: Option<&str>,
        memory: Option<&str>,
        tools: Option<&str>,
        messages: Option<&str>,
    ) -> Self {
        Self {
            system_prompt: system_prompt.map(sha256_hex),
            agents: agents.map(sha256_hex),
            skills: skills.map(sha256_hex),
            memory: memory.map(sha256_hex),
            tools: tools.map(sha256_hex),
            messages: messages.map(sha256_hex),
            ..Self::default()
        }
    }

    pub fn fingerprint(&self) -> String {
        let encoded = serde_json::to_vec(self).unwrap_or_default();
        sha256_hex(encoded)
    }
}

/// Снимок состояния context перед provider request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextManifest {
    #[serde(default = "default_manifest_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub revision: ContextRevision,
    #[serde(default)]
    pub components: ContextComponentHashes,
    #[serde(default)]
    pub provider_generation: u64,
    #[serde(default)]
    pub estimated_input_tokens: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_input_tokens: Option<usize>,
}

fn default_manifest_schema_version() -> u32 {
    CONTEXT_MANIFEST_SCHEMA_VERSION
}

impl Default for ContextManifest {
    fn default() -> Self {
        Self {
            schema_version: CONTEXT_MANIFEST_SCHEMA_VERSION,
            revision: ContextRevision::INITIAL,
            components: ContextComponentHashes::default(),
            provider_generation: 0,
            estimated_input_tokens: 0,
            observed_input_tokens: None,
        }
    }
}

impl ContextManifest {
    /// Проверяет, соответствует ли manifest текущим компонентам и provider
    /// generation.
    pub fn is_fresh(&self, components: &ContextComponentHashes, provider_generation: u64) -> bool {
        self.schema_version == CONTEXT_MANIFEST_SCHEMA_VERSION
            && self.components == *components
            && self.provider_generation == provider_generation
    }

    /// Обновляет source hashes. Возвращает `true`, если revision изменилась.
    pub fn update_components(
        &mut self,
        components: ContextComponentHashes,
        provider_generation: u64,
    ) -> bool {
        if self.is_fresh(&components, provider_generation) {
            return false;
        }

        self.schema_version = CONTEXT_MANIFEST_SCHEMA_VERSION;
        self.revision = self.revision.next();
        self.components = components;
        self.provider_generation = provider_generation;
        self.observed_input_tokens = None;
        true
    }

    pub fn fingerprint(&self) -> String {
        let encoded = serde_json::to_vec(self).unwrap_or_default();
        sha256_hex(encoded)
    }
}

/// Ограничение provider context с резервом под ответ и safety margin.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBudget {
    pub provider_context_limit: usize,
    pub reserved_output_tokens: usize,
    pub safety_margin_tokens: usize,
    pub estimated_input_tokens: usize,
}

impl ContextBudget {
    pub fn max_input_tokens(&self) -> usize {
        self.provider_context_limit
            .saturating_sub(self.reserved_output_tokens)
            .saturating_sub(self.safety_margin_tokens)
    }

    pub fn remaining_input_tokens(&self) -> usize {
        self.max_input_tokens()
            .saturating_sub(self.estimated_input_tokens)
    }

    pub fn fits(&self) -> bool {
        self.estimated_input_tokens <= self.max_input_tokens()
    }
}

/// Режим построения следующего prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    #[default]
    Conversation,
    State,
    Fallback,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_is_monotonic_and_saturating() {
        assert_eq!(ContextRevision::INITIAL.next(), ContextRevision(1));
        assert_eq!(ContextRevision(u64::MAX).next(), ContextRevision(u64::MAX));
    }

    #[test]
    fn budget_reserves_output_and_margin() {
        let budget = ContextBudget {
            provider_context_limit: 100,
            reserved_output_tokens: 20,
            safety_margin_tokens: 10,
            estimated_input_tokens: 69,
        };

        assert_eq!(budget.max_input_tokens(), 70);
        assert_eq!(budget.remaining_input_tokens(), 1);
        assert!(budget.fits());
    }

    #[test]
    fn budget_saturates_when_reservations_exceed_limit() {
        let budget = ContextBudget {
            provider_context_limit: 10,
            reserved_output_tokens: 8,
            safety_margin_tokens: 8,
            estimated_input_tokens: 1,
        };

        assert_eq!(budget.max_input_tokens(), 0);
        assert!(!budget.fits());
    }

    #[test]
    fn manifest_revision_changes_only_when_sources_change() {
        let mut manifest = ContextManifest::default();
        let first =
            ContextComponentHashes::from_texts(Some("system"), None, None, None, None, None);

        assert!(manifest.update_components(first.clone(), 1));
        assert_eq!(manifest.revision, ContextRevision(1));
        assert!(manifest.is_fresh(&first, 1));
        assert!(!manifest.update_components(first.clone(), 1));
        assert_eq!(manifest.revision, ContextRevision(1));
        assert!(manifest.update_components(first, 2));
        assert_eq!(manifest.revision, ContextRevision(2));
        assert!(manifest.observed_input_tokens.is_none());
    }

    #[test]
    fn component_hashes_are_deterministic_and_sensitive_to_text() {
        let first = ContextComponentHashes::from_texts(Some("same"), None, None, None, None, None);
        let second = ContextComponentHashes::from_texts(Some("same"), None, None, None, None, None);
        let changed =
            ContextComponentHashes::from_texts(Some("changed"), None, None, None, None, None);

        assert_eq!(first, second);
        assert_eq!(first.fingerprint(), second.fingerprint());
        assert_ne!(first.fingerprint(), changed.fingerprint());
        assert_eq!(sha256_hex("hello").len(), 64);
    }

    #[test]
    fn unsupported_manifest_schema_is_not_fresh() {
        let components =
            ContextComponentHashes::from_texts(Some("system"), None, None, None, None, None);
        let mut manifest = ContextManifest {
            schema_version: CONTEXT_MANIFEST_SCHEMA_VERSION + 1,
            components: components.clone(),
            provider_generation: 1,
            ..ContextManifest::default()
        };

        assert!(!manifest.is_fresh(&components, 1));
        assert!(manifest.update_components(components.clone(), 1));
        assert_eq!(manifest.schema_version, CONTEXT_MANIFEST_SCHEMA_VERSION);
        assert!(manifest.is_fresh(&components, 1));
    }
}
