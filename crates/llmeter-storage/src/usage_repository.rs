use std::path::Path;

use chrono::{DateTime, Utc};
use llmeter_core::Provider;
use rusqlite::params;

use crate::{Database, StorageError, database::from_sqlite_u64};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overview {
    pub event_count: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub estimated_cost_usd: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DailyUsage {
    pub day: String,
    pub total_tokens: u64,
    pub estimated_cost_usd: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DailyModelUsage {
    pub day: String,
    pub model: String,
    pub total_tokens: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderUsage {
    pub provider: Provider,
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub estimated_cost_usd: Option<f64>,
    pub last_activity: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelUsage {
    pub provider: Provider,
    pub model: String,
    pub total_tokens: u64,
    pub estimated_cost_usd: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectUsage {
    pub project_name: String,
    pub project_path: Option<String>,
    pub total_tokens: u64,
    pub estimated_cost_usd: Option<f64>,
    pub last_activity: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecentActivity {
    pub provider: Provider,
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub total_tokens: u64,
    pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionSummary {
    pub provider: Provider,
    pub session_id: Option<String>,
    pub source_file: Option<String>,
    pub project_name: Option<String>,
    pub project_path: Option<String>,
    pub model: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub turn_count: u64,
    pub total_tokens: u64,
    pub estimated_cost_usd: Option<f64>,
}

impl SessionSummary {
    pub fn title(&self) -> String {
        if let Some(name) = self
            .project_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != "Unknown project")
        {
            return name.to_string();
        }
        if let Some(path) = self.project_path.as_deref()
            && let Some(name) = Path::new(path).file_name()
        {
            let name = name.to_string_lossy();
            if !name.is_empty() {
                return name.into_owned();
            }
        }
        if let Some(inferred) = infer_project_from_source(self.source_file.as_deref()) {
            return inferred;
        }
        self.session_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(short_session_label)
            .unwrap_or_else(|| "未命名会话".into())
    }

    pub fn project_label(&self) -> Option<String> {
        self.project_name
            .clone()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                self.project_path.as_deref().and_then(|path| {
                    Path::new(path)
                        .file_name()
                        .map(|value| value.to_string_lossy().into_owned())
                        .filter(|value| !value.is_empty())
                })
            })
            .or_else(|| infer_project_from_source(self.source_file.as_deref()))
    }

    pub fn duration_secs(&self) -> i64 {
        (self.ended_at - self.started_at).num_seconds().max(0)
    }

    pub fn is_one_shot(&self) -> bool {
        self.turn_count <= 1
    }

    pub fn resume_command(&self) -> Option<String> {
        self.provider
            .resume_ref(self.session_id.as_deref(), self.source_file.as_deref())
            .and_then(|value| self.provider.resume_command(&value))
    }

    pub fn matches_query(&self, query: &str) -> bool {
        let query = query.trim();
        if query.is_empty() {
            return true;
        }
        let query = query.to_ascii_lowercase();
        let haystacks = [
            Some(self.title()),
            self.project_label(),
            self.project_path.clone(),
            self.model.clone(),
            self.session_id.clone(),
            self.source_file.clone(),
            Some(self.provider.display_name().to_string()),
        ];
        haystacks
            .into_iter()
            .flatten()
            .any(|value| value.to_ascii_lowercase().contains(&query))
    }
}

fn infer_project_from_source(source: Option<&str>) -> Option<String> {
    let parent = Path::new(source?)
        .parent()
        .and_then(|path| path.file_name())
        .map(|value| value.to_string_lossy().into_owned())?;
    let cleaned = parent.trim_matches('-');
    if cleaned.is_empty() {
        return None;
    }
    let parts = cleaned
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return None;
    }
    if parts.len() >= 3 && parts[parts.len() - 1].len() <= 4 {
        return Some(parts[parts.len() - 3..].join("-"));
    }
    Some(parts[parts.len() - 1].to_string())
}

fn short_session_label(session_id: &str) -> String {
    if session_id.len() <= 24 {
        session_id.to_string()
    } else {
        format!("{}\u{2026}", &session_id[..16])
    }
}

#[derive(Clone)]
pub struct UsageRepository {
    database: Database,
}

impl UsageRepository {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    pub fn get_overview(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Overview, StorageError> {
        let connection = self.database.lock()?;
        connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(cached_input_tokens), 0),
                        COALESCE(SUM(cache_creation_input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                        COALESCE(SUM(reasoning_tokens), 0), COALESCE(SUM(total_tokens), 0),
                        SUM(estimated_cost_usd)
                 FROM usage_events WHERE timestamp >= ?1 AND timestamp < ?2",
                params![start.timestamp(), end.timestamp()],
                |row| {
                    Ok(Overview {
                        event_count: from_sqlite_u64(row.get::<_, i64>(0)?),
                        input_tokens: from_sqlite_u64(row.get(1)?),
                        cached_input_tokens: from_sqlite_u64(row.get(2)?),
                        cache_creation_input_tokens: from_sqlite_u64(row.get(3)?),
                        output_tokens: from_sqlite_u64(row.get(4)?),
                        reasoning_tokens: from_sqlite_u64(row.get(5)?),
                        total_tokens: from_sqlite_u64(row.get(6)?),
                        estimated_cost_usd: row.get(7)?,
                    })
                },
            )
            .map_err(StorageError::from)
    }

    pub fn get_today_usage(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Overview, StorageError> {
        self.get_overview(start, end)
    }

    pub fn get_daily_usage(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<DailyUsage>, StorageError> {
        let connection = self.database.lock()?;
        let mut statement = connection.prepare(
            "SELECT strftime('%Y-%m-%d', timestamp, 'unixepoch', 'localtime') AS day,
                    COALESCE(SUM(total_tokens), 0), SUM(estimated_cost_usd)
             FROM usage_events WHERE timestamp >= ?1 AND timestamp < ?2
             GROUP BY day ORDER BY day",
        )?;
        let rows = statement.query_map(params![start.timestamp(), end.timestamp()], |row| {
            Ok(DailyUsage {
                day: row.get(0)?,
                total_tokens: from_sqlite_u64(row.get(1)?),
                estimated_cost_usd: row.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    pub fn get_daily_model_usage(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<DailyModelUsage>, StorageError> {
        let connection = self.database.lock()?;
        let mut statement = connection.prepare(
            "SELECT strftime('%Y-%m-%d', timestamp, 'unixepoch', 'localtime') AS day,
                    COALESCE(model, 'Unknown'), COALESCE(SUM(total_tokens), 0)
             FROM usage_events WHERE timestamp >= ?1 AND timestamp < ?2
             GROUP BY day, model ORDER BY day, 3 DESC",
        )?;
        let rows = statement.query_map(params![start.timestamp(), end.timestamp()], |row| {
            Ok(DailyModelUsage {
                day: row.get(0)?,
                model: row.get(1)?,
                total_tokens: from_sqlite_u64(row.get(2)?),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    pub fn get_provider_usage(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<ProviderUsage>, StorageError> {
        let connection = self.database.lock()?;
        let mut statement = connection.prepare(
            "SELECT provider, COALESCE(SUM(total_tokens), 0), COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cached_input_tokens), 0),
                    SUM(estimated_cost_usd), MAX(timestamp)
             FROM usage_events WHERE timestamp >= ?1 AND timestamp < ?2
             GROUP BY provider ORDER BY 2 DESC",
        )?;
        let rows = statement.query_map(params![start.timestamp(), end.timestamp()], |row| {
            let provider_text: String = row.get(0)?;
            let provider = provider_text.parse::<Provider>().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
                )
            })?;
            let last_activity: Option<i64> = row.get(6)?;
            Ok(ProviderUsage {
                provider,
                total_tokens: from_sqlite_u64(row.get(1)?),
                input_tokens: from_sqlite_u64(row.get(2)?),
                output_tokens: from_sqlite_u64(row.get(3)?),
                cached_input_tokens: from_sqlite_u64(row.get(4)?),
                estimated_cost_usd: row.get(5)?,
                last_activity: last_activity
                    .and_then(|value| DateTime::<Utc>::from_timestamp(value, 0)),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    pub fn get_model_usage(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<ModelUsage>, StorageError> {
        let connection = self.database.lock()?;
        let mut statement = connection.prepare(
            "SELECT provider, COALESCE(model, 'Unknown'), COALESCE(SUM(total_tokens), 0), SUM(estimated_cost_usd)
             FROM usage_events WHERE timestamp >= ?1 AND timestamp < ?2
             GROUP BY provider, model ORDER BY 3 DESC",
        )?;
        let rows = statement.query_map(params![start.timestamp(), end.timestamp()], |row| {
            let provider_text: String = row.get(0)?;
            let provider = provider_text.parse::<Provider>().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
                )
            })?;
            Ok(ModelUsage {
                provider,
                model: row.get(1)?,
                total_tokens: from_sqlite_u64(row.get(2)?),
                estimated_cost_usd: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    pub fn get_project_usage(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<ProjectUsage>, StorageError> {
        let connection = self.database.lock()?;
        let mut statement = connection.prepare(
            "SELECT COALESCE(project_name, 'Unknown project'), MAX(project_path),
                    COALESCE(SUM(total_tokens), 0), SUM(estimated_cost_usd), MAX(timestamp)
             FROM usage_events WHERE timestamp >= ?1 AND timestamp < ?2
             GROUP BY 1 ORDER BY 3 DESC",
        )?;
        let rows = statement.query_map(params![start.timestamp(), end.timestamp()], |row| {
            let last_activity: Option<i64> = row.get(4)?;
            Ok(ProjectUsage {
                project_name: row.get(0)?,
                project_path: row.get(1)?,
                total_tokens: from_sqlite_u64(row.get(2)?),
                estimated_cost_usd: row.get(3)?,
                last_activity: last_activity
                    .and_then(|value| DateTime::<Utc>::from_timestamp(value, 0)),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    pub fn get_recent_activity(&self, limit: usize) -> Result<Vec<RecentActivity>, StorageError> {
        let connection = self.database.lock()?;
        let mut statement = connection.prepare(
            "SELECT provider, model, session_id, total_tokens, timestamp
             FROM usage_events ORDER BY timestamp DESC LIMIT ?1",
        )?;
        let rows =
            statement.query_map(params![i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                let provider_text: String = row.get(0)?;
                let provider = provider_text.parse::<Provider>().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
                    )
                })?;
                let timestamp: i64 = row.get(4)?;
                Ok(RecentActivity {
                    provider,
                    model: row.get(1)?,
                    session_id: row.get(2)?,
                    total_tokens: from_sqlite_u64(row.get(3)?),
                    timestamp: DateTime::<Utc>::from_timestamp(timestamp, 0)
                        .unwrap_or_else(Utc::now),
                })
            })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    pub fn get_sessions(&self) -> Result<Vec<SessionSummary>, StorageError> {
        let connection = self.database.lock()?;
        let mut statement = connection.prepare(
            "SELECT provider,
                    session_id,
                    source_file,
                    project_name,
                    project_path,
                    MAX(model),
                    MIN(timestamp),
                    MAX(timestamp),
                    COUNT(*),
                    COALESCE(SUM(total_tokens), 0),
                    SUM(estimated_cost_usd)
             FROM usage_events
             GROUP BY provider,
                      COALESCE(session_id, source_file, id),
                      COALESCE(source_file, ''),
                      COALESCE(project_name, ''),
                      COALESCE(project_path, '')
             ORDER BY MAX(timestamp) DESC",
        )?;
        let rows = statement.query_map([], |row| {
            let provider_text: String = row.get(0)?;
            let provider = provider_text.parse::<Provider>().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
                )
            })?;
            let started_at: i64 = row.get(6)?;
            let ended_at: i64 = row.get(7)?;
            Ok(SessionSummary {
                provider,
                session_id: row.get(1)?,
                source_file: row.get(2)?,
                project_name: row.get(3)?,
                project_path: row.get(4)?,
                model: row.get(5)?,
                started_at: DateTime::<Utc>::from_timestamp(started_at, 0).unwrap_or_else(Utc::now),
                ended_at: DateTime::<Utc>::from_timestamp(ended_at, 0).unwrap_or_else(Utc::now),
                turn_count: from_sqlite_u64(row.get(8)?),
                total_tokens: from_sqlite_u64(row.get(9)?),
                estimated_cost_usd: row.get(10)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    pub fn get_session_projects(&self) -> Result<Vec<String>, StorageError> {
        let mut names = self
            .get_sessions()?
            .into_iter()
            .filter_map(|session| session.project_label())
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        Ok(names)
    }
}
