use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use llmeter_core::{
    Provider, ProviderDetection, ProviderStatus, SourceFile, SourceFormat, TokenCounts,
};
use rusqlite::{Connection, OpenFlags};

use super::{ParsedUsage, ProviderAdapter, home_dir, project_name};

const HERMES_PARSER_VERSION: u32 = 1;
const SESSION_COLUMNS: &[&str] = &[
    "id",
    "model",
    "started_at",
    "ended_at",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "reasoning_tokens",
    "cwd",
    "actual_cost_usd",
];
const MODEL_USAGE_COLUMNS: &[&str] = &[
    "session_id",
    "model",
    "billing_provider",
    "billing_base_url",
    "billing_mode",
    "task",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "reasoning_tokens",
    "actual_cost_usd",
    "last_seen",
];

#[derive(Clone, Debug)]
pub struct HermesAdapter {
    root: PathBuf,
}

impl Default for HermesAdapter {
    fn default() -> Self {
        let root = std::env::var_os("HERMES_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".hermes"));
        Self { root }
    }
}

impl HermesAdapter {
    pub fn with_home(home: PathBuf) -> Self {
        Self {
            root: home.join(".hermes"),
        }
    }

    fn databases(&self) -> Result<Vec<PathBuf>> {
        let mut databases = Vec::new();
        let primary = self.root.join("state.db");
        if primary.is_file() {
            databases.push(primary);
        }

        let profiles = self.root.join("profiles");
        if profiles.is_dir() {
            for entry in fs::read_dir(&profiles)
                .with_context(|| format!("read Hermes profiles directory {}", profiles.display()))?
            {
                let entry = entry?;
                if entry.file_type()?.is_symlink() || !entry.file_type()?.is_dir() {
                    continue;
                }
                let database = entry.path().join("state.db");
                if database.is_file() {
                    databases.push(database);
                }
            }
        }
        databases.sort();
        databases.dedup();
        Ok(databases)
    }
}

impl ProviderAdapter for HermesAdapter {
    fn provider(&self) -> Provider {
        Provider::Hermes
    }

    fn parser_version(&self) -> u32 {
        HERMES_PARSER_VERSION
    }
    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn detect(&self) -> Result<ProviderDetection> {
        let databases = self.databases()?;
        let roots = vec![
            self.root.clone(),
            self.root.join("state.db"),
            self.root.join("profiles"),
        ];
        if databases.is_empty() {
            return Ok(ProviderDetection {
                provider: Provider::Hermes,
                status: if self.root.exists() {
                    ProviderStatus::Installed
                } else {
                    ProviderStatus::NotInstalled
                },
                roots,
                detail: None,
            });
        }

        for database in &databases {
            if !supported_schema(database)? {
                return Ok(ProviderDetection {
                    provider: Provider::Hermes,
                    status: ProviderStatus::UnsupportedVersion,
                    roots,
                    detail: Some(format!(
                        "Hermes state database has an unsupported schema: {}",
                        database.display()
                    )),
                });
            }
        }
        let has_data = databases
            .iter()
            .map(|path| usage_row_count(path))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .any(|count| count > 0);
        Ok(ProviderDetection {
            provider: Provider::Hermes,
            status: if has_data {
                ProviderStatus::DataFound
            } else {
                ProviderStatus::Installed
            },
            roots,
            detail: None,
        })
    }

    fn discover_sources(&self) -> Result<Vec<SourceFile>> {
        self.databases()?
            .into_iter()
            .filter_map(|path| match supported_schema(&path) {
                Ok(true) => Some(Ok(SourceFile {
                    path,
                    provider: Provider::Hermes,
                    format: SourceFormat::Sqlite,
                    session_id: None,
                    project_path: None,
                    project_name: None,
                })),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    fn parse_line(&self, _source: &SourceFile, _line: &[u8]) -> Result<Option<ParsedUsage>> {
        Ok(None)
    }

    fn parse_sqlite(&self, source: &SourceFile) -> Result<Vec<ParsedUsage>> {
        let connection =
            Connection::open_with_flags(&source.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("open Hermes state database {}", source.path.display()))?;
        let mut attributed = HashMap::<String, UsageTotals>::new();
        let mut parsed = Vec::new();

        let mut statement = connection.prepare(
            "SELECT u.session_id, u.model, u.billing_provider, u.billing_base_url,
                    u.billing_mode, u.task, u.input_tokens, u.output_tokens,
                    u.cache_read_tokens, u.cache_write_tokens, u.reasoning_tokens,
                    u.actual_cost_usd, u.last_seen, s.started_at, s.ended_at, s.cwd
             FROM session_model_usage u
             JOIN sessions s ON s.id = u.session_id
             ORDER BY COALESCE(u.last_seen, s.ended_at, s.started_at),
                      u.session_id, u.model, u.billing_provider,
                      u.billing_base_url, u.billing_mode, u.task",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ModelUsageRow {
                session_id: row.get(0)?,
                model: row.get(1)?,
                billing_provider: row.get(2)?,
                billing_base_url: row.get(3)?,
                billing_mode: row.get(4)?,
                task: row.get(5)?,
                counts: token_counts(
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                ),
                actual_cost_usd: row.get(11)?,
                timestamp: timestamp_from_parts(row.get(12)?, row.get(14)?, row.get(13)?),
                cwd: row.get(15)?,
            })
        })?;
        for row in rows {
            let row = row?;
            attributed
                .entry(row.session_id.clone())
                .or_default()
                .add(row.counts, row.actual_cost_usd);
            if row.counts.is_zero() && positive_cost(row.actual_cost_usd).is_none() {
                continue;
            }
            let project_path = hermes_project_path(row.cwd.as_deref());
            let source_event_id = route_event_id(source, &row);
            parsed.push(ParsedUsage {
                counts: row.counts,
                cumulative_snapshot: None,
                timestamp: row.timestamp,
                model: non_empty(row.model),
                session_id: Some(row.session_id.clone()),
                project_name: project_name(project_path.as_deref()),
                project_path,
                source_event_id: Some(source_event_id),
                reported_cost_usd: positive_cost(row.actual_cost_usd),
            });
        }
        drop(statement);

        let mut statement = connection.prepare(
            "SELECT id, model, started_at, ended_at, input_tokens, output_tokens,
                    cache_read_tokens, cache_write_tokens, reasoning_tokens, cwd,
                    actual_cost_usd
             FROM sessions ORDER BY COALESCE(ended_at, started_at), id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(SessionUsageRow {
                session_id: row.get(0)?,
                model: row.get(1)?,
                timestamp: timestamp_from_parts(None, row.get(3)?, row.get(2)?),
                counts: token_counts(
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ),
                cwd: row.get(9)?,
                actual_cost_usd: row.get(10)?,
            })
        })?;
        for row in rows {
            let row = row?;
            let totals = attributed.get(&row.session_id).copied().unwrap_or_default();
            let counts = subtract_counts(row.counts, totals.counts);
            let actual_cost_usd = positive_cost(Some(
                row.actual_cost_usd.unwrap_or_default() - totals.actual_cost_usd,
            ));
            if counts.is_zero() && actual_cost_usd.is_none() {
                continue;
            }
            let project_path = hermes_project_path(row.cwd.as_deref());
            parsed.push(ParsedUsage {
                counts,
                cumulative_snapshot: None,
                timestamp: row.timestamp,
                model: row.model.and_then(non_empty),
                session_id: Some(row.session_id.clone()),
                project_name: project_name(project_path.as_deref()),
                project_path,
                source_event_id: Some(residual_event_id(source, &row.session_id)),
                reported_cost_usd: actual_cost_usd,
            });
        }
        parsed.sort_by(|left, right| {
            left.timestamp.cmp(&right.timestamp).then_with(|| {
                left.source_event_id
                    .as_deref()
                    .cmp(&right.source_event_id.as_deref())
            })
        });
        Ok(parsed)
    }
}

#[derive(Debug)]
struct ModelUsageRow {
    session_id: String,
    model: String,
    billing_provider: String,
    billing_base_url: String,
    billing_mode: String,
    task: String,
    counts: TokenCounts,
    actual_cost_usd: Option<f64>,
    timestamp: DateTime<Utc>,
    cwd: Option<String>,
}

#[derive(Debug)]
struct SessionUsageRow {
    session_id: String,
    model: Option<String>,
    timestamp: DateTime<Utc>,
    counts: TokenCounts,
    cwd: Option<String>,
    actual_cost_usd: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default)]
struct UsageTotals {
    counts: TokenCounts,
    actual_cost_usd: f64,
}

impl UsageTotals {
    fn add(&mut self, counts: TokenCounts, actual_cost_usd: Option<f64>) {
        self.counts = self.counts.saturating_add(counts);
        self.actual_cost_usd += positive_cost(actual_cost_usd).unwrap_or_default();
    }
}

fn supported_schema(path: &Path) -> Result<bool> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(has_columns(&connection, "sessions", SESSION_COLUMNS)?
        && has_columns(&connection, "session_model_usage", MODEL_USAGE_COLUMNS)?)
}

fn has_columns(connection: &Connection, table: &str, required: &[&str]) -> Result<bool> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .is_ok();
    if !exists {
        return Ok(false);
    }
    let pragma = format!("PRAGMA table_info('{}')", table.replace('\'', "''"));
    let mut statement = connection.prepare(&pragma)?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(required
        .iter()
        .all(|required| columns.iter().any(|column| column == required)))
}

fn usage_row_count(path: &Path) -> Result<u64> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let count = connection.query_row(
        "SELECT count(*) FROM sessions
         WHERE COALESCE(input_tokens, 0) + COALESCE(output_tokens, 0)
             + COALESCE(cache_read_tokens, 0) + COALESCE(cache_write_tokens, 0)
             + COALESCE(reasoning_tokens, 0) > 0
            OR COALESCE(actual_cost_usd, 0) > 0",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(count.max(0) as u64)
}

fn token_counts(
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
) -> TokenCounts {
    let input_tokens = non_negative(input);
    let output_tokens = non_negative(output);
    let cached_input_tokens = non_negative(cache_read);
    let cache_creation_input_tokens = non_negative(cache_write);
    let reasoning_tokens = non_negative(reasoning);
    TokenCounts {
        input_tokens,
        cached_input_tokens,
        cache_creation_input_tokens,
        output_tokens,
        reasoning_tokens,
        total_tokens: input_tokens
            .saturating_add(cached_input_tokens)
            .saturating_add(cache_creation_input_tokens)
            .saturating_add(output_tokens)
            .saturating_add(reasoning_tokens),
    }
}

fn subtract_counts(total: TokenCounts, attributed: TokenCounts) -> TokenCounts {
    token_counts(
        total.input_tokens.saturating_sub(attributed.input_tokens) as i64,
        total.output_tokens.saturating_sub(attributed.output_tokens) as i64,
        total
            .cached_input_tokens
            .saturating_sub(attributed.cached_input_tokens) as i64,
        total
            .cache_creation_input_tokens
            .saturating_sub(attributed.cache_creation_input_tokens) as i64,
        total
            .reasoning_tokens
            .saturating_sub(attributed.reasoning_tokens) as i64,
    )
}

fn timestamp_from_parts(
    preferred: Option<f64>,
    ended_at: Option<f64>,
    started_at: f64,
) -> DateTime<Utc> {
    let value = preferred.or(ended_at).unwrap_or(started_at).max(0.0);
    let seconds = value.trunc() as i64;
    let nanos = ((value.fract() * 1_000_000_000.0).round() as u32).min(999_999_999);
    DateTime::<Utc>::from_timestamp(seconds, nanos).unwrap_or_else(Utc::now)
}

fn hermes_project_path(value: Option<&str>) -> Option<PathBuf> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    if value.starts_with('{') {
        return serde_json::from_str::<serde_json::Value>(value)
            .ok()
            .and_then(|value| {
                value
                    .get("cwd")
                    .and_then(|value| value.as_str())
                    .map(PathBuf::from)
            });
    }
    Some(PathBuf::from(value))
}

fn route_event_id(source: &SourceFile, row: &ModelUsageRow) -> String {
    let source_path = source.path.to_string_lossy();
    let route = [
        source_path.as_ref(),
        row.session_id.as_str(),
        row.model.as_str(),
        row.billing_provider.as_str(),
        row.billing_base_url.as_str(),
        row.billing_mode.as_str(),
        row.task.as_str(),
    ]
    .join("\0");
    format!("usage:{}", blake3::hash(route.as_bytes()).to_hex())
}

fn residual_event_id(source: &SourceFile, session_id: &str) -> String {
    let value = format!("{}\0{session_id}\0residual", source.path.display());
    format!("residual:{}", blake3::hash(value.as_bytes()).to_hex())
}

fn positive_cost(value: Option<f64>) -> Option<f64> {
    value.filter(|cost| cost.is_finite() && *cost > 0.0)
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn non_negative(value: i64) -> u64 {
    value.max(0) as u64
}

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::Duration;
    use llmeter_storage::{Database, UsageRepository};
    use rusqlite::params;

    use super::*;
    use crate::sync::SyncEngine;

    fn create_state_database(path: &Path) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    model TEXT,
                    started_at REAL NOT NULL,
                    ended_at REAL,
                    input_tokens INTEGER DEFAULT 0,
                    output_tokens INTEGER DEFAULT 0,
                    cache_read_tokens INTEGER DEFAULT 0,
                    cache_write_tokens INTEGER DEFAULT 0,
                    reasoning_tokens INTEGER DEFAULT 0,
                    cwd TEXT,
                    actual_cost_usd REAL
                );
                CREATE TABLE session_model_usage (
                    session_id TEXT NOT NULL,
                    model TEXT NOT NULL,
                    billing_provider TEXT NOT NULL DEFAULT '',
                    billing_base_url TEXT NOT NULL DEFAULT '',
                    billing_mode TEXT NOT NULL DEFAULT '',
                    task TEXT NOT NULL DEFAULT '',
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                    actual_cost_usd REAL NOT NULL DEFAULT 0,
                    last_seen REAL,
                    PRIMARY KEY (
                        session_id, model, billing_provider,
                        billing_base_url, billing_mode, task
                    )
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO sessions VALUES
                 ('session-1', 'grok-4.6', 1787194800.0, 1787194860.0,
                  100, 20, 50, 5, 3, '/tmp/hermes-project', 0.42)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session_model_usage VALUES
                 ('session-1', 'grok-4.6', 'xai-oauth', '', '', '',
                  70, 10, 30, 5, 2, 0.30, 1787194840.0),
                 ('session-1', 'grok-4.5', 'xai-oauth', '', '', 'title_generation',
                  10, 4, 0, 0, 0, 0.02, 1787194850.0)",
                [],
            )
            .unwrap();
    }

    #[test]
    fn parses_model_usage_and_reconciles_session_residual() {
        let home = std::env::temp_dir().join(format!(
            "llmeter-hermes-adapter-{}-parse",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&home);
        let root = home.join(".hermes");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("state.db");
        create_state_database(&path);

        let adapter = HermesAdapter::with_home(home.clone());
        assert_eq!(adapter.detect().unwrap().status, ProviderStatus::DataFound);
        let source = adapter.discover_sources().unwrap().remove(0);
        let parsed = adapter.parse_sqlite(&source).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(
            parsed
                .iter()
                .map(|usage| usage.counts.total_tokens)
                .sum::<u64>(),
            178
        );
        let residual = parsed
            .iter()
            .find(|usage| {
                usage
                    .source_event_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with("residual:"))
            })
            .unwrap();
        assert_eq!(residual.counts.input_tokens, 20);
        assert_eq!(residual.counts.cached_input_tokens, 20);
        assert_eq!(residual.counts.reasoning_tokens, 1);
        assert!(
            (residual.reported_cost_usd.unwrap() - 0.1).abs() < 1e-9,
            "unexpected residual cost: {:?}",
            residual.reported_cost_usd
        );
        assert_eq!(
            residual.project_path.as_deref(),
            Some(Path::new("/tmp/hermes-project"))
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn syncs_state_database_idempotently_and_updates_snapshots() {
        let home = std::env::temp_dir().join(format!(
            "llmeter-hermes-adapter-{}-sync",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&home);
        let root = home.join(".hermes");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("state.db");
        create_state_database(&path);

        let database = Database::open_in_memory().unwrap();
        let engine = SyncEngine::with_adapters(
            database.clone(),
            vec![Box::new(HermesAdapter::with_home(home.clone()))],
        );
        let first = engine.sync_all().unwrap();
        assert_eq!(first.events_inserted, 3);
        assert_eq!(first.tokens_added, 178);
        let second = engine.sync_all().unwrap();
        assert_eq!(second.events_inserted, 0);
        assert_eq!(second.tokens_added, 0);

        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE session_model_usage SET input_tokens = input_tokens + 5
                 WHERE model = 'grok-4.6' AND task = ''",
                [],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE sessions SET input_tokens = input_tokens + 5 WHERE id = ?1",
                params!["session-1"],
            )
            .unwrap();
        drop(connection);
        let third = engine.sync_all().unwrap();
        assert_eq!(third.events_inserted, 0);

        let overview = UsageRepository::new(database)
            .get_overview(
                DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
                Utc::now() + Duration::days(1),
            )
            .unwrap();
        assert_eq!(overview.total_tokens, 183);
        let _ = fs::remove_dir_all(home);
    }
}
