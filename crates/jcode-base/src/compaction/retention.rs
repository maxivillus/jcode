use crate::config::ContextRetention;
use crate::message::Message;

pub(super) fn soft_compaction_threshold(manager: &super::CompactionManager) -> f32 {
    match manager.compaction_config.retention {
        ContextRetention::High => 0.90,
        ContextRetention::Mid => super::COMPACTION_THRESHOLD,
        ContextRetention::Low => 0.60,
        ContextRetention::Disabled => 1.0,
    }
}

pub(super) fn soft_recent_turns_to_keep(manager: &super::CompactionManager) -> usize {
    match manager.compaction_config.retention {
        ContextRetention::High => 20,
        ContextRetention::Mid => super::RECENT_TURNS_TO_KEEP,
        ContextRetention::Low => 6,
        ContextRetention::Disabled => super::RECENT_TURNS_TO_KEEP,
    }
}

pub(super) fn soft_compaction_enabled(manager: &super::CompactionManager) -> bool {
    manager.compaction_config.retention != ContextRetention::Disabled
}

pub(super) fn emit_telemetry(manager: &super::CompactionManager, mode: &str) {
    super::compaction_telemetry::event(
        mode,
        manager.compaction_config.retention.as_str(),
        manager.last_compaction.as_ref(),
    );
}

impl super::CompactionManager {
    /// Check if soft compaction should start for the configured retention policy.
    pub fn should_compact_with(&self, all_messages: &[Message]) -> bool {
        use crate::config::CompactionMode;

        if self.suppress_compaction_until_new_message || !soft_compaction_enabled(self) {
            return false;
        }
        let active = self.active_messages(all_messages);
        let keep_turns = soft_recent_turns_to_keep(self);
        let threshold = soft_compaction_threshold(self);
        match self.mode {
            CompactionMode::Reactive => {
                self.pending_task.is_none()
                    && self.context_usage_with(all_messages) >= threshold
                    && active.len() > keep_turns
            }
            CompactionMode::Proactive => {
                active.len() > keep_turns && self.should_compact_proactively(all_messages)
            }
            CompactionMode::Semantic => {
                active.len() > keep_turns && self.should_compact_semantic(all_messages)
            }
        }
    }
}
