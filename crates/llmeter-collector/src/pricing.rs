use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result};
use llmeter_core::{PricingCatalog, PricingSource, estimate_cost_usd, install_catalog};
use llmeter_storage::Database;
use serde_json::{Value, json};
use tracing::{info, warn};

pub const LITELLM_PRICING_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PricingRefresh {
    pub source: PricingSource,
    pub model_count: usize,
    pub repriced: usize,
}

pub fn cache_path(cache_dir: impl AsRef<Path>) -> PathBuf {
    cache_dir.as_ref().join("pricing.json")
}

pub fn load_cached_pricing(cache_dir: impl AsRef<Path>) -> Option<PricingCatalog> {
    let path = cache_path(cache_dir);
    let catalog = read_cache(&path, PricingSource::DiskCache).ok()?;
    install_catalog(catalog.clone());
    Some(catalog)
}

pub fn refresh_pricing(
    cache_dir: impl AsRef<Path>,
    database: Option<&Database>,
) -> Result<PricingRefresh> {
    refresh_pricing_with(cache_dir, database, fetch_upstream)
}

pub fn refresh_pricing_with(
    cache_dir: impl AsRef<Path>,
    database: Option<&Database>,
    fetch: impl FnOnce() -> Result<Value>,
) -> Result<PricingRefresh> {
    let cache_dir = cache_dir.as_ref();
    fs::create_dir_all(cache_dir)
        .with_context(|| format!("create pricing cache directory {}", cache_dir.display()))?;
    let path = cache_path(cache_dir);
    let catalog = load_or_fetch_catalog(&path, fetch)?;
    install_catalog(catalog.clone());
    let repriced = match database {
        Some(database) => reprice_events(database)?,
        None => 0,
    };
    info!(
        source = catalog.source().as_str(),
        models = catalog.model_count(),
        repriced,
        "loaded model pricing"
    );
    Ok(PricingRefresh {
        source: catalog.source(),
        model_count: catalog.model_count(),
        repriced,
    })
}

fn load_or_fetch_catalog(
    path: &Path,
    fetch: impl FnOnce() -> Result<Value>,
) -> Result<PricingCatalog> {
    if cache_is_fresh(path) {
        if let Ok(catalog) = read_cache(path, PricingSource::DiskCache) {
            return Ok(catalog);
        }
        warn!("pricing disk cache was unreadable; fetching upstream");
    }

    match fetch() {
        Ok(value) => {
            if let Err(error) = write_cache(path, &value) {
                warn!(error = %error, "failed to write pricing cache");
            }
            Ok(PricingCatalog::from_litellm_json(
                &value,
                PricingSource::Upstream,
            ))
        }
        Err(error) => {
            warn!(error = %error, "pricing upstream fetch failed");
            if path.exists() {
                return read_cache(path, PricingSource::StaleCache);
            }
            Ok(PricingCatalog::fallback())
        }
    }
}

fn read_cache(path: &Path, source: PricingSource) -> Result<PricingCatalog> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read pricing cache {}", path.display()))?;
    let value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse pricing cache {}", path.display()))?;
    Ok(PricingCatalog::from_litellm_json(&value, source))
}

fn write_cache(path: &Path, value: &Value) -> Result<()> {
    let Some(object) = value.as_object() else {
        anyhow::bail!("LiteLLM pricing payload is not an object");
    };
    let mut slim = serde_json::Map::new();
    let mut kept = 0usize;
    for (name, entry) in object {
        if name.starts_with('_') || !entry.is_object() {
            continue;
        }
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let mut out = serde_json::Map::new();
        for field in [
            "input_cost_per_token",
            "output_cost_per_token",
            "cache_read_input_token_cost",
            "cache_creation_input_token_cost",
        ] {
            if let Some(number) = entry.get(field).and_then(Value::as_f64) {
                out.insert(field.to_string(), json!(number));
            }
        }
        if out.is_empty() {
            continue;
        }
        slim.insert(name.clone(), Value::Object(out));
        kept += 1;
    }
    slim.insert(
        "_meta".into(),
        json!({
            "source": LITELLM_PRICING_URL,
            "cached_at": chrono::Utc::now().to_rfc3339(),
            "kept_models": kept,
        }),
    );
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec(&Value::Object(slim))?)
        .with_context(|| format!("write pricing cache {}", path.display()))?;
    Ok(())
}

fn cache_is_fresh(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age < CACHE_TTL)
}

fn fetch_upstream() -> Result<Value> {
    let response = ureq::get(LITELLM_PRICING_URL)
        .set("User-Agent", "llmeter/0.1")
        .timeout(FETCH_TIMEOUT)
        .call()
        .context("request LiteLLM pricing")?;
    response.into_json().context("decode LiteLLM pricing JSON")
}

pub fn reprice_events(database: &Database) -> Result<usize> {
    let rows = database.list_usage_for_pricing()?;
    let mut updates = Vec::new();
    for row in rows {
        let next = estimate_cost_usd(row.provider, row.model.as_deref(), row.counts);
        if cost_changed(row.estimated_cost_usd, next) {
            updates.push((row.id, next));
        }
    }
    Ok(database.update_estimated_costs(&updates)?)
}

fn cost_changed(previous: Option<f64>, next: Option<f64>) -> bool {
    match (previous, next) {
        (None, None) => false,
        (Some(previous), Some(next)) => (previous - next).abs() > 1e-9,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use llmeter_core::{Provider, UsageEvent, current_catalog, install_catalog};
    use serde_json::json;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn temp_cache_dir(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("llmeter-pricing-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn event(id: &str, model: &str, input: u64, output: u64) -> UsageEvent {
        UsageEvent {
            id: id.into(),
            provider: Provider::Codex,
            model: Some(model.into()),
            session_id: Some("session".into()),
            project_path: None,
            project_name: Some("demo".into()),
            timestamp: chrono::Utc::now(),
            input_tokens: input,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            output_tokens: output,
            reasoning_tokens: 0,
            total_tokens: input + output,
            estimated_cost_usd: None,
            source_file: None,
            source_event_id: Some(id.into()),
        }
    }

    #[test]
    fn refresh_uses_fetch_then_fresh_cache() {
        let _guard = TEST_LOCK.lock().unwrap();
        install_catalog(PricingCatalog::fallback());
        let cache_dir = temp_cache_dir("fresh");
        let payload = json!({
            "gpt-5.4": {
                "input_cost_per_token": 2.5e-6,
                "output_cost_per_token": 1.5e-5
            }
        });
        let first = refresh_pricing_with(&cache_dir, None, || Ok(payload.clone())).unwrap();
        assert_eq!(first.source, PricingSource::Upstream);
        assert_eq!(first.model_count, 1);

        let second =
            refresh_pricing_with(&cache_dir, None, || panic!("fresh cache should not fetch"))
                .unwrap();
        assert_eq!(second.source, PricingSource::DiskCache);
        assert_eq!(current_catalog().model_count(), 1);
        install_catalog(PricingCatalog::fallback());
        let _ = fs::remove_dir_all(cache_dir);
    }

    #[test]
    fn refresh_reprices_existing_events() {
        let _guard = TEST_LOCK.lock().unwrap();
        install_catalog(PricingCatalog::fallback());
        let cache_dir = temp_cache_dir("reprice");
        let database = Database::open_in_memory().unwrap();
        database
            .insert_usage_events(&[event("one", "gpt-5.4", 1_000_000, 1_000_000)])
            .unwrap();

        let result = refresh_pricing_with(&cache_dir, Some(&database), || {
            Ok(json!({
                "gpt-5.4": {
                    "input_cost_per_token": 2.5e-6,
                    "output_cost_per_token": 1.5e-5
                }
            }))
        })
        .unwrap();
        assert_eq!(result.repriced, 1);

        let stored = database.list_usage_for_pricing().unwrap();
        assert_eq!(stored[0].estimated_cost_usd, Some(17.5));
        install_catalog(PricingCatalog::fallback());
        let _ = fs::remove_dir_all(cache_dir);
    }
}
