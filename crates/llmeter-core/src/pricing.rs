use crate::{Provider, TokenCounts};

#[derive(Clone, Copy, Debug)]
pub struct ModelPricing {
    pub provider: Provider,
    pub model_pattern: &'static str,
    pub input_per_million: f64,
    pub cached_input_per_million: f64,
    pub output_per_million: f64,
}

// Bundled estimates are intentionally static. They are not a billing source
// of truth; unknown models return None instead of being guessed.
const PRICING: &[ModelPricing] = &[
    ModelPricing {
        provider: Provider::Codex,
        model_pattern: "gpt-5*",
        input_per_million: 2.5,
        cached_input_per_million: 0.25,
        output_per_million: 10.0,
    },
    ModelPricing {
        provider: Provider::Claude,
        model_pattern: "claude-*",
        input_per_million: 3.0,
        cached_input_per_million: 0.30,
        output_per_million: 15.0,
    },
    ModelPricing {
        provider: Provider::Pi,
        model_pattern: "gpt-5*",
        input_per_million: 2.5,
        cached_input_per_million: 0.25,
        output_per_million: 10.0,
    },
];

pub fn estimate_cost_usd(
    provider: Provider,
    model: Option<&str>,
    counts: TokenCounts,
) -> Option<f64> {
    let model = model?;
    let rule = PRICING
        .iter()
        .find(|rule| rule.provider == provider && wildcard_match(rule.model_pattern, model))?;
    let input = counts.input_tokens as f64 / 1_000_000.0 * rule.input_per_million;
    let cached = counts.cached_input_tokens as f64 / 1_000_000.0 * rule.cached_input_per_million;
    let cache_creation =
        counts.cache_creation_input_tokens as f64 / 1_000_000.0 * rule.input_per_million;
    let output = counts.output_tokens as f64 / 1_000_000.0 * rule.output_per_million;
    Some(input + cached + cache_creation + output)
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let value = value.to_ascii_lowercase();
    match pattern.strip_suffix('*') {
        Some(prefix) => value.starts_with(prefix),
        None => pattern == value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_model_is_not_priced() {
        assert_eq!(
            estimate_cost_usd(
                Provider::Codex,
                Some("future-model"),
                TokenCounts::default()
            ),
            None
        );
    }

    #[test]
    fn known_model_estimate_is_static() {
        let value = estimate_cost_usd(
            Provider::Codex,
            Some("gpt-5.4"),
            TokenCounts {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                ..Default::default()
            },
        );
        assert_eq!(value, Some(12.5));
    }
}
