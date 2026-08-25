use std::path::Path;

use chrono::{DateTime, Utc};
use llmeter_core::Provider;
use rusqlite::{Connection, params, params_from_iter, types::Value};

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
    pub cache_creation_input_tokens: u64,
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
            .unwrap_or_default()
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

#[derive(Clone, Copy, Debug)]
pub struct DashboardQuery {
    pub today_start: DateTime<Utc>,
    pub seven_start: DateTime<Utc>,
    pub thirty_start: DateTime<Utc>,
    pub heatmap_start: DateTime<Utc>,
    pub overview_start: DateTime<Utc>,
    pub overview_end: DateTime<Utc>,
    pub now_end: DateTime<Utc>,
    pub session_load: SessionLoad,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionQuery {
    pub provider: Option<Provider>,
    pub ended_after: Option<DateTime<Utc>>,
}

impl SessionQuery {
    pub fn is_unfiltered(self) -> bool {
        self.provider.is_none() && self.ended_after.is_none()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionLoad {
    #[default]
    Skip,
    Count,
    List(SessionQuery),
    ListAndCount(SessionQuery),
}

#[derive(Clone, Debug)]
pub struct DashboardSnapshot {
    pub today: Overview,
    pub seven_days: Overview,
    pub thirty_days: Overview,
    pub overview: Overview,
    pub overview_daily: Vec<DailyUsage>,
    pub overview_providers: Vec<ProviderUsage>,
    pub overview_models: Vec<ModelUsage>,
    pub heatmap_daily: Vec<DailyUsage>,
    pub heatmap_models: Vec<DailyModelUsage>,
    pub providers: Vec<ProviderUsage>,
    pub models: Vec<ModelUsage>,
    pub projects: Vec<ProjectUsage>,
    pub recent: Vec<RecentActivity>,
    pub sessions: Vec<SessionSummary>,
    pub session_count: u64,
}

pub type OverviewRangeData = (
    Overview,
    Vec<DailyUsage>,
    Vec<ProviderUsage>,
    Vec<ModelUsage>,
);

impl UsageRepository {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    pub fn load_dashboard(&self, query: DashboardQuery) -> Result<DashboardSnapshot, StorageError> {
        let mut snapshot = {
            let connection = self.database.lock()?;
            let (today, seven_days, thirty_days) = query_windowed_overviews(
                &connection,
                query.today_start,
                query.seven_start,
                query.thirty_start,
                query.now_end,
            )?;
            DashboardSnapshot {
                today,
                seven_days,
                thirty_days,
                overview: query_overview(&connection, query.overview_start, query.overview_end)?,
                overview_daily: query_daily_usage(
                    &connection,
                    query.overview_start,
                    query.overview_end,
                )?,
                overview_providers: query_provider_usage(
                    &connection,
                    query.overview_start,
                    query.overview_end,
                )?,
                overview_models: query_model_usage(
                    &connection,
                    query.overview_start,
                    query.overview_end,
                )?,
                heatmap_daily: query_daily_usage(&connection, query.heatmap_start, query.now_end)?,
                heatmap_models: query_daily_model_usage(
                    &connection,
                    query.heatmap_start,
                    query.now_end,
                )?,
                providers: query_provider_usage(&connection, query.thirty_start, query.now_end)?,
                models: query_model_usage(&connection, query.thirty_start, query.now_end)?,
                projects: query_project_usage(&connection, query.thirty_start, query.now_end)?,
                recent: query_recent_activity(&connection, 8)?,
                sessions: Vec::new(),
                session_count: 0,
            }
        };
        if query.session_load == SessionLoad::Skip {
            return Ok(snapshot);
        }
        let connection = self.database.lock()?;
        match query.session_load {
            SessionLoad::Skip => {}
            SessionLoad::Count => {
                snapshot.session_count = query_session_count(&connection)?;
            }
            SessionLoad::List(filter) => {
                snapshot.sessions = query_sessions(&connection, filter)?;
            }
            SessionLoad::ListAndCount(filter) => {
                snapshot.sessions = query_sessions(&connection, filter)?;
                snapshot.session_count = if filter.is_unfiltered() {
                    snapshot.sessions.len() as u64
                } else {
                    query_session_count(&connection)?
                };
            }
        }
        Ok(snapshot)
    }

    pub fn load_overview_range(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<OverviewRangeData, StorageError> {
        let connection = self.database.lock()?;
        Ok((
            query_overview(&connection, start, end)?,
            query_daily_usage(&connection, start, end)?,
            query_provider_usage(&connection, start, end)?,
            query_model_usage(&connection, start, end)?,
        ))
    }

    pub fn get_overview(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Overview, StorageError> {
        let connection = self.database.lock()?;
        query_overview(&connection, start, end)
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
        query_daily_usage(&connection, start, end)
    }

    pub fn get_daily_model_usage(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<DailyModelUsage>, StorageError> {
        let connection = self.database.lock()?;
        query_daily_model_usage(&connection, start, end)
    }

    pub fn get_provider_usage(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<ProviderUsage>, StorageError> {
        let connection = self.database.lock()?;
        query_provider_usage(&connection, start, end)
    }

    pub fn get_model_usage(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<ModelUsage>, StorageError> {
        let connection = self.database.lock()?;
        query_model_usage(&connection, start, end)
    }

    pub fn get_project_usage(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<ProjectUsage>, StorageError> {
        let connection = self.database.lock()?;
        query_project_usage(&connection, start, end)
    }

    pub fn get_recent_activity(&self, limit: usize) -> Result<Vec<RecentActivity>, StorageError> {
        let connection = self.database.lock()?;
        query_recent_activity(&connection, limit)
    }

    pub fn get_session_count(&self) -> Result<u64, StorageError> {
        let connection = self.database.lock()?;
        query_session_count(&connection)
    }

    pub fn get_sessions(&self) -> Result<Vec<SessionSummary>, StorageError> {
        self.get_sessions_matching(SessionQuery::default())
    }

    pub fn get_sessions_matching(
        &self,
        query: SessionQuery,
    ) -> Result<Vec<SessionSummary>, StorageError> {
        let connection = self.database.lock()?;
        query_sessions(&connection, query)
    }

    pub fn get_session_projects(&self) -> Result<Vec<String>, StorageError> {
        let connection = self.database.lock()?;
        let mut statement = connection.prepare(
            "SELECT DISTINCT project_name, project_path, source_file
             FROM usage_events",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(SessionSummary {
                provider: Provider::Codex,
                session_id: None,
                source_file: row.get(2)?,
                project_name: row.get(0)?,
                project_path: row.get(1)?,
                model: None,
                started_at: Utc::now(),
                ended_at: Utc::now(),
                turn_count: 0,
                total_tokens: 0,
                estimated_cost_usd: None,
            })
        })?;
        let mut names = rows
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|session| session.project_label())
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        Ok(names)
    }
}

fn parse_provider(value: String, column: usize) -> rusqlite::Result<Provider> {
    value.parse::<Provider>().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
        )
    })
}

fn overview_from_row(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Overview> {
    Ok(Overview {
        event_count: from_sqlite_u64(row.get(offset)?),
        input_tokens: from_sqlite_u64(row.get(offset + 1)?),
        cached_input_tokens: from_sqlite_u64(row.get(offset + 2)?),
        cache_creation_input_tokens: from_sqlite_u64(row.get(offset + 3)?),
        output_tokens: from_sqlite_u64(row.get(offset + 4)?),
        reasoning_tokens: from_sqlite_u64(row.get(offset + 5)?),
        total_tokens: from_sqlite_u64(row.get(offset + 6)?),
        estimated_cost_usd: row.get(offset + 7)?,
    })
}

fn query_overview(
    connection: &Connection,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Overview, StorageError> {
    connection
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(cached_input_tokens), 0),
                    COALESCE(SUM(cache_creation_input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(reasoning_tokens), 0), COALESCE(SUM(total_tokens), 0),
                    SUM(COALESCE(reported_cost_usd, estimated_cost_usd))
             FROM usage_events WHERE timestamp >= ?1 AND timestamp < ?2",
            params![start.timestamp(), end.timestamp()],
            |row| overview_from_row(row, 0),
        )
        .map_err(StorageError::from)
}

fn query_windowed_overviews(
    connection: &Connection,
    today_start: DateTime<Utc>,
    seven_start: DateTime<Utc>,
    thirty_start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<(Overview, Overview, Overview), StorageError> {
    let scan_start = today_start.min(seven_start).min(thirty_start);
    connection
        .query_row(
            "SELECT
                COUNT(*) FILTER (WHERE timestamp >= ?1),
                COALESCE(SUM(input_tokens) FILTER (WHERE timestamp >= ?1), 0),
                COALESCE(SUM(cached_input_tokens) FILTER (WHERE timestamp >= ?1), 0),
                COALESCE(SUM(cache_creation_input_tokens) FILTER (WHERE timestamp >= ?1), 0),
                COALESCE(SUM(output_tokens) FILTER (WHERE timestamp >= ?1), 0),
                COALESCE(SUM(reasoning_tokens) FILTER (WHERE timestamp >= ?1), 0),
                COALESCE(SUM(total_tokens) FILTER (WHERE timestamp >= ?1), 0),
                SUM(COALESCE(reported_cost_usd, estimated_cost_usd)) FILTER (WHERE timestamp >= ?1),
                COUNT(*) FILTER (WHERE timestamp >= ?2),
                COALESCE(SUM(input_tokens) FILTER (WHERE timestamp >= ?2), 0),
                COALESCE(SUM(cached_input_tokens) FILTER (WHERE timestamp >= ?2), 0),
                COALESCE(SUM(cache_creation_input_tokens) FILTER (WHERE timestamp >= ?2), 0),
                COALESCE(SUM(output_tokens) FILTER (WHERE timestamp >= ?2), 0),
                COALESCE(SUM(reasoning_tokens) FILTER (WHERE timestamp >= ?2), 0),
                COALESCE(SUM(total_tokens) FILTER (WHERE timestamp >= ?2), 0),
                SUM(COALESCE(reported_cost_usd, estimated_cost_usd)) FILTER (WHERE timestamp >= ?2),
                COUNT(*) FILTER (WHERE timestamp >= ?3),
                COALESCE(SUM(input_tokens) FILTER (WHERE timestamp >= ?3), 0),
                COALESCE(SUM(cached_input_tokens) FILTER (WHERE timestamp >= ?3), 0),
                COALESCE(SUM(cache_creation_input_tokens) FILTER (WHERE timestamp >= ?3), 0),
                COALESCE(SUM(output_tokens) FILTER (WHERE timestamp >= ?3), 0),
                COALESCE(SUM(reasoning_tokens) FILTER (WHERE timestamp >= ?3), 0),
                COALESCE(SUM(total_tokens) FILTER (WHERE timestamp >= ?3), 0),
                SUM(COALESCE(reported_cost_usd, estimated_cost_usd)) FILTER (WHERE timestamp >= ?3)
             FROM usage_events WHERE timestamp >= ?4 AND timestamp < ?5",
            params![
                today_start.timestamp(),
                seven_start.timestamp(),
                thirty_start.timestamp(),
                scan_start.timestamp(),
                end.timestamp()
            ],
            |row| {
                Ok((
                    overview_from_row(row, 0)?,
                    overview_from_row(row, 8)?,
                    overview_from_row(row, 16)?,
                ))
            },
        )
        .map_err(StorageError::from)
}

fn query_daily_usage(
    connection: &Connection,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<DailyUsage>, StorageError> {
    let mut statement = connection.prepare(
        "SELECT strftime('%Y-%m-%d', timestamp, 'unixepoch', 'localtime') AS day,
                COALESCE(SUM(total_tokens), 0),
                SUM(COALESCE(reported_cost_usd, estimated_cost_usd))
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

fn query_daily_model_usage(
    connection: &Connection,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<DailyModelUsage>, StorageError> {
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

fn query_provider_usage(
    connection: &Connection,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<ProviderUsage>, StorageError> {
    let mut statement = connection.prepare(
        "SELECT provider, COALESCE(SUM(total_tokens), 0), COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cached_input_tokens), 0),
                COALESCE(SUM(cache_creation_input_tokens), 0),
                SUM(COALESCE(reported_cost_usd, estimated_cost_usd)), MAX(timestamp)
         FROM usage_events WHERE timestamp >= ?1 AND timestamp < ?2
         GROUP BY provider ORDER BY 2 DESC",
    )?;
    let rows = statement.query_map(params![start.timestamp(), end.timestamp()], |row| {
        let last_activity: Option<i64> = row.get(7)?;
        Ok(ProviderUsage {
            provider: parse_provider(row.get(0)?, 0)?,
            total_tokens: from_sqlite_u64(row.get(1)?),
            input_tokens: from_sqlite_u64(row.get(2)?),
            output_tokens: from_sqlite_u64(row.get(3)?),
            cached_input_tokens: from_sqlite_u64(row.get(4)?),
            cache_creation_input_tokens: from_sqlite_u64(row.get(5)?),
            estimated_cost_usd: row.get(6)?,
            last_activity: last_activity
                .and_then(|value| DateTime::<Utc>::from_timestamp(value, 0)),
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StorageError::from)
}

fn query_model_usage(
    connection: &Connection,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<ModelUsage>, StorageError> {
    let mut statement = connection.prepare(
        "SELECT provider, COALESCE(model, 'Unknown'), COALESCE(SUM(total_tokens), 0),
                SUM(COALESCE(reported_cost_usd, estimated_cost_usd))
         FROM usage_events WHERE timestamp >= ?1 AND timestamp < ?2
         GROUP BY provider, model ORDER BY 3 DESC",
    )?;
    let rows = statement.query_map(params![start.timestamp(), end.timestamp()], |row| {
        Ok(ModelUsage {
            provider: parse_provider(row.get(0)?, 0)?,
            model: row.get(1)?,
            total_tokens: from_sqlite_u64(row.get(2)?),
            estimated_cost_usd: row.get(3)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StorageError::from)
}

fn query_project_usage(
    connection: &Connection,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<ProjectUsage>, StorageError> {
    let mut statement = connection.prepare(
        "SELECT COALESCE(project_name, 'Unknown project'), MAX(project_path),
                COALESCE(SUM(total_tokens), 0),
                SUM(COALESCE(reported_cost_usd, estimated_cost_usd)), MAX(timestamp)
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

fn query_recent_activity(
    connection: &Connection,
    limit: usize,
) -> Result<Vec<RecentActivity>, StorageError> {
    let mut statement = connection.prepare(
        "SELECT provider, model, session_id, total_tokens, timestamp
         FROM usage_events ORDER BY timestamp DESC LIMIT ?1",
    )?;
    let rows = statement.query_map(params![i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
        let timestamp: i64 = row.get(4)?;
        Ok(RecentActivity {
            provider: parse_provider(row.get(0)?, 0)?,
            model: row.get(1)?,
            session_id: row.get(2)?,
            total_tokens: from_sqlite_u64(row.get(3)?),
            timestamp: DateTime::<Utc>::from_timestamp(timestamp, 0).unwrap_or_else(Utc::now),
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StorageError::from)
}

fn query_session_count(connection: &Connection) -> Result<u64, StorageError> {
    connection
        .query_row(
            "SELECT COUNT(*) FROM (
                SELECT 1 FROM usage_events
                GROUP BY provider,
                         COALESCE(session_id, source_file, id),
                         COALESCE(source_file, ''),
                         COALESCE(project_name, ''),
                         COALESCE(project_path, '')
             )",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(from_sqlite_u64)
        .map_err(StorageError::from)
}

fn query_sessions(
    connection: &Connection,
    query: SessionQuery,
) -> Result<Vec<SessionSummary>, StorageError> {
    let mut sql = String::from(
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
                SUM(COALESCE(reported_cost_usd, estimated_cost_usd))
         FROM usage_events",
    );
    let mut values = Vec::new();
    if let Some(provider) = query.provider {
        sql.push_str(" WHERE provider = ?");
        values.push(Value::Text(provider.as_str().to_string()));
    }
    sql.push_str(
        " GROUP BY provider,
                  COALESCE(session_id, source_file, id),
                  COALESCE(source_file, ''),
                  COALESCE(project_name, ''),
                  COALESCE(project_path, '')",
    );
    if let Some(ended_after) = query.ended_after {
        sql.push_str(" HAVING MAX(timestamp) >= ?");
        values.push(Value::Integer(ended_after.timestamp()));
    }
    sql.push_str(" ORDER BY MAX(timestamp) DESC");
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(values), |row| {
        let started_at: i64 = row.get(6)?;
        let ended_at: i64 = row.get(7)?;
        Ok(SessionSummary {
            provider: parse_provider(row.get(0)?, 0)?,
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
