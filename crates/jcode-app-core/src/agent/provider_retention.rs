use crate::config::ContextRetention;

use super::{
    AUTOMATIC_TRIGGER_DENOMINATOR, AUTOMATIC_TRIGGER_NUMERATOR,
    MAX_PROJECTED_TURN_GROUPS_BEFORE_REBUILD, MAX_TURN_GROUPS_BEFORE_PROJECTION,
    RECENT_TURN_GROUPS,
};

#[derive(Debug, Clone, Copy)]
pub(super) struct RetentionSettings {
    pub(super) trigger_numerator: usize,
    pub(super) trigger_denominator: usize,
    pub(super) recent_turn_groups: usize,
    pub(super) max_turn_groups_before_projection: usize,
    pub(super) max_projected_turn_groups_before_rebuild: usize,
    pub(super) workflow_tail_groups: usize,
    pub(super) semantic_rebuild_interval: Option<usize>,
    pub(super) semantic_tail_groups: usize,
}

pub(super) fn retention_settings_with_interval(
    retention: ContextRetention,
    rebuild_interval_override: Option<usize>,
) -> RetentionSettings {
    let rebuild_interval = |default| match rebuild_interval_override {
        Some(0) => None,
        Some(interval) => Some(interval),
        None => Some(default),
    };
    match retention {
        ContextRetention::High => RetentionSettings {
            trigger_numerator: 9,
            trigger_denominator: 10,
            recent_turn_groups: 16,
            max_turn_groups_before_projection: 48,
            max_projected_turn_groups_before_rebuild: 32,
            workflow_tail_groups: 16,
            semantic_rebuild_interval: rebuild_interval(9),
            semantic_tail_groups: 4,
        },
        ContextRetention::Mid => RetentionSettings {
            trigger_numerator: 4,
            trigger_denominator: 5,
            recent_turn_groups: 12,
            max_turn_groups_before_projection: 32,
            max_projected_turn_groups_before_rebuild: 24,
            workflow_tail_groups: 8,
            semantic_rebuild_interval: rebuild_interval(6),
            semantic_tail_groups: 2,
        },
        ContextRetention::Low => RetentionSettings {
            trigger_numerator: AUTOMATIC_TRIGGER_NUMERATOR,
            trigger_denominator: AUTOMATIC_TRIGGER_DENOMINATOR,
            recent_turn_groups: RECENT_TURN_GROUPS,
            max_turn_groups_before_projection: MAX_TURN_GROUPS_BEFORE_PROJECTION,
            max_projected_turn_groups_before_rebuild: MAX_PROJECTED_TURN_GROUPS_BEFORE_REBUILD,
            workflow_tail_groups: 1,
            semantic_rebuild_interval: rebuild_interval(3),
            semantic_tail_groups: 1,
        },
        ContextRetention::Disabled => RetentionSettings {
            trigger_numerator: 1,
            trigger_denominator: 1,
            recent_turn_groups: usize::MAX,
            max_turn_groups_before_projection: usize::MAX,
            max_projected_turn_groups_before_rebuild: usize::MAX,
            workflow_tail_groups: usize::MAX,
            semantic_rebuild_interval: None,
            semantic_tail_groups: 0,
        },
    }
}
