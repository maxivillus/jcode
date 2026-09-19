use super::Agent;
use crate::logging;

fn workflow_context_env_value(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            logging::warn("Workflow context mode environment value is not valid Unicode");
            None
        }
    }
}

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
        let context_mode = workflow_context_env_value("JCODE_CONTEXT_MODE");
        let legacy_state_first = workflow_context_env_value("JCODE_STATE_FIRST");
        let requested = Self::workflow_context_mode_requested_from_values(
            context_mode.as_deref(),
            legacy_state_first.as_deref(),
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
