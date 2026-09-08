//! Read-only preflight logic for the next provider request.
//!
//! The controller keeps the decision rule separate from `Session` and the
//! transcript. The turn loop may use its result to select an existing
//! compaction path without making the controller a second transcript owner.

use crate::context::{ContextBudget, ContextComponentHashes, ContextManifest, ContextRevision};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextPreflightAction {
    Send,
    Refresh,
    Compact,
    RefreshThenCompact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextPreflightPlan {
    pub action: ContextPreflightAction,
    pub revision: ContextRevision,
    pub estimated_input_tokens: usize,
    pub max_input_tokens: usize,
}

impl ContextPreflightPlan {
    pub fn needs_refresh(&self) -> bool {
        matches!(
            self.action,
            ContextPreflightAction::Refresh | ContextPreflightAction::RefreshThenCompact
        )
    }

    pub fn needs_compaction(&self) -> bool {
        matches!(
            self.action,
            ContextPreflightAction::Compact | ContextPreflightAction::RefreshThenCompact
        )
    }
}

#[derive(Debug, Clone, Default)]
pub struct ContextController {
    manifest: ContextManifest,
}

impl ContextController {
    pub fn new(manifest: ContextManifest) -> Self {
        Self { manifest }
    }

    pub fn manifest(&self) -> &ContextManifest {
        &self.manifest
    }

    /// Обновляет hashes и provider generation до отправки запроса.
    pub fn update_sources(
        &mut self,
        components: ContextComponentHashes,
        provider_generation: u64,
    ) -> bool {
        self.manifest
            .update_components(components, provider_generation)
    }

    /// Строит план без изменения Session, transcript или provider state.
    pub fn plan(
        &self,
        budget: &ContextBudget,
        current_components: &ContextComponentHashes,
        provider_generation: u64,
    ) -> ContextPreflightPlan {
        let needs_refresh = !self
            .manifest
            .is_fresh(current_components, provider_generation);
        let needs_compaction = !budget.fits();
        let action = match (needs_refresh, needs_compaction) {
            (false, false) => ContextPreflightAction::Send,
            (true, false) => ContextPreflightAction::Refresh,
            (false, true) => ContextPreflightAction::Compact,
            (true, true) => ContextPreflightAction::RefreshThenCompact,
        };

        ContextPreflightPlan {
            action,
            revision: self.manifest.revision,
            estimated_input_tokens: budget.estimated_input_tokens,
            max_input_tokens: budget.max_input_tokens(),
        }
    }

    /// Обновляет revision при изменении источников и запоминает estimate.
    /// Session, transcript и provider state не изменяются.
    pub fn prepare(
        &mut self,
        budget: &ContextBudget,
        current_components: ContextComponentHashes,
        provider_generation: u64,
    ) -> ContextPreflightPlan {
        let mut plan = self.plan(budget, &current_components, provider_generation);
        if plan.needs_refresh() {
            self.update_sources(current_components, provider_generation);
            plan.revision = self.manifest.revision;
        }
        self.manifest.estimated_input_tokens = budget.estimated_input_tokens;
        plan
    }

    /// Записывает фактический usage только для той revision, которая была
    /// отправлена. Поздний результат старой revision отклоняется.
    pub fn record_observed_input_tokens(
        &mut self,
        revision: ContextRevision,
        observed_input_tokens: usize,
    ) -> bool {
        if self.manifest.revision != revision {
            return false;
        }
        self.manifest.observed_input_tokens = Some(observed_input_tokens);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn components(value: &str) -> ContextComponentHashes {
        ContextComponentHashes::from_texts(Some(value), None, None, None, None, None)
    }

    fn budget(estimated_input_tokens: usize) -> ContextBudget {
        ContextBudget {
            provider_context_limit: 100,
            reserved_output_tokens: 20,
            safety_margin_tokens: 10,
            estimated_input_tokens,
        }
    }

    #[test]
    fn fresh_budget_allows_send() {
        let mut controller = ContextController::default();
        controller.update_sources(components("same"), 1);

        let plan = controller.plan(&budget(50), &components("same"), 1);

        assert_eq!(plan.action, ContextPreflightAction::Send);
        assert!(!plan.needs_refresh());
        assert!(!plan.needs_compaction());
    }

    #[test]
    fn stale_sources_require_refresh_before_send() {
        let mut controller = ContextController::default();
        controller.update_sources(components("old"), 1);

        let plan = controller.plan(&budget(50), &components("new"), 1);

        assert_eq!(plan.action, ContextPreflightAction::Refresh);
        assert!(plan.needs_refresh());
        assert!(!plan.needs_compaction());
    }

    #[test]
    fn oversized_request_requires_compaction() {
        let mut controller = ContextController::default();
        controller.update_sources(components("same"), 1);

        let plan = controller.plan(&budget(71), &components("same"), 1);

        assert_eq!(plan.action, ContextPreflightAction::Compact);
        assert!(!plan.needs_refresh());
        assert!(plan.needs_compaction());
    }

    #[test]
    fn stale_oversized_request_requires_both_steps() {
        let controller = ContextController::default();

        let plan = controller.plan(&budget(71), &components("current"), 1);

        assert_eq!(plan.action, ContextPreflightAction::RefreshThenCompact);
        assert!(plan.needs_refresh());
        assert!(plan.needs_compaction());
    }

    #[test]
    fn late_usage_from_old_revision_is_rejected() {
        let mut controller = ContextController::default();
        controller.update_sources(components("first"), 1);
        let old_revision = controller.manifest().revision;
        controller.update_sources(components("second"), 1);

        assert!(!controller.record_observed_input_tokens(old_revision, 10));
        assert!(controller.record_observed_input_tokens(controller.manifest().revision, 20));
        assert_eq!(controller.manifest().observed_input_tokens, Some(20));
    }

    #[test]
    fn prepare_refreshes_revision_and_records_estimate() {
        let mut controller = ContextController::default();
        let plan = controller.prepare(&budget(71), components("current"), 1);

        assert_eq!(plan.action, ContextPreflightAction::RefreshThenCompact);
        assert_eq!(plan.revision, ContextRevision(1));
        assert_eq!(controller.manifest().estimated_input_tokens, 71);
        assert!(controller.manifest().is_fresh(&components("current"), 1));
    }
}
