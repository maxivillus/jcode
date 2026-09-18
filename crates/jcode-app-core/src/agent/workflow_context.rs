use super::Agent;
use crate::logging;

impl Agent {
    /// Enables the state-first provider view for an active workflow by default.
    /// The transcript view remains an explicit compatibility opt-out.
    pub(super) fn has_active_workflow(&self) -> bool {
        // `active_skill` is retained at the protocol and registry boundary for
        // compatibility. The state-first architecture treats it as a workflow
        // activation signal rather than as the state owner.
        self.active_skill.is_some()
    }

    pub(super) fn workflow_context_mode_requested_from_values(
        context_mode: Option<&str>,
        legacy_state_first: Option<&str>,
    ) -> bool {
        match context_mode {
            Some(value) => value.trim().eq_ignore_ascii_case("state_first"),
            None => match legacy_state_first {
                Some(value) => {
                    let value = value.trim();
                    value == "1"
                        || value.eq_ignore_ascii_case("true")
                        || value.eq_ignore_ascii_case("on")
                }
                None => true,
            },
        }
    }

    pub(super) fn should_use_workflow_context_view(&self) -> bool {
        if !self.has_active_workflow() {
            return false;
        }
        let requested = Self::workflow_context_mode_requested_from_values(
            std::env::var("JCODE_CONTEXT_MODE").ok().as_deref(),
            std::env::var("JCODE_STATE_FIRST").ok().as_deref(),
        );
        if !requested {
            return false;
        }

        let state_valid = self
            .context_controller
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .workflow_run_state()
            .validate()
            .is_ok();
        if !state_valid {
            logging::warn("Workflow context view disabled: workflow run state is invalid");
        }
        state_valid
    }
}
