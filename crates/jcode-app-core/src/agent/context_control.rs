use super::Agent;
use crate::context::{ContextBudget, ContextComponentHashes, ContextRevision, sha256_hex};
use crate::context_controller::ContextPreflightPlan;
use crate::message::{ContentBlock, Message, ToolDefinition};
use crate::prompt::SplitSystemPrompt;
use crate::skill_runtime::SkillRuntimeRegistry;
use serde::Serialize;

const RESERVED_OUTPUT_TOKENS: usize = 4096;
const SAFETY_MARGIN_TOKENS: usize = 512;
const IMAGE_TOKEN_ESTIMATE: usize = 256;
const OPAQUE_NATIVE_ITEM_TOKEN_ESTIMATE: usize = 64;

fn serialized_fingerprint<T: Serialize + ?Sized>(value: &T) -> String {
    match serde_json::to_vec(value) {
        Ok(encoded) => sha256_hex(encoded),
        Err(_) => sha256_hex("serialization-error"),
    }
}

fn serialized_token_estimate<T: Serialize + ?Sized>(value: &T) -> usize {
    match serde_json::to_string(value) {
        Ok(encoded) => crate::util::estimate_tokens(&encoded),
        Err(_) => usize::MAX,
    }
}

fn message_token_estimate(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|message| {
            let block_tokens = message
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text, .. } | ContentBlock::Reasoning { text } => {
                        crate::util::estimate_tokens(text)
                    }
                    ContentBlock::ReasoningTrace { .. } => 0,
                    ContentBlock::AnthropicThinking {
                        thinking,
                        signature,
                    } => crate::util::estimate_tokens(thinking)
                        .saturating_add(crate::util::estimate_tokens(signature)),
                    ContentBlock::OpenAIReasoning {
                        id,
                        summary,
                        status,
                        ..
                    } => crate::util::estimate_tokens(id)
                        .saturating_add(
                            summary
                                .iter()
                                .map(|text| crate::util::estimate_tokens(text))
                                .fold(0usize, |total, tokens| total.saturating_add(tokens)),
                        )
                        .saturating_add(
                            status
                                .as_deref()
                                .map(crate::util::estimate_tokens)
                                .unwrap_or_default(),
                        ),
                    ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        thought_signature,
                    } => crate::util::estimate_tokens(id)
                        .saturating_add(crate::util::estimate_tokens(name))
                        .saturating_add(serialized_token_estimate(input))
                        .saturating_add(
                            thought_signature
                                .as_deref()
                                .map(crate::util::estimate_tokens)
                                .unwrap_or_default(),
                        ),
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => crate::util::estimate_tokens(tool_use_id)
                        .saturating_add(crate::util::estimate_tokens(content)),
                    ContentBlock::Image { media_type, .. } => IMAGE_TOKEN_ESTIMATE
                        .saturating_add(crate::util::estimate_tokens(media_type)),
                    ContentBlock::OpenAICompaction { .. } => OPAQUE_NATIVE_ITEM_TOKEN_ESTIMATE,
                })
                .fold(0usize, |total, tokens| total.saturating_add(tokens));
            block_tokens.saturating_add(4)
        })
        .fold(0usize, |total, tokens| total.saturating_add(tokens))
}

impl Agent {
    /// Снимает provider-facing snapshot перед запросом.
    ///
    /// Здесь не меняются Session, transcript или provider state. Controller
    /// обновляет только свою локальную revision и запоминает estimate. Если
    /// registry отсутствует или повреждён, текущий transcript flow сохраняется.
    pub(super) fn prepare_context_preflight(
        &mut self,
        messages: &[Message],
        tools: &[ToolDefinition],
        split_prompt: &SplitSystemPrompt,
    ) -> ContextPreflightPlan {
        let system_prompt = if split_prompt.dynamic_part.is_empty() {
            split_prompt.static_part.clone()
        } else {
            format!(
                "{}\n\n{}",
                split_prompt.static_part, split_prompt.dynamic_part
            )
        };
        let skill_runtime_fingerprint = match SkillRuntimeRegistry::load_default() {
            Ok(registry) => Some(registry.fingerprint()),
            Err(_) => {
                crate::logging::warn(
                    "Skill runtime registry ignored during context preflight: invalid or unavailable",
                );
                None
            }
        };
        let components = ContextComponentHashes {
            system_prompt: Some(sha256_hex(system_prompt.as_bytes())),
            skills: skill_runtime_fingerprint,
            tools: Some(serialized_fingerprint(tools)),
            messages: Some(serialized_fingerprint(messages)),
            ..ContextComponentHashes::default()
        };
        let estimated_input_tokens = split_prompt
            .estimated_tokens()
            .saturating_add(ToolDefinition::aggregate_prompt_token_estimate(tools))
            .saturating_add(message_token_estimate(messages));
        let provider_context_limit = self.provider.context_window();
        let reserved_output_tokens = RESERVED_OUTPUT_TOKENS.min(provider_context_limit / 4);
        let safety_margin_tokens = SAFETY_MARGIN_TOKENS.min(
            provider_context_limit
                .saturating_sub(reserved_output_tokens)
                .saturating_div(16),
        );
        let budget = ContextBudget {
            provider_context_limit,
            reserved_output_tokens,
            safety_margin_tokens,
            estimated_input_tokens,
        };
        let provider_generation = super::stable_hash_str(&format!(
            "{}:{}:{}",
            self.provider.name(),
            self.provider.model(),
            provider_context_limit
        ));
        let plan = self
            .context_controller
            .prepare(&budget, components, provider_generation);

        crate::logging::info(&format!(
            "Context preflight: action={:?} revision={} estimated={} max={} provider_limit={}",
            plan.action,
            plan.revision.0,
            plan.estimated_input_tokens,
            plan.max_input_tokens,
            provider_context_limit
        ));
        if plan.needs_compaction() {
            crate::logging::warn(&format!(
                "Context preflight exceeded budget at revision {}: estimated {} > max {}; existing compaction path will be used",
                plan.revision.0, plan.estimated_input_tokens, plan.max_input_tokens
            ));
        }
        plan
    }

    pub(super) fn record_context_usage(
        &mut self,
        revision: ContextRevision,
        observed_input_tokens: u64,
    ) {
        let observed = usize::try_from(observed_input_tokens).unwrap_or(usize::MAX);
        if !self
            .context_controller
            .record_observed_input_tokens(revision, observed)
        {
            crate::logging::warn(&format!(
                "Ignored provider usage for stale context revision {}",
                revision.0
            ));
        }
    }
}
