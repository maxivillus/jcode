//! Внешний registry для runtime-профилей skills.
//!
//! Файлы skills намеренно не участвуют в этом формате и не изменяются.
//! Registry является данными runtime, а не дополнительным источником
//! инструкций. При отсутствии или ошибке registry вызывающий код должен
//! использовать обычный `transcript` flow.

use crate::context::{ExecutionMode, sha256_hex};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_SKILL_RUNTIME_DIR: &str = "skill-runtime";
pub const DEFAULT_REGISTRY_FILE: &str = "registry.json";
pub const SKILL_RUNTIME_REGISTRY_VERSION: u32 = 1;
pub const DEFAULT_MAX_STATE_BYTES: usize = 16 * 1024;
const MAX_STATE_BYTES: usize = 1024 * 1024;

/// Технический профиль skill, расположенный вне файла skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRuntimeProfile {
    /// Стабильное имя skill, совпадающее с именем в SkillRegistry.
    pub skill: String,
    #[serde(default)]
    pub mode: ExecutionMode,
    #[serde(default = "default_max_state_bytes")]
    pub max_state_bytes: usize,
    #[serde(default)]
    pub allowed_observations: Vec<String>,
    #[serde(default)]
    pub allowed_actions: Vec<String>,
}

fn default_max_state_bytes() -> usize {
    DEFAULT_MAX_STATE_BYTES
}

/// Версионированный read-only registry профилей.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRuntimeRegistry {
    #[serde(default = "default_registry_version")]
    pub version: u32,
    #[serde(default)]
    pub profiles: Vec<SkillRuntimeProfile>,
}

fn default_registry_version() -> u32 {
    SKILL_RUNTIME_REGISTRY_VERSION
}

impl Default for SkillRuntimeRegistry {
    fn default() -> Self {
        Self {
            version: SKILL_RUNTIME_REGISTRY_VERSION,
            profiles: Vec::new(),
        }
    }
}

impl SkillRuntimeRegistry {
    /// Возвращает рекомендуемый каталог в домашней папке пользователя.
    pub fn default_dir() -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(DEFAULT_SKILL_RUNTIME_DIR))
    }

    pub fn default_path() -> Option<PathBuf> {
        Self::default_dir().map(|dir| dir.join(DEFAULT_REGISTRY_FILE))
    }

    /// Загружает registry по умолчательному пути.
    ///
    /// Отсутствующий файл означает пустой registry. Ошибка формата не
    /// скрывается, чтобы вызывающий код мог явно выбрать fallback.
    pub fn load_default() -> Result<Self> {
        match Self::default_path() {
            Some(path) if path.is_file() => Self::load_from_path(path),
            _ => Ok(Self::default()),
        }
    }

    /// Загружает `registry.json` из указанного каталога.
    pub fn load_from_dir(dir: impl AsRef<Path>) -> Result<Self> {
        let path = dir.as_ref().join(DEFAULT_REGISTRY_FILE);
        if !path.is_file() {
            return Ok(Self::default());
        }
        Self::load_from_path(path)
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path).with_context(|| {
            format!("failed to read skill runtime registry: {}", path.display())
        })?;
        let registry: Self = serde_json::from_str(&raw).with_context(|| {
            format!("failed to parse skill runtime registry: {}", path.display())
        })?;
        registry.validate()?;
        Ok(registry)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != SKILL_RUNTIME_REGISTRY_VERSION {
            bail!(
                "unsupported skill runtime registry version: {}",
                self.version
            );
        }

        let mut names = BTreeSet::new();
        for profile in &self.profiles {
            let skill = profile.skill.trim();
            if skill.is_empty() {
                bail!("skill runtime profile has an empty skill name");
            }
            if profile.skill.as_str() != skill {
                bail!(
                    "skill runtime profile name must not have leading or trailing whitespace: {skill}"
                );
            }
            if !names.insert(skill.to_string()) {
                bail!("duplicate skill runtime profile: {skill}");
            }
            if profile.max_state_bytes == 0 || profile.max_state_bytes > MAX_STATE_BYTES {
                bail!(
                    "invalid max_state_bytes for skill {skill}: {}",
                    profile.max_state_bytes
                );
            }
            validate_names("observation", skill, &profile.allowed_observations)?;
            validate_names("action", skill, &profile.allowed_actions)?;
        }
        Ok(())
    }

    pub fn profile_for_skill(&self, skill: &str) -> Option<&SkillRuntimeProfile> {
        self.profiles.iter().find(|profile| profile.skill == skill)
    }

    pub fn mode_for_skill(&self, skill: &str) -> ExecutionMode {
        self.profile_for_skill(skill)
            .map(|profile| profile.mode)
            .unwrap_or_default()
    }

    /// Хеш нормализованного registry для ContextRevision.
    pub fn fingerprint(&self) -> String {
        let encoded = serde_json::to_vec(self).unwrap_or_default();
        sha256_hex(encoded)
    }
}

fn validate_names(kind: &str, skill: &str, names: &[String]) -> Result<()> {
    for name in names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            bail!("skill {skill} has an empty allowed {kind}");
        }
        if trimmed != name {
            bail!("skill {skill} has leading or trailing whitespace in allowed {kind}: {name}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn state_profile(skill: &str) -> SkillRuntimeProfile {
        SkillRuntimeProfile {
            skill: skill.to_string(),
            mode: ExecutionMode::State,
            max_state_bytes: DEFAULT_MAX_STATE_BYTES,
            allowed_observations: vec!["git".to_string()],
            allowed_actions: vec!["read".to_string()],
        }
    }

    #[test]
    fn missing_registry_falls_back_to_empty() {
        let dir = tempdir().expect("tempdir");
        let registry = SkillRuntimeRegistry::load_from_dir(dir.path()).expect("load");

        assert!(registry.profiles.is_empty());
        assert_eq!(
            registry.mode_for_skill("build"),
            ExecutionMode::Conversation
        );
    }

    #[test]
    fn registry_loads_external_profile_without_touching_skill_files() {
        let dir = tempdir().expect("tempdir");
        let registry_path = dir.path().join(DEFAULT_REGISTRY_FILE);
        let registry = SkillRuntimeRegistry {
            version: SKILL_RUNTIME_REGISTRY_VERSION,
            profiles: vec![state_profile("build")],
        };
        fs::write(
            &registry_path,
            serde_json::to_vec_pretty(&registry).expect("serialize"),
        )
        .expect("write registry");

        let loaded = SkillRuntimeRegistry::load_from_path(&registry_path).expect("load");
        assert_eq!(loaded.mode_for_skill("build"), ExecutionMode::State);
        assert_eq!(loaded.mode_for_skill("test"), ExecutionMode::Conversation);
        assert_eq!(loaded.fingerprint(), registry.fingerprint());
    }

    #[test]
    fn duplicate_profiles_are_rejected() {
        let registry = SkillRuntimeRegistry {
            version: SKILL_RUNTIME_REGISTRY_VERSION,
            profiles: vec![state_profile("build"), state_profile("build")],
        };

        let error = registry.validate().expect_err("duplicate must fail");
        assert!(error.to_string().contains("duplicate"));
    }

    #[test]
    fn invalid_state_limit_is_rejected() {
        let mut profile = state_profile("build");
        profile.max_state_bytes = MAX_STATE_BYTES + 1;
        let registry = SkillRuntimeRegistry {
            version: SKILL_RUNTIME_REGISTRY_VERSION,
            profiles: vec![profile],
        };

        let error = registry.validate().expect_err("limit must fail");
        assert!(error.to_string().contains("max_state_bytes"));
    }

    #[test]
    fn whitespace_in_profile_name_is_rejected() {
        let registry = SkillRuntimeRegistry {
            version: SKILL_RUNTIME_REGISTRY_VERSION,
            profiles: vec![state_profile(" build ")],
        };

        let error = registry.validate().expect_err("whitespace must fail");
        assert!(error.to_string().contains("whitespace"));
    }

    #[test]
    fn whitespace_in_allowed_name_is_rejected() {
        let mut profile = state_profile("build");
        profile.allowed_actions = vec![" read ".to_string()];
        let registry = SkillRuntimeRegistry {
            version: SKILL_RUNTIME_REGISTRY_VERSION,
            profiles: vec![profile],
        };

        let error = registry.validate().expect_err("whitespace must fail");
        assert!(error.to_string().contains("whitespace"));
    }
}
