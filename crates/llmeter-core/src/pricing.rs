use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use serde::Deserialize;
use serde_json::Value;

use crate::{Provider, TokenCounts};

#[derive(Clone, Copy, Debug)]
pub struct ModelPricing {
    pub provider: Provider,
    pub model_pattern: &'static str,
    pub input_per_million: f64,
    pub cached_input_per_million: f64,
    pub output_per_million: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelRates {
    pub input_per_million: f64,
    pub cached_input_per_million: f64,
    pub cache_write_per_million: f64,
    pub output_per_million: f64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PricingSource {
    #[default]
    Fallback,
    DiskCache,
    Upstream,
    StaleCache,
}

impl PricingSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fallback => "fallback",
            Self::DiskCache => "disk-cache",
            Self::Upstream => "upstream",
            Self::StaleCache => "stale-cache",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PricingCatalog {
    rates: HashMap<String, ModelRates>,
    source: PricingSource,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
struct LiteLlmEntry {
    input_cost_per_token: Option<f64>,
    output_cost_per_token: Option<f64>,
    cache_read_input_token_cost: Option<f64>,
    cache_creation_input_token_cost: Option<f64>,
}

// Bundled estimates are intentionally static. They are not a billing source
// of truth; unknown models return None instead of being guessed.
const FALLBACK_PRICING: &[ModelPricing] = &[
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

const MODEL_ALIASES: &[(&str, &str)] = &[("k2p5", "kimi-k2p5")];

const SUFFIX_STRIP_PATTERNS: &[&str] = &[
    "-xhigh-fast",
    "-high-fast",
    "-medium-fast",
    "-low-fast",
    "-xhigh",
    "-high",
    "-medium",
    "-low",
    "-fast",
    "-thinking",
    "-free",
    "-preview",
];

static CATALOG: OnceLock<RwLock<PricingCatalog>> = OnceLock::new();

fn catalog_lock() -> &'static RwLock<PricingCatalog> {
    CATALOG.get_or_init(|| RwLock::new(PricingCatalog::fallback()))
}

fn lock_catalog() -> std::sync::RwLockReadGuard<'static, PricingCatalog> {
    catalog_lock()
        .read()
        .unwrap_or_else(|error| error.into_inner())
}

fn lock_catalog_mut() -> std::sync::RwLockWriteGuard<'static, PricingCatalog> {
    catalog_lock()
        .write()
        .unwrap_or_else(|error| error.into_inner())
}

impl PricingCatalog {
    pub fn fallback() -> Self {
        Self {
            rates: HashMap::new(),
            source: PricingSource::Fallback,
        }
    }

    pub fn from_litellm_json(value: &Value, source: PricingSource) -> Self {
        let mut rates = HashMap::new();
        let Some(object) = value.as_object() else {
            return Self { rates, source };
        };
        for (name, entry) in object {
            if name.starts_with('_') {
                continue;
            }
            if let Some(rate) = rates_from_litellm_entry(entry) {
                rates.insert(name.to_ascii_lowercase(), rate);
            }
        }
        Self { rates, source }
    }

    pub fn source(&self) -> PricingSource {
        self.source
    }

    pub fn model_count(&self) -> usize {
        self.rates.len()
    }

    pub fn lookup(&self, model: &str) -> Option<ModelRates> {
        let normalized = normalize_model(model);
        if normalized.is_empty() {
            return None;
        }
        if let Some(rates) = self.rates.get(&normalized) {
            return Some(*rates);
        }
        let aliased = apply_alias(&normalized);
        if aliased != normalized
            && let Some(rates) = self.rates.get(&aliased)
        {
            return Some(*rates);
        }
        let stripped = strip_reasoning_suffix(&normalized);
        if stripped != normalized
            && let Some(rates) = self.rates.get(&stripped)
        {
            return Some(*rates);
        }
        if let Some(rates) = self.lookup_prefixed(&normalized) {
            return Some(rates);
        }
        if stripped != normalized
            && let Some(rates) = self.lookup_prefixed(&stripped)
        {
            return Some(rates);
        }
        if aliased != normalized
            && let Some(rates) = self.lookup_prefixed(&aliased)
        {
            return Some(rates);
        }
        self.lookup_contained(&normalized)
            .or_else(|| self.lookup_contained(&stripped))
            .or_else(|| self.lookup_contained(&aliased))
    }

    pub fn estimate(
        &self,
        provider: Provider,
        model: Option<&str>,
        counts: TokenCounts,
    ) -> Option<f64> {
        let model = model?;
        if let Some(rates) = self.lookup(model) {
            return Some(cost_from_rates(rates, counts));
        }
        fallback_estimate(provider, model, counts)
    }

    fn lookup_prefixed(&self, model: &str) -> Option<ModelRates> {
        let suffix = format!("/{model}");
        let mut best_key: Option<&str> = None;
        for key in self.rates.keys() {
            if key.len() > suffix.len()
                && key.ends_with(&suffix)
                && better_prefix_key(key, best_key)
            {
                best_key = Some(key);
            }
        }
        best_key.and_then(|key| self.rates.get(key)).copied()
    }

    fn lookup_contained(&self, model: &str) -> Option<ModelRates> {
        let mut best: Option<(&str, ModelRates)> = None;
        for (key, rates) in &self.rates {
            if key.contains('/') || !model.contains(key.as_str()) {
                continue;
            }
            if best.is_none_or(|(current, _)| key.len() > current.len()) {
                best = Some((key, *rates));
            }
        }
        best.map(|(_, rates)| rates)
    }
}

pub fn install_catalog(catalog: PricingCatalog) {
    *lock_catalog_mut() = catalog;
}

pub fn current_catalog() -> PricingCatalog {
    lock_catalog().clone()
}

pub fn catalog_source() -> PricingSource {
    lock_catalog().source
}

pub fn estimate_cost_usd(
    provider: Provider,
    model: Option<&str>,
    counts: TokenCounts,
) -> Option<f64> {
    lock_catalog().estimate(provider, model, counts)
}

fn fallback_estimate(provider: Provider, model: &str, counts: TokenCounts) -> Option<f64> {
    let rule = FALLBACK_PRICING
        .iter()
        .find(|rule| rule.provider == provider && wildcard_match(rule.model_pattern, model))?;
    Some(cost_from_rates(
        ModelRates {
            input_per_million: rule.input_per_million,
            cached_input_per_million: rule.cached_input_per_million,
            cache_write_per_million: rule.input_per_million,
            output_per_million: rule.output_per_million,
        },
        counts,
    ))
}

fn cost_from_rates(rates: ModelRates, counts: TokenCounts) -> f64 {
    let input = counts.input_tokens as f64 / 1_000_000.0 * rates.input_per_million;
    let cached = counts.cached_input_tokens as f64 / 1_000_000.0 * rates.cached_input_per_million;
    let cache_creation =
        counts.cache_creation_input_tokens as f64 / 1_000_000.0 * rates.cache_write_per_million;
    let output = counts.output_tokens as f64 / 1_000_000.0 * rates.output_per_million;
    input + cached + cache_creation + output
}

fn rates_from_litellm_entry(entry: &Value) -> Option<ModelRates> {
    let parsed: LiteLlmEntry = serde_json::from_value(entry.clone()).ok()?;
    let input = parsed
        .input_cost_per_token
        .filter(|value| value.is_finite());
    let output = parsed
        .output_cost_per_token
        .filter(|value| value.is_finite());
    if input.is_none() && output.is_none() {
        return None;
    }
    let input_per_million = input.map(per_million).unwrap_or(0.0);
    let output_per_million = output.map(per_million).unwrap_or(0.0);
    let cached_input_per_million = parsed
        .cache_read_input_token_cost
        .filter(|value| value.is_finite())
        .map(per_million)
        .unwrap_or(input_per_million * 0.1);
    let cache_write_per_million = parsed
        .cache_creation_input_token_cost
        .filter(|value| value.is_finite())
        .map(per_million)
        .unwrap_or(input_per_million);
    Some(ModelRates {
        input_per_million,
        cached_input_per_million,
        cache_write_per_million,
        output_per_million,
    })
}

fn per_million(per_token: f64) -> f64 {
    (per_token * 1_000_000.0 * 10_000_000_000.0).round() / 10_000_000_000.0
}

fn normalize_model(model: &str) -> String {
    let mut value = model.trim().to_ascii_lowercase();
    while let Some(start) = value.find('(') {
        let Some(end) = value[start..].find(')') else {
            break;
        };
        value.replace_range(start..=start + end, " ");
    }
    if let Some((_, last)) = value.rsplit_once('/') {
        value = last.to_string();
    }
    let mut normalized = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '-') {
            normalized.push(character);
        } else {
            normalized.push('-');
        }
    }
    let mut normalized = collapse_dashes(&normalized);
    if let Some(rest) = normalized.strip_prefix("antigravity-") {
        normalized = rest.to_string();
    }
    if let Some(rest) = normalized
        .strip_prefix("gemini-claude-")
        .or_else(|| normalized.strip_prefix("gemini-gpt-"))
    {
        normalized = rest.to_string();
    }
    let normalized = normalize_claude_name(&normalized);
    strip_reasoning_suffix(&normalized)
}

fn normalize_claude_name(model: &str) -> String {
    if let Some(captures) = claude_dotted_tier(model) {
        return captures;
    }
    if let Some(captures) = claude_inverted_version(model) {
        return captures;
    }
    model.to_string()
}

fn claude_dotted_tier(model: &str) -> Option<String> {
    for family in ["sonnet", "opus", "haiku"] {
        let prefix = format!("claude-{family}-");
        let Some(rest) = model.strip_prefix(&prefix) else {
            continue;
        };
        let mut parts = rest.splitn(2, '-');
        let version = parts.next()?;
        if let Some((major, minor)) = version.split_once('.')
            && major.chars().all(|item| item.is_ascii_digit())
            && minor.chars().all(|item| item.is_ascii_digit())
        {
            let suffix = parts.next().unwrap_or("");
            return Some(if suffix.is_empty() {
                format!("claude-{family}-{major}-{minor}")
            } else {
                format!("claude-{family}-{major}-{minor}-{suffix}")
            });
        }
    }
    None
}

fn claude_inverted_version(model: &str) -> Option<String> {
    let rest = model.strip_prefix("claude-")?;
    for family in ["sonnet", "opus", "haiku"] {
        let suffix = format!("-{family}");
        let Some(version) = rest.strip_suffix(&suffix) else {
            continue;
        };
        let version = version.replace('.', "-");
        let mut parts = version.split('-');
        let major = parts.next()?;
        let minor = parts.next()?;
        if parts.next().is_some() {
            continue;
        }
        let major_number = major.parse::<u32>().ok()?;
        if major_number >= 4 && minor.chars().all(|item| item.is_ascii_digit()) {
            return Some(format!("claude-{family}-{major}-{minor}"));
        }
    }
    None
}

fn apply_alias(model: &str) -> String {
    MODEL_ALIASES
        .iter()
        .find(|(alias, _)| *alias == model)
        .map(|(_, target)| (*target).to_string())
        .unwrap_or_else(|| model.to_string())
}

fn strip_reasoning_suffix(model: &str) -> String {
    for suffix in SUFFIX_STRIP_PATTERNS {
        if let Some(stripped) = model.strip_suffix(suffix)
            && !stripped.is_empty()
        {
            return stripped.to_string();
        }
    }
    model.to_string()
}

fn collapse_dashes(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut previous_dash = false;
    for character in value.chars() {
        if character == '-' {
            if !previous_dash && !out.is_empty() {
                out.push('-');
            }
            previous_dash = true;
        } else {
            previous_dash = false;
            out.push(character);
        }
    }
    if out.ends_with('-') {
        out.pop();
    }
    out
}

fn better_prefix_key(candidate: &str, current: Option<&str>) -> bool {
    let Some(current) = current else {
        return true;
    };
    let score = |value: &str| {
        let slashes = value.bytes().filter(|item| *item == b'/').count();
        let mut score = slashes * 10;
        if value.contains("/us/") || value.contains("/eu/") {
            score += 5;
        }
        if value.starts_with("azure") {
            score += 2;
        }
        score
    };
    let candidate_score = score(candidate);
    let current_score = score(current);
    candidate_score < current_score || (candidate_score == current_score && candidate < current)
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
    use serde_json::json;

    fn million_tokens() -> TokenCounts {
        TokenCounts {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..Default::default()
        }
    }

    #[test]
    fn unknown_model_is_not_priced() {
        assert_eq!(
            PricingCatalog::fallback().estimate(
                Provider::Codex,
                Some("future-model"),
                TokenCounts::default()
            ),
            None
        );
    }

    #[test]
    fn known_model_estimate_is_static() {
        let value =
            PricingCatalog::fallback().estimate(Provider::Codex, Some("gpt-5.4"), million_tokens());
        assert_eq!(value, Some(12.5));
    }

    #[test]
    fn remote_catalog_overrides_fallback_and_matches_aliases() {
        let catalog = PricingCatalog::from_litellm_json(
            &json!({
                "gpt-5.4": {
                    "input_cost_per_token": 2.5e-6,
                    "output_cost_per_token": 1.5e-5,
                    "cache_read_input_token_cost": 2.5e-7,
                    "cache_creation_input_token_cost": 3.125e-6
                },
                "claude-opus-4-5": {
                    "input_cost_per_token": 5e-6,
                    "output_cost_per_token": 2.5e-5,
                    "cache_read_input_token_cost": 5e-7,
                    "cache_creation_input_token_cost": 6.25e-6
                },
                "openrouter/xiaomi/mimo-v2.5-pro": {
                    "input_cost_per_token": 1e-6,
                    "output_cost_per_token": 3e-6
                },
                "fireworks_ai/kimi-k2p5": {
                    "input_cost_per_token": 6e-7,
                    "output_cost_per_token": 3e-6
                },
                "gpt-5.6-terra": {
                    "input_cost_per_token": 2e-6,
                    "output_cost_per_token": 1.2e-5
                }
            }),
            PricingSource::Upstream,
        );
        assert_eq!(
            catalog.estimate(Provider::Codex, Some("gpt-5.4"), million_tokens()),
            Some(17.5)
        );
        assert_eq!(
            catalog.estimate(
                Provider::OpenCode,
                Some("claude-opus-4.5"),
                million_tokens()
            ),
            Some(30.0)
        );
        assert_eq!(
            catalog
                .estimate(Provider::OpenCode, Some("mimo-v2.5-pro"), million_tokens())
                .map(|value| (value * 100.0).round() / 100.0),
            Some(4.0)
        );
        assert_eq!(
            catalog
                .estimate(Provider::OpenCode, Some("k2p5"), million_tokens())
                .map(|value| (value * 100.0).round() / 100.0),
            Some(3.6)
        );
        assert_eq!(
            catalog.estimate(
                Provider::OpenCode,
                Some("gpt-5.6-terra-fast"),
                million_tokens()
            ),
            Some(14.0)
        );
    }

    #[test]
    fn normalize_maps_claude_display_names() {
        assert_eq!(normalize_model("Claude Opus 4.5"), "claude-opus-4-5");
        assert_eq!(
            normalize_model("antigravity-claude-sonnet-4-5-thinking"),
            "claude-sonnet-4-5"
        );
        assert_eq!(
            normalize_model("anthropic/claude-4.6-opus"),
            "claude-opus-4-6"
        );
    }
}
