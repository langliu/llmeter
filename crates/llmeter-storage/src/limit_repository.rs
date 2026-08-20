use chrono::Utc;
use llmeter_core::{LimitSource, LimitsSnapshot, Provider, ProviderLimits};
use rusqlite::{OptionalExtension, params};

use crate::{Database, StorageError};

#[derive(Clone)]
pub struct LimitRepository {
    database: Database,
}

impl LimitRepository {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    pub fn save(&self, limits: &ProviderLimits) -> Result<(), StorageError> {
        if !limits.configured || limits.error.is_some() || limits.windows.is_empty() {
            return Ok(());
        }
        let payload = serde_json::to_string(limits)?;
        let connection = self.database.lock()?;
        connection.execute(
            "INSERT INTO limit_snapshots(provider, payload_json, captured_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(provider) DO UPDATE SET
                payload_json = excluded.payload_json,
                captured_at = excluded.captured_at",
            params![
                limits.provider.as_str(),
                payload,
                limits.captured_at.timestamp()
            ],
        )?;
        Ok(())
    }

    pub fn load(&self, provider: Provider) -> Result<Option<ProviderLimits>, StorageError> {
        let connection = self.database.lock()?;
        let payload = connection
            .query_row(
                "SELECT payload_json FROM limit_snapshots WHERE provider = ?1",
                params![provider.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        payload
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(StorageError::from)
    }

    pub fn load_snapshot(&self) -> Result<LimitsSnapshot, StorageError> {
        let now = Utc::now();
        let mut providers = Vec::new();
        for provider in [Provider::Claude, Provider::Codex, Provider::Grok] {
            if let Some(mut limits) = self.load(provider)? {
                if limits.cache_is_too_old(now) {
                    continue;
                }
                limits.retain_unexpired_windows(now);
                if limits.windows.is_empty() {
                    continue;
                }
                limits.source = LimitSource::DiskCache;
                limits.stale = true;
                providers.push(limits);
            }
        }
        let fetched_at = providers.iter().map(|item| item.captured_at).max();
        Ok(LimitsSnapshot {
            fetched_at,
            providers,
        })
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use llmeter_core::{LimitWindow, ProviderLimits};

    use super::*;

    #[test]
    fn round_trips_last_good_provider_snapshot() {
        let repository = LimitRepository::new(Database::open_in_memory().unwrap());
        let limits = ProviderLimits {
            provider: Provider::Codex,
            configured: true,
            plan: Some("Plus".into()),
            windows: vec![LimitWindow::new("five_hour", 37.0)],
            captured_at: Utc::now(),
            source: LimitSource::ProviderApi,
            stale: false,
            error: None,
            last_error: None,
        };

        repository.save(&limits).unwrap();
        let loaded = repository.load(Provider::Codex).unwrap().unwrap();

        assert_eq!(loaded, limits);
    }

    #[test]
    fn errors_do_not_replace_last_good_snapshot() {
        let repository = LimitRepository::new(Database::open_in_memory().unwrap());
        let good = ProviderLimits {
            provider: Provider::Claude,
            configured: true,
            plan: None,
            windows: vec![LimitWindow::new("seven_day", 18.0)],
            captured_at: Utc::now(),
            source: LimitSource::ProviderApi,
            stale: false,
            error: None,
            last_error: None,
        };
        repository.save(&good).unwrap();
        repository
            .save(&ProviderLimits::failed(
                Provider::Claude,
                Utc::now(),
                "network",
            ))
            .unwrap();

        assert_eq!(repository.load(Provider::Claude).unwrap(), Some(good));
    }

    #[test]
    fn startup_snapshot_ignores_expired_cache() {
        let repository = LimitRepository::new(Database::open_in_memory().unwrap());
        let old = ProviderLimits {
            provider: Provider::Grok,
            configured: true,
            plan: None,
            windows: vec![LimitWindow::new("monthly", 25.0)],
            captured_at: Utc::now() - Duration::days(8),
            source: LimitSource::ProviderApi,
            stale: false,
            error: None,
            last_error: None,
        };

        repository.save(&old).unwrap();

        assert!(repository.load_snapshot().unwrap().providers.is_empty());
    }
}
