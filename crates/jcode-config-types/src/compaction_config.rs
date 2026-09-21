use super::{CompactionMode, ContextRetention};
use serde::{Deserialize, Serialize};

/// Compaction configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionConfig {
    /// Compaction mode: reactive (default), proactive, or semantic
    pub mode: CompactionMode,

    /// Context retention strength: low keeps less context, mid is balanced, high keeps more context.
    /// Disabled leaves emergency provider-limit recovery enabled.
    pub retention: ContextRetention,

    /// Optional semantic retention rebuild interval in turns.
    /// `None` uses the mode defaults: low=3, mid=6, high=9.
    /// `Some(0)` disables retention projection; positive values override the mode default.
    pub retention_rebuild_interval: Option<usize>,

    /// [proactive] Number of turns to look ahead when projecting token growth
    pub lookahead_turns: usize,

    /// [proactive] EWMA alpha for token growth smoothing (0.0-1.0, higher = more recency bias)
    pub ewma_alpha: f32,

    /// [proactive/semantic] Minimum context fill level before any proactive check fires (0.0-1.0)
    pub proactive_floor: f32,

    /// [proactive/semantic] Minimum number of token snapshots needed before proactive check
    pub min_samples: usize,

    /// [proactive/semantic] Number of stable turns (no growth) before suppressing proactive compact
    pub stall_window: usize,

    /// [proactive/semantic] Minimum turns between two compactions (cooldown)
    pub min_turns_between_compactions: usize,

    /// [semantic] Cosine similarity threshold below which a topic shift is detected (0.0-1.0)
    pub topic_shift_threshold: f32,

    /// [semantic] Cosine similarity above which a message is kept verbatim (0.0-1.0)
    pub relevance_keep_threshold: f32,

    /// [semantic] Number of recent turns to look at for building the "current goal" embedding
    pub goal_window_turns: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            mode: CompactionMode::Reactive,
            retention: ContextRetention::Mid,
            retention_rebuild_interval: None,
            lookahead_turns: 15,
            ewma_alpha: 0.3,
            proactive_floor: 0.40,
            min_samples: 3,
            stall_window: 5,
            min_turns_between_compactions: 10,
            topic_shift_threshold: 0.45,
            relevance_keep_threshold: 0.65,
            goal_window_turns: 5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CompactionConfig, ContextRetention};

    #[test]
    fn compaction_config_defaults_retention_to_mid() {
        assert_eq!(CompactionConfig::default().retention, ContextRetention::Mid);
        assert_eq!(CompactionConfig::default().retention_rebuild_interval, None);

        let config: CompactionConfig = serde_json::from_str(r#"{"mode":"reactive"}"#)
            .expect("omitted retention should use the default");
        assert_eq!(config.retention, ContextRetention::Mid);

        let uppercase: CompactionConfig = serde_json::from_str(r#"{"retention":"HIGH"}"#)
            .expect("uppercase retention should be accepted");
        assert_eq!(uppercase.retention, ContextRetention::High);

        let configured: CompactionConfig =
            serde_json::from_str(r#"{"retention":"mid","retention_rebuild_interval":4}"#)
                .expect("retention rebuild interval should be configurable");
        assert_eq!(configured.retention_rebuild_interval, Some(4));

        let disabled: CompactionConfig =
            serde_json::from_str(r#"{"retention":"mid","retention_rebuild_interval":0}"#)
                .expect("zero retention rebuild interval should be accepted");
        assert_eq!(disabled.retention_rebuild_interval, Some(0));
    }
}
