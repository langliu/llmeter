use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::Provider;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LimitsSnapshot {
    pub fetched_at: Option<DateTime<Utc>>,
    pub providers: Vec<ProviderLimits>,
}

impl LimitsSnapshot {
    pub fn provider(&self, provider: Provider) -> Option<&ProviderLimits> {
        self.providers.iter().find(|item| item.provider == provider)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderLimits {
    pub provider: Provider,
    pub configured: bool,
    pub plan: Option<String>,
    pub windows: Vec<LimitWindow>,
    pub captured_at: DateTime<Utc>,
    pub source: LimitSource,
    pub stale: bool,
    pub error: Option<String>,
    pub last_error: Option<String>,
}

impl ProviderLimits {
    pub fn not_configured(provider: Provider, captured_at: DateTime<Utc>) -> Self {
        Self {
            provider,
            configured: false,
            plan: None,
            windows: Vec::new(),
            captured_at,
            source: LimitSource::ProviderApi,
            stale: false,
            error: None,
            last_error: None,
        }
    }

    pub fn failed(
        provider: Provider,
        captured_at: DateTime<Utc>,
        error: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            configured: true,
            plan: None,
            windows: Vec::new(),
            captured_at,
            source: LimitSource::ProviderApi,
            stale: false,
            error: Some(error.into()),
            last_error: None,
        }
    }

    pub fn retain_unexpired_windows(&mut self, now: DateTime<Utc>) {
        self.windows
            .retain(|window| window.reset_at.is_none_or(|reset| reset > now));
    }

    pub fn cache_is_too_old(&self, now: DateTime<Utc>) -> bool {
        now.signed_duration_since(self.captured_at) > Duration::days(7)
    }

    pub fn as_stale(mut self, error: impl Into<String>, now: DateTime<Utc>) -> Option<Self> {
        if self.cache_is_too_old(now) {
            return None;
        }
        self.retain_unexpired_windows(now);
        if self.windows.is_empty() {
            return None;
        }
        self.source = LimitSource::DiskCache;
        self.stale = true;
        self.error = None;
        self.last_error = Some(error.into());
        Some(self)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LimitWindow {
    pub key: String,
    pub used_percent: f64,
    pub reset_at: Option<DateTime<Utc>>,
    pub window_seconds: Option<u64>,
    pub used_amount: Option<f64>,
    pub limit_amount: Option<f64>,
    pub unit: Option<String>,
}

impl LimitWindow {
    pub fn new(key: impl Into<String>, used_percent: f64) -> Self {
        Self {
            key: key.into(),
            used_percent: used_percent.clamp(0.0, 100.0),
            reset_at: None,
            window_seconds: None,
            used_amount: None,
            limit_amount: None,
            unit: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitSource {
    #[default]
    ProviderApi,
    DiskCache,
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    #[test]
    fn stale_snapshot_drops_windows_that_already_reset() {
        let now = Utc::now();
        let mut limits = ProviderLimits {
            provider: Provider::Codex,
            configured: true,
            plan: None,
            windows: vec![
                LimitWindow {
                    reset_at: Some(now - Duration::seconds(1)),
                    ..LimitWindow::new("five_hour", 90.0)
                },
                LimitWindow {
                    reset_at: Some(now + Duration::hours(1)),
                    ..LimitWindow::new("seven_day", 20.0)
                },
            ],
            captured_at: now - Duration::minutes(5),
            source: LimitSource::ProviderApi,
            stale: false,
            error: None,
            last_error: None,
        };

        limits = limits.as_stale("offline", now).unwrap();

        assert_eq!(limits.windows.len(), 1);
        assert_eq!(limits.windows[0].key, "seven_day");
        assert_eq!(limits.source, LimitSource::DiskCache);
        assert_eq!(limits.last_error.as_deref(), Some("offline"));
    }

    #[test]
    fn stale_snapshot_rejects_data_older_than_seven_days() {
        let now = Utc::now();
        let limits = ProviderLimits {
            provider: Provider::Claude,
            configured: true,
            plan: None,
            windows: vec![LimitWindow::new("credits", 10.0)],
            captured_at: now - Duration::days(8),
            source: LimitSource::ProviderApi,
            stale: false,
            error: None,
            last_error: None,
        };

        assert!(limits.as_stale("offline", now).is_none());
    }
}
