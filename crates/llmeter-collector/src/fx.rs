use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tracing::warn;

pub const EXCHANGE_RATE_URL: &str = "https://open.er-api.com/v6/latest/USD";
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

const DEFAULT_EUR: f64 = 0.92;
const DEFAULT_GBP: f64 = 0.79;
const DEFAULT_CNY: f64 = 7.2;
const DEFAULT_JPY: f64 = 155.0;
const DEFAULT_HKD: f64 = 7.8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FxSource {
    Default,
    DiskCache,
    Upstream,
    StaleCache,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExchangeRates {
    pub eur: f64,
    pub gbp: f64,
    pub cny: f64,
    pub jpy: f64,
    pub hkd: f64,
    pub source: FxSource,
}

impl Default for ExchangeRates {
    fn default() -> Self {
        Self {
            eur: DEFAULT_EUR,
            gbp: DEFAULT_GBP,
            cny: DEFAULT_CNY,
            jpy: DEFAULT_JPY,
            hkd: DEFAULT_HKD,
            source: FxSource::Default,
        }
    }
}

impl ExchangeRates {
    pub fn usd_to(&self, code: &str) -> f64 {
        match code {
            "EUR" => positive(self.eur).unwrap_or(DEFAULT_EUR),
            "GBP" => positive(self.gbp).unwrap_or(DEFAULT_GBP),
            "CNY" => positive(self.cny).unwrap_or(DEFAULT_CNY),
            "JPY" => positive(self.jpy).unwrap_or(DEFAULT_JPY),
            "HKD" => positive(self.hkd).unwrap_or(DEFAULT_HKD),
            _ => 1.0,
        }
    }
}

pub fn cache_path(cache_dir: impl AsRef<Path>) -> std::path::PathBuf {
    cache_dir.as_ref().join("exchange-rates.json")
}

pub fn load_cached_exchange_rates(cache_dir: impl AsRef<Path>) -> ExchangeRates {
    read_cache(&cache_path(cache_dir), FxSource::DiskCache).unwrap_or_default()
}

pub fn refresh_exchange_rates(cache_dir: impl AsRef<Path>) -> Result<ExchangeRates> {
    refresh_exchange_rates_with(cache_dir, fetch_upstream)
}

pub fn refresh_exchange_rates_with(
    cache_dir: impl AsRef<Path>,
    fetch: impl FnOnce() -> Result<Value>,
) -> Result<ExchangeRates> {
    let cache_dir = cache_dir.as_ref();
    fs::create_dir_all(cache_dir)
        .with_context(|| format!("create exchange-rate cache {}", cache_dir.display()))?;
    let path = cache_path(cache_dir);
    if cache_is_fresh(&path)
        && let Ok(rates) = read_cache(&path, FxSource::DiskCache)
    {
        return Ok(rates);
    }
    match fetch() {
        Ok(value) => match rates_from_payload(&value, FxSource::Upstream) {
            Ok(rates) => {
                if let Err(error) = write_cache(&path, &rates) {
                    warn!(error = %error, "failed to write exchange-rate cache");
                }
                Ok(rates)
            }
            Err(error) => {
                warn!(error = %error, "exchange-rate payload rejected");
                Ok(cached_or_default(&path))
            }
        },
        Err(error) => {
            warn!(error = %error, "exchange-rate fetch failed");
            Ok(cached_or_default(&path))
        }
    }
}

fn rates_from_payload(value: &Value, source: FxSource) -> Result<ExchangeRates> {
    if let Some(result) = value.get("result").and_then(Value::as_str)
        && result != "success"
    {
        anyhow::bail!("exchange-rate result is {result}");
    }
    let rates = value
        .get("rates")
        .and_then(Value::as_object)
        .context("exchange-rate payload missing rates")?;
    Ok(ExchangeRates {
        eur: required_rate(rates, "EUR")?,
        gbp: required_rate(rates, "GBP")?,
        cny: required_rate(rates, "CNY")?,
        jpy: required_rate(rates, "JPY")?,
        hkd: required_rate(rates, "HKD")?,
        source,
    })
}

fn required_rate(rates: &serde_json::Map<String, Value>, key: &str) -> Result<f64> {
    number_field(rates, key).with_context(|| format!("missing or invalid {key} rate"))
}

fn cached_or_default(path: &Path) -> ExchangeRates {
    if path.exists() {
        read_cache(path, FxSource::StaleCache).unwrap_or_default()
    } else {
        ExchangeRates::default()
    }
}

fn number_field(rates: &serde_json::Map<String, Value>, key: &str) -> Option<f64> {
    rates.get(key).and_then(Value::as_f64).and_then(positive)
}

fn positive(value: f64) -> Option<f64> {
    (value.is_finite() && value > 0.0).then_some(value)
}

fn read_cache(path: &Path, source: FxSource) -> Result<ExchangeRates> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read exchange-rate cache {}", path.display()))?;
    let value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse exchange-rate cache {}", path.display()))?;
    let mut rates = rates_from_payload(&value, source)?;
    if source == FxSource::DiskCache && !cache_is_fresh(path) {
        rates.source = FxSource::StaleCache;
    }
    Ok(rates)
}

fn write_cache(path: &Path, rates: &ExchangeRates) -> Result<()> {
    let payload = json!({
        "rates": {
            "USD": 1.0,
            "EUR": rates.eur,
            "GBP": rates.gbp,
            "CNY": rates.cny,
            "JPY": rates.jpy,
            "HKD": rates.hkd,
        },
        "_meta": {
            "source": EXCHANGE_RATE_URL,
            "cached_at": chrono::Utc::now().to_rfc3339(),
        }
    });
    fs::write(path, serde_json::to_vec(&payload)?)
        .with_context(|| format!("write exchange-rate cache {}", path.display()))?;
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
    let response = ureq::get(EXCHANGE_RATE_URL)
        .set("User-Agent", "llmeter/0.1")
        .timeout(FETCH_TIMEOUT)
        .call()
        .context("request exchange rates")?;
    response.into_json().context("decode exchange-rate JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("llmeter-fx-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn missing_cache_uses_offline_defaults() {
        let dir = temp_dir("missing");
        let rates = load_cached_exchange_rates(&dir);
        assert_eq!(rates, ExchangeRates::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn fetch_writes_cache_and_second_call_skips_network() {
        let dir = temp_dir("fresh");
        let first = refresh_exchange_rates_with(&dir, || {
            Ok(json!({
                "rates": { "CNY": 6.8, "EUR": 0.91, "GBP": 0.8, "JPY": 150.0, "HKD": 7.75 }
            }))
        })
        .unwrap();
        assert_eq!(first.source, FxSource::Upstream);
        assert_eq!(first.cny, 6.8);
        assert_eq!(first.usd_to("CNY"), 6.8);
        assert_eq!(first.usd_to("USD"), 1.0);

        let second =
            refresh_exchange_rates_with(&dir, || panic!("fresh cache should not fetch")).unwrap();
        assert_eq!(second.source, FxSource::DiskCache);
        assert_eq!(second.cny, 6.8);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn fetch_failure_falls_back_to_defaults() {
        let dir = temp_dir("fail");
        let rates = refresh_exchange_rates_with(&dir, || anyhow::bail!("offline")).unwrap();
        assert_eq!(rates, ExchangeRates::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn fetch_failure_keeps_stale_cache() {
        let dir = temp_dir("stale");
        refresh_exchange_rates_with(&dir, || {
            Ok(json!({
                "result": "success",
                "rates": { "CNY": 6.8, "EUR": 0.91, "GBP": 0.8, "JPY": 150.0, "HKD": 7.75 }
            }))
        })
        .unwrap();
        age_cache(&cache_path(&dir));

        let rates = refresh_exchange_rates_with(&dir, || anyhow::bail!("offline")).unwrap();
        assert_eq!(rates.source, FxSource::StaleCache);
        assert_eq!(rates.cny, 6.8);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_or_failed_payload_does_not_overwrite_cache() {
        let dir = temp_dir("empty");
        refresh_exchange_rates_with(&dir, || {
            Ok(json!({
                "result": "success",
                "rates": { "CNY": 6.8, "EUR": 0.91, "GBP": 0.8, "JPY": 150.0, "HKD": 7.75 }
            }))
        })
        .unwrap();
        age_cache(&cache_path(&dir));

        let empty =
            refresh_exchange_rates_with(&dir, || Ok(json!({ "result": "success", "rates": {} })))
                .unwrap();
        assert_eq!(empty.source, FxSource::StaleCache);
        assert_eq!(empty.cny, 6.8);

        let failed = refresh_exchange_rates_with(&dir, || {
            Ok(json!({
                "result": "error",
                "rates": { "CNY": 1.0, "EUR": 1.0, "GBP": 1.0, "JPY": 1.0, "HKD": 1.0 }
            }))
        })
        .unwrap();
        assert_eq!(failed.source, FxSource::StaleCache);
        assert_eq!(failed.cny, 6.8);
        let _ = fs::remove_dir_all(dir);
    }

    fn age_cache(path: &Path) {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(25 * 60 * 60))
            .unwrap();
    }
}
