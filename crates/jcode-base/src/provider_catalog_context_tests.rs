use jcode_provider_core::models::open_weight_family_context_limit as context_limit_for_model;

#[test]
fn newer_open_weight_family_context_limits_match_published_windows() {
    for (model, expected) in [
        ("grok-4.3", 1_000_000),
        ("grok-4.5", 500_000),
        ("grok-4.6", 500_000),
        ("seed-2.0-pro", 256_000),
        ("seed-2.0-code", 256_000),
        ("seed-2.0-mini", 256_000),
        ("step-3.7-flash", 262_144),
        ("step-3.7-flash-novita", 262_144),
        ("hy3", 262_144),
        ("hy3-tencent", 262_144),
        ("hy3-novita", 262_144),
        ("ling-3.0-flash", 131_072),
        ("inkling", 524_288),
        ("inkling-small", 524_288),
        ("nemotron-3-ultra", 262_144),
        ("nemotron-3-ultra-together", 262_144),
        ("nemotron-3-super-120b", 262_144),
        ("nemotron-3.5-lightning", 262_144),
        ("mistral-large-latest", 256_000),
        ("mistral-medium-latest", 256_000),
        ("mistral-small-latest", 256_000),
        ("command-a-cohere", 256_000),
        ("llama-4-maverick", 1_048_576),
        ("llama-4-scout", 327_680),
        ("gemma-4-31b", 128_000),
    ] {
        assert_eq!(
            context_limit_for_model(model),
            Some(expected),
            "unexpected context window for {model}"
        );
    }
}
