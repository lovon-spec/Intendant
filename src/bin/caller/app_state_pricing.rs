//! Server-side pricing lookup for session cost estimation.
//! Mirrors the pricing table in presence-web/app_state.rs.

/// Per-token pricing in USD.
struct Pricing {
    input: f64,
    cached: f64,
    output: f64,
}

const TABLE: &[(&str, Pricing)] = &[
    ("gpt-5.4", Pricing { input: 2.5e-6, cached: 1.25e-6, output: 15.0e-6 }),
    ("gpt-5.4-mini", Pricing { input: 0.5e-6, cached: 0.25e-6, output: 3.0e-6 }),
    ("gpt-5.4-nano", Pricing { input: 0.15e-6, cached: 0.075e-6, output: 0.6e-6 }),
    ("gpt-5.2-codex", Pricing { input: 1.75e-6, cached: 0.175e-6, output: 7.0e-6 }),
    ("gpt-5", Pricing { input: 1.25e-6, cached: 0.625e-6, output: 10.0e-6 }),
    ("gpt-5-mini", Pricing { input: 0.25e-6, cached: 0.125e-6, output: 2.0e-6 }),
    ("gpt-4.1", Pricing { input: 2.0e-6, cached: 1.0e-6, output: 8.0e-6 }),
    ("gpt-4.1-mini", Pricing { input: 0.4e-6, cached: 0.2e-6, output: 1.6e-6 }),
    ("gpt-4.1-nano", Pricing { input: 0.1e-6, cached: 0.05e-6, output: 0.4e-6 }),
    ("o3", Pricing { input: 2.0e-6, cached: 1.0e-6, output: 8.0e-6 }),
    ("o3-pro", Pricing { input: 150.0e-6, cached: 75.0e-6, output: 600.0e-6 }),
    ("o4-mini", Pricing { input: 1.1e-6, cached: 0.55e-6, output: 4.4e-6 }),
    ("claude-opus-4-6", Pricing { input: 5.0e-6, cached: 0.5e-6, output: 25.0e-6 }),
    ("claude-sonnet-4-6", Pricing { input: 3.0e-6, cached: 0.3e-6, output: 15.0e-6 }),
    ("claude-sonnet-4-5-20250929", Pricing { input: 3.0e-6, cached: 0.3e-6, output: 15.0e-6 }),
    ("claude-opus-4-5-20250929", Pricing { input: 15.0e-6, cached: 1.5e-6, output: 75.0e-6 }),
    ("claude-haiku-4-5", Pricing { input: 0.25e-6, cached: 0.025e-6, output: 1.25e-6 }),
    ("gemini-2.5-pro", Pricing { input: 1.25e-6, cached: 0.125e-6, output: 10.0e-6 }),
    ("gemini-2.5-flash", Pricing { input: 0.3e-6, cached: 0.03e-6, output: 2.5e-6 }),
    ("gemini-2.5-flash-lite", Pricing { input: 0.1e-6, cached: 0.01e-6, output: 0.4e-6 }),
    ("gemini-2.0-flash", Pricing { input: 0.1e-6, cached: 0.01e-6, output: 0.4e-6 }),
];

fn find_pricing(model: &str) -> Option<&'static Pricing> {
    for &(key, ref pricing) in TABLE {
        if model == key { return Some(pricing); }
    }
    for &(key, ref pricing) in TABLE {
        if model.starts_with(key) || model.contains(key) { return Some(pricing); }
    }
    None
}

/// Estimate session cost from model name and token counts.
pub fn estimate_session_cost(
    model: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: u64,
) -> Option<f64> {
    let p = find_pricing(model)?;
    let uncached = prompt_tokens.saturating_sub(cached_tokens);
    Some(
        uncached as f64 * p.input
            + cached_tokens as f64 * p.cached
            + completion_tokens as f64 * p.output,
    )
}
