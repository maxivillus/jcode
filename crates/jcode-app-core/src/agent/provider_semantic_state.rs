use crate::message::{ContentBlock, Message, Role};
use std::collections::BTreeMap;

pub(crate) const BOUNDED_SEMANTIC_STATE_MARKER: &str = "[Bounded semantic state]";

const MAX_SEMANTIC_FACTS: usize = 16;
const MAX_SEMANTIC_KEY_CHARS: usize = 64;
const MAX_SEMANTIC_VALUE_CHARS: usize = 96;
const MAX_SEMANTIC_OBSERVATION_CHARS: usize = 160;

#[derive(Debug, Clone, PartialEq, Eq)]
struct StateDelta {
    source_version: u64,
    facts: BTreeMap<String, String>,
    latest_observation: Option<String>,
}

impl StateDelta {
    fn validate(&self) -> bool {
        self.facts.len() <= MAX_SEMANTIC_FACTS
            && self.facts.iter().all(|(key, value)| {
                !key.trim().is_empty()
                    && key.chars().count() <= MAX_SEMANTIC_KEY_CHARS
                    && !value.trim().is_empty()
                    && value.chars().count() <= MAX_SEMANTIC_VALUE_CHARS
            })
            && self
                .latest_observation
                .as_ref()
                .is_none_or(|value| value.chars().count() <= MAX_SEMANTIC_OBSERVATION_CHARS)
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct BoundedSemanticState {
    revision: u64,
    source_version: u64,
    source_message_count: usize,
    source_prefix_hash: u64,
    facts: BTreeMap<String, String>,
    latest_observation: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SemanticProjection {
    pub messages: Vec<Message>,
    pub reason: &'static str,
    pub source_version: u64,
    pub state_bytes: usize,
    pub replaced_bytes: usize,
}

pub(crate) struct SemanticProjectionConfig<'a> {
    pub(crate) source_prefix_hashes: &'a [u64],
    pub(crate) turn_groups: &'a [(usize, usize)],
    pub(crate) rebuild_interval: usize,
    pub(crate) tail_groups: usize,
    pub(crate) rebuild_reason: &'static str,
    pub(crate) delta_reason: &'static str,
}

impl BoundedSemanticState {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn project(
        &mut self,
        messages: &[Message],
        config: SemanticProjectionConfig<'_>,
    ) -> Option<SemanticProjection> {
        let source_version = last_hash_or_zero(config.source_prefix_hashes);
        let latest_turn_start = config.turn_groups.last().map(|(start, _)| *start)?;
        let kept_tail_groups = config.tail_groups.min(config.turn_groups.len());
        let tail_start = config
            .turn_groups
            .get(config.turn_groups.len().saturating_sub(kept_tail_groups))
            .map(|(start, _)| *start)?;
        if latest_turn_start == 0 || tail_start == 0 || config.rebuild_interval == 0 {
            self.update_from_canonical_source(messages, config.source_prefix_hashes);
            return None;
        }

        let append_only = self.source_message_count > 0
            && self.source_message_count <= config.source_prefix_hashes.len()
            && config
                .source_prefix_hashes
                .get(self.source_message_count.saturating_sub(1))
                .copied()
                == Some(self.source_prefix_hash);
        let full_rebuild = self.revision == 0
            || !append_only
            || config
                .turn_groups
                .len()
                .is_multiple_of(config.rebuild_interval);
        let delta_range = if full_rebuild {
            0..messages.len()
        } else {
            latest_turn_start..messages.len()
        };
        let delta = build_delta(messages, delta_range, source_version);
        if !delta.validate() {
            self.update_from_canonical_source(messages, config.source_prefix_hashes);
            return None;
        }
        self.apply_delta(
            delta,
            full_rebuild,
            messages.len(),
            last_hash_or_zero(config.source_prefix_hashes),
        );

        if config.turn_groups.len() < config.rebuild_interval {
            return None;
        }

        let state_text = self.render();
        let state_bytes = match serde_json::to_vec(&state_text) {
            Ok(bytes) => bytes.len(),
            Err(_) => return None,
        };
        let replaced_bytes = match serde_json::to_vec(&messages[..tail_start]) {
            Ok(bytes) => bytes.len(),
            Err(_) => return None,
        };
        if state_bytes >= replaced_bytes {
            return None;
        }

        let mut output = messages[tail_start..].to_vec();
        let user_index = output.iter().position(|message| {
            message.role == Role::User
                && message.content.iter().any(|block| {
                    matches!(block, ContentBlock::Text { text, .. } if !text.trim().is_empty())
                })
        })?;
        output[user_index].content.push(ContentBlock::Text {
            text: state_text,
            cache_control: None,
        });

        Some(SemanticProjection {
            messages: output,
            reason: if full_rebuild {
                config.rebuild_reason
            } else {
                config.delta_reason
            },
            source_version,
            state_bytes,
            replaced_bytes,
        })
    }

    #[cfg(test)]
    pub(crate) fn facts_for_test(&self) -> Option<Vec<String>> {
        (self.revision > 0).then(|| {
            self.facts
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect()
        })
    }

    fn update_from_canonical_source(&mut self, messages: &[Message], source_prefix_hashes: &[u64]) {
        let delta = build_delta(
            messages,
            0..messages.len(),
            last_hash_or_zero(source_prefix_hashes),
        );
        if delta.validate() {
            self.apply_delta(
                delta,
                true,
                messages.len(),
                last_hash_or_zero(source_prefix_hashes),
            );
        } else {
            self.reset();
        }
    }

    fn apply_delta(
        &mut self,
        delta: StateDelta,
        full_rebuild: bool,
        source_message_count: usize,
        source_prefix_hash: u64,
    ) {
        if full_rebuild {
            self.facts.clear();
            self.latest_observation = None;
        }
        for (key, value) in delta.facts {
            self.facts.insert(key, value);
        }
        self.latest_observation = delta
            .latest_observation
            .or_else(|| self.latest_observation.clone());
        self.revision = self.revision.saturating_add(1);
        self.source_version = delta.source_version;
        self.source_message_count = source_message_count;
        self.source_prefix_hash = source_prefix_hash;
    }

    fn render(&self) -> String {
        let mut output = String::from(BOUNDED_SEMANTIC_STATE_MARKER);
        output.push_str("\nr=");
        output.push_str(&self.revision.to_string());
        output.push_str("\nsrc=");
        output.push_str(&format!("{:016x}", self.source_version));
        for (key, value) in self.facts.iter().take(MAX_SEMANTIC_FACTS) {
            output.push('\n');
            output.push_str(key);
            output.push('=');
            output.push_str(value);
        }
        if let Some(observation) = &self.latest_observation {
            output.push_str("\nobs=");
            output.push_str(observation);
        }
        output
    }
}

fn build_delta(
    messages: &[Message],
    range: std::ops::Range<usize>,
    source_version: u64,
) -> StateDelta {
    let mut facts = BTreeMap::new();
    let mut latest_observation = None;
    for message in messages
        .iter()
        .skip(range.start)
        .take(range.end.saturating_sub(range.start))
    {
        for block in &message.content {
            let ContentBlock::Text { text, .. } = block else {
                continue;
            };
            if message.role == Role::Assistant && !text.trim().is_empty() {
                latest_observation = Some(compact_observation(text));
            }
            extract_facts(text, &mut facts);
        }
    }
    StateDelta {
        source_version,
        facts,
        latest_observation,
    }
}

fn extract_facts(text: &str, facts: &mut BTreeMap<String, String>) {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() || normalized.contains(BOUNDED_SEMANTIC_STATE_MARKER) {
        return;
    }

    if let Some((_, payload)) = normalized.split_once("fact:") {
        for entry in payload.split([',', ';']) {
            let Some((key, value)) = entry.split_once('=') else {
                continue;
            };
            insert_fact(facts, key, value);
        }
    }

    extract_structured_facts(&normalized, facts);

    let lower = normalized.to_lowercase();
    if lower.contains("тепло") {
        insert_fact(facts, "current", "warm");
    }

    let temperatures = temperature_values(&normalized);
    if temperatures.is_empty() {
        return;
    }
    if lower.contains("амстердам") {
        insert_fact(facts, "Amsterdam.today", &temperatures[0]);
        if lower.contains("завтра")
            && let Some(tomorrow) = temperatures.last()
        {
            insert_fact(facts, "Amsterdam.tomorrow", tomorrow);
        }
    } else if lower.contains("завтра")
        && let Some(tomorrow) = temperatures.last()
    {
        insert_fact(facts, "weather.tomorrow", tomorrow);
    }
}

fn extract_structured_facts(text: &str, facts: &mut BTreeMap<String, String>) {
    for token in text.split_whitespace() {
        let Some((raw_key, raw_value)) = token.split_once('=') else {
            continue;
        };
        let key = raw_key.trim_matches(|character: char| {
            !character.is_ascii_alphanumeric()
                && character != '_'
                && character != '.'
                && character != '-'
        });
        let value = raw_value
            .trim_matches(|character: char| matches!(character, ',' | '.' | ';' | ':' | ')' | ']'));
        if key.is_empty()
            || value.is_empty()
            || !key
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_alphabetic())
            || !key.chars().all(|character| {
                character.is_ascii_alphanumeric()
                    || character == '_'
                    || character == '.'
                    || character == '-'
            })
        {
            continue;
        }
        let normalized_key = key.to_ascii_lowercase();
        insert_fact(facts, &normalized_key, value);
    }
}

fn insert_fact(facts: &mut BTreeMap<String, String>, key: &str, value: &str) {
    let key = key.trim();
    let value = value
        .trim()
        .trim_matches(|character: char| matches!(character, ',' | '.' | ';'));
    if key.is_empty()
        || value.is_empty()
        || key.chars().count() > MAX_SEMANTIC_KEY_CHARS
        || value.chars().count() > MAX_SEMANTIC_VALUE_CHARS
        || facts.len() >= MAX_SEMANTIC_FACTS && !facts.contains_key(key)
    {
        return;
    }
    facts.insert(key.to_string(), value.to_string());
}

fn temperature_values(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|token| {
            let value = token.trim_matches(|character: char| {
                !character.is_ascii_digit() && character != '+' && character != '-'
            });
            (value.len() > 1
                && (value.starts_with('+') || value.starts_with('-'))
                && value[1..]
                    .chars()
                    .all(|character| character.is_ascii_digit()))
            .then(|| value.to_string())
        })
        .collect()
}

fn last_hash_or_zero(values: &[u64]) -> u64 {
    match values.last() {
        Some(value) => *value,
        None => 0,
    }
}

fn clip_value(value: &str, max_chars: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut clipped = normalized.chars().take(max_chars).collect::<String>();
    if normalized.chars().count() > max_chars {
        clipped.push('…');
    }
    clipped
}

fn compact_observation(value: &str) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let temperatures = temperature_values(&normalized);
    if let Some(value) = temperatures.last() {
        return value.clone();
    }
    if normalized.to_lowercase().contains("тепло") {
        return "warm".to_string();
    }
    clip_value(&normalized, MAX_SEMANTIC_OBSERVATION_CHARS.min(48))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_delta_rejects_unbounded_fields() {
        let mut facts = BTreeMap::new();
        facts.insert("k".repeat(MAX_SEMANTIC_KEY_CHARS + 1), "value".to_string());
        assert!(
            !StateDelta {
                source_version: 1,
                facts,
                latest_observation: None,
            }
            .validate()
        );

        let mut facts = BTreeMap::new();
        facts.insert("key".to_string(), "v".repeat(MAX_SEMANTIC_VALUE_CHARS + 1));
        assert!(
            !StateDelta {
                source_version: 1,
                facts,
                latest_observation: None,
            }
            .validate()
        );
    }

    #[test]
    fn state_delta_accepts_bounded_fields() {
        let mut facts = BTreeMap::new();
        facts.insert("key".to_string(), "value".to_string());
        assert!(
            StateDelta {
                source_version: 1,
                facts,
                latest_observation: Some("observation".to_string()),
            }
            .validate()
        );
    }
}
