//! Server-side pricing lookup for session cost estimation.
//! Mirrors the pricing table in presence-web/app_state.rs.

/// Per-token pricing in USD.
struct Pricing {
    input: f64,
    cache_write: f64,
    cached: f64,
    output: f64,
}

const TABLE: &[(&str, Pricing)] = &[
    (
        "gpt-5.5",
        Pricing {
            input: 5.0e-6,
            cache_write: 5.0e-6,
            cached: 0.5e-6,
            output: 30.0e-6,
        },
    ),
    (
        "gpt-5.4",
        Pricing {
            input: 2.5e-6,
            cache_write: 2.5e-6,
            cached: 0.25e-6,
            output: 15.0e-6,
        },
    ),
    (
        "gpt-5.4-mini",
        Pricing {
            input: 0.75e-6,
            cache_write: 0.75e-6,
            cached: 0.075e-6,
            output: 4.5e-6,
        },
    ),
    (
        "gpt-5.4-nano",
        Pricing {
            input: 0.2e-6,
            cache_write: 0.2e-6,
            cached: 0.02e-6,
            output: 1.25e-6,
        },
    ),
    (
        "gpt-5.2",
        Pricing {
            input: 1.75e-6,
            cache_write: 1.75e-6,
            cached: 0.175e-6,
            output: 14.0e-6,
        },
    ),
    (
        "gpt-5.2-codex",
        Pricing {
            input: 1.75e-6,
            cache_write: 1.75e-6,
            cached: 0.175e-6,
            output: 14.0e-6,
        },
    ),
    (
        "gpt-5.3-codex",
        Pricing {
            input: 1.75e-6,
            cache_write: 1.75e-6,
            cached: 0.175e-6,
            output: 14.0e-6,
        },
    ),
    (
        "gpt-5",
        Pricing {
            input: 1.25e-6,
            cache_write: 1.25e-6,
            cached: 0.125e-6,
            output: 10.0e-6,
        },
    ),
    (
        "gpt-5-mini",
        Pricing {
            input: 0.25e-6,
            cache_write: 0.25e-6,
            cached: 0.025e-6,
            output: 2.0e-6,
        },
    ),
    (
        "gpt-4.1",
        Pricing {
            input: 2.0e-6,
            cache_write: 2.0e-6,
            cached: 0.5e-6,
            output: 8.0e-6,
        },
    ),
    (
        "gpt-4.1-mini",
        Pricing {
            input: 0.4e-6,
            cache_write: 0.4e-6,
            cached: 0.1e-6,
            output: 1.6e-6,
        },
    ),
    (
        "gpt-4.1-nano",
        Pricing {
            input: 0.1e-6,
            cache_write: 0.1e-6,
            cached: 0.025e-6,
            output: 0.4e-6,
        },
    ),
    (
        "o3",
        Pricing {
            input: 2.0e-6,
            cache_write: 2.0e-6,
            cached: 1.0e-6,
            output: 8.0e-6,
        },
    ),
    (
        "o3-pro",
        Pricing {
            input: 150.0e-6,
            cache_write: 150.0e-6,
            cached: 75.0e-6,
            output: 600.0e-6,
        },
    ),
    (
        "o4-mini",
        Pricing {
            input: 1.1e-6,
            cache_write: 1.1e-6,
            cached: 0.55e-6,
            output: 4.4e-6,
        },
    ),
    (
        "claude-opus-4-8",
        Pricing {
            input: 5.0e-6,
            cache_write: 6.25e-6,
            cached: 0.5e-6,
            output: 25.0e-6,
        },
    ),
    (
        "claude-opus-4-6",
        Pricing {
            input: 5.0e-6,
            cache_write: 6.25e-6,
            cached: 0.5e-6,
            output: 25.0e-6,
        },
    ),
    (
        "claude-opus-4-7",
        Pricing {
            input: 5.0e-6,
            cache_write: 6.25e-6,
            cached: 0.5e-6,
            output: 25.0e-6,
        },
    ),
    (
        "claude-sonnet-4-6",
        Pricing {
            input: 3.0e-6,
            cache_write: 3.75e-6,
            cached: 0.3e-6,
            output: 15.0e-6,
        },
    ),
    (
        "claude-sonnet-4-5-20250929",
        Pricing {
            input: 3.0e-6,
            cache_write: 3.75e-6,
            cached: 0.3e-6,
            output: 15.0e-6,
        },
    ),
    (
        "claude-opus-4-5-20250929",
        Pricing {
            input: 5.0e-6,
            cache_write: 6.25e-6,
            cached: 0.5e-6,
            output: 25.0e-6,
        },
    ),
    (
        "claude-haiku-4-5",
        Pricing {
            input: 1.0e-6,
            cache_write: 1.25e-6,
            cached: 0.1e-6,
            output: 5.0e-6,
        },
    ),
    (
        "gemini-3-flash",
        Pricing {
            input: 0.5e-6,
            cache_write: 0.5e-6,
            cached: 0.05e-6,
            output: 3.0e-6,
        },
    ),
    (
        "gemini-3.1-flash",
        Pricing {
            input: 0.5e-6,
            cache_write: 0.5e-6,
            cached: 0.05e-6,
            output: 3.0e-6,
        },
    ),
    (
        "gemini-2.5-pro",
        Pricing {
            input: 1.25e-6,
            cache_write: 1.25e-6,
            cached: 0.125e-6,
            output: 10.0e-6,
        },
    ),
    (
        "gemini-2.5-flash",
        Pricing {
            input: 0.3e-6,
            cache_write: 0.3e-6,
            cached: 0.03e-6,
            output: 2.5e-6,
        },
    ),
    (
        "gemini-2.5-flash-lite",
        Pricing {
            input: 0.1e-6,
            cache_write: 0.1e-6,
            cached: 0.01e-6,
            output: 0.4e-6,
        },
    ),
    (
        "gemini-2.0-flash",
        Pricing {
            input: 0.1e-6,
            cache_write: 0.1e-6,
            cached: 0.01e-6,
            output: 0.4e-6,
        },
    ),
];

fn model_key_matches(model: &str, key: &str) -> bool {
    model == key || model.starts_with(&format!("{key}-"))
}

fn find_pricing(model: &str) -> Option<&'static Pricing> {
    let model = model.rsplit('/').next().unwrap_or(model);
    for &(key, ref pricing) in TABLE {
        if model == key {
            return Some(pricing);
        }
    }
    TABLE
        .iter()
        .filter(|(key, _)| model_key_matches(model, key))
        .max_by_key(|(key, _)| key.len())
        .map(|(_, pricing)| pricing)
}

/// Estimate session cost from model name and token counts.
pub fn estimate_session_cost(
    model: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: u64,
    cache_creation_tokens: u64,
) -> Option<f64> {
    let p = find_pricing(model)?;
    let uncached = prompt_tokens
        .saturating_sub(cached_tokens)
        .saturating_sub(cache_creation_tokens);
    Some(
        uncached as f64 * p.input
            + cache_creation_tokens as f64 * p.cache_write
            + cached_tokens as f64 * p.cached
            + completion_tokens as f64 * p.output,
    )
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LiveUsageTokens {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub thinking_tokens: u64,
    pub input_text_tokens: u64,
    pub input_audio_tokens: u64,
    pub input_image_tokens: u64,
    pub cached_text_tokens: u64,
    pub cached_audio_tokens: u64,
    pub cached_image_tokens: u64,
    pub output_text_tokens: u64,
    pub output_audio_tokens: u64,
}

#[derive(Debug, Clone, Copy)]
struct LivePricing {
    text_input: f64,
    text_cached: f64,
    text_output: f64,
    audio_input: f64,
    audio_cached: f64,
    audio_output: f64,
    image_input: f64,
    image_cached: f64,
}

const LIVE_TABLE: &[(&str, LivePricing)] = &[
    (
        "gpt-realtime-1.5",
        LivePricing {
            text_input: 4.0e-6,
            text_cached: 0.4e-6,
            text_output: 16.0e-6,
            audio_input: 32.0e-6,
            audio_cached: 0.4e-6,
            audio_output: 64.0e-6,
            image_input: 5.0e-6,
            image_cached: 0.5e-6,
        },
    ),
    (
        "gpt-realtime",
        LivePricing {
            text_input: 4.0e-6,
            text_cached: 0.4e-6,
            text_output: 16.0e-6,
            audio_input: 32.0e-6,
            audio_cached: 0.4e-6,
            audio_output: 64.0e-6,
            image_input: 5.0e-6,
            image_cached: 0.5e-6,
        },
    ),
    (
        "gpt-realtime-mini",
        LivePricing {
            text_input: 0.6e-6,
            text_cached: 0.06e-6,
            text_output: 2.4e-6,
            audio_input: 10.0e-6,
            audio_cached: 0.3e-6,
            audio_output: 20.0e-6,
            image_input: 0.6e-6,
            image_cached: 0.06e-6,
        },
    ),
    (
        "gpt-4o-realtime-preview",
        LivePricing {
            text_input: 5.0e-6,
            text_cached: 2.5e-6,
            text_output: 20.0e-6,
            audio_input: 40.0e-6,
            audio_cached: 2.5e-6,
            audio_output: 80.0e-6,
            image_input: 5.0e-6,
            image_cached: 2.5e-6,
        },
    ),
    (
        "gemini-3.1-flash-live-preview",
        LivePricing {
            text_input: 0.75e-6,
            text_cached: 0.75e-6,
            text_output: 4.5e-6,
            audio_input: 3.0e-6,
            audio_cached: 3.0e-6,
            audio_output: 12.0e-6,
            image_input: 1.0e-6,
            image_cached: 1.0e-6,
        },
    ),
    (
        "gemini-2.5-flash-native-audio-preview-12-2025",
        LivePricing {
            text_input: 0.5e-6,
            text_cached: 0.5e-6,
            text_output: 3.0e-6,
            audio_input: 3.0e-6,
            audio_cached: 3.0e-6,
            audio_output: 12.0e-6,
            image_input: 1.0e-6,
            image_cached: 1.0e-6,
        },
    ),
];

fn find_live_pricing(model: &str) -> Option<&'static LivePricing> {
    let model = model.rsplit('/').next().unwrap_or(model);
    for &(key, ref pricing) in LIVE_TABLE {
        if model == key {
            return Some(pricing);
        }
    }
    LIVE_TABLE
        .iter()
        .filter(|(key, _)| model_key_matches(model, key))
        .max_by_key(|(key, _)| key.len())
        .map(|(_, pricing)| pricing)
}

fn billed_input_cost(tokens: u64, cached: u64, input_rate: f64, cached_rate: f64) -> f64 {
    let cached = cached.min(tokens);
    tokens.saturating_sub(cached) as f64 * input_rate + cached as f64 * cached_rate
}

pub fn estimate_live_usage_cost(model: &str, usage: LiveUsageTokens) -> Option<f64> {
    if let Some(p) = find_live_pricing(model) {
        let has_details = usage.input_text_tokens
            + usage.input_audio_tokens
            + usage.input_image_tokens
            + usage.cached_text_tokens
            + usage.cached_audio_tokens
            + usage.cached_image_tokens
            + usage.output_text_tokens
            + usage.output_audio_tokens
            > 0;
        if has_details {
            let input_cost = billed_input_cost(
                usage.input_text_tokens,
                usage.cached_text_tokens,
                p.text_input,
                p.text_cached,
            ) + billed_input_cost(
                usage.input_audio_tokens,
                usage.cached_audio_tokens,
                p.audio_input,
                p.audio_cached,
            ) + billed_input_cost(
                usage.input_image_tokens,
                usage.cached_image_tokens,
                p.image_input,
                p.image_cached,
            );
            let output_cost = (usage.output_text_tokens + usage.thinking_tokens) as f64
                * p.text_output
                + usage.output_audio_tokens as f64 * p.audio_output;
            return Some(input_cost + output_cost);
        }

        let cached = usage.cached_tokens.min(usage.input_tokens);
        return Some(
            usage.input_tokens.saturating_sub(cached) as f64 * p.audio_input
                + cached as f64 * p.audio_cached
                + (usage.output_tokens + usage.thinking_tokens) as f64 * p.audio_output,
        );
    }

    estimate_session_cost(
        model,
        usage.input_tokens,
        usage.output_tokens + usage.thinking_tokens,
        usage.cached_tokens,
        0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_4_8_session_cost_uses_anthropic_pricing() {
        let cost = estimate_session_cost("claude-opus-4-8", 1_000, 500, 200, 100).unwrap();
        let expected = 700.0 * 5.0e-6 + 100.0 * 6.25e-6 + 200.0 * 0.5e-6 + 500.0 * 25.0e-6;
        assert!((cost - expected).abs() < 1e-12);
    }

    #[test]
    fn opus_4_8_pricing_matches_version_suffixes() {
        let cost =
            estimate_session_cost("claude-opus-4-8-20260528", 1_000_000, 1_000_000, 0, 0).unwrap();
        assert!((cost - 30.0).abs() < 1e-12);
    }
}
