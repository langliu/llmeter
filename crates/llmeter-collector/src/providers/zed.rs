use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use llmeter_core::{
    Provider, ProviderDetection, ProviderStatus, SourceFile, SourceFormat, TokenCounts,
    parse_timestamp,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use super::{ParsedUsage, ProviderAdapter, home_dir, project_name};

const ZED_PARSER_VERSION: u32 = 1;
const REQUIRED_COLUMNS: &[&str] = &["id", "updated_at", "data_type", "data", "folder_paths"];

#[derive(Clone, Debug)]
pub struct ZedAdapter {
    databases: Vec<PathBuf>,
}

impl Default for ZedAdapter {
    fn default() -> Self {
        let home = home_dir();
        Self {
            databases: vec![
                home.join("Library")
                    .join("Application Support")
                    .join("Zed")
                    .join("threads")
                    .join("threads.db"),
                home.join(".local")
                    .join("share")
                    .join("zed")
                    .join("threads")
                    .join("threads.db"),
            ],
        }
    }
}

impl ZedAdapter {
    pub fn with_database(path: PathBuf) -> Self {
        Self {
            databases: vec![path],
        }
    }

    fn existing_databases(&self) -> Vec<PathBuf> {
        self.databases
            .iter()
            .filter(|path| path.is_file())
            .cloned()
            .collect()
    }
}

impl ProviderAdapter for ZedAdapter {
    fn provider(&self) -> Provider {
        Provider::Zed
    }

    fn parser_version(&self) -> u32 {
        ZED_PARSER_VERSION
    }

    fn detect(&self) -> Result<ProviderDetection> {
        let roots = self
            .databases
            .iter()
            .flat_map(|path| [path.parent().map(Path::to_path_buf), Some(path.clone())])
            .flatten()
            .collect::<Vec<_>>();
        let databases = self.existing_databases();
        if databases.is_empty() {
            let status = if roots.iter().any(|root| root.exists()) {
                ProviderStatus::Installed
            } else {
                ProviderStatus::NotInstalled
            };
            return Ok(ProviderDetection {
                provider: Provider::Zed,
                status,
                roots,
                detail: None,
            });
        }

        for path in &databases {
            if !supported_schema(path)? {
                return Ok(ProviderDetection {
                    provider: Provider::Zed,
                    status: ProviderStatus::UnsupportedVersion,
                    roots,
                    detail: Some(format!(
                        "Zed threads database has an unsupported schema: {}",
                        path.display()
                    )),
                });
            }
        }
        let has_data = databases
            .iter()
            .map(|path| supported_thread_count(path))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .any(|count| count > 0);
        Ok(ProviderDetection {
            provider: Provider::Zed,
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
        self.existing_databases()
            .into_iter()
            .filter_map(|path| match supported_schema(&path) {
                Ok(true) => Some(Ok(SourceFile {
                    path,
                    provider: Provider::Zed,
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
                .with_context(|| format!("open Zed threads database {}", source.path.display()))?;
        let mut statement = connection.prepare(
            "SELECT id, updated_at, data_type, data, folder_paths
             FROM threads ORDER BY updated_at, id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;

        let mut parsed = Vec::new();
        for row in rows {
            let (thread_id, updated_at, data_type, data, folder_paths) = row?;
            let json = match data_type.as_str() {
                "json" => data,
                "zstd" => zstd::decode_all(data.as_slice())
                    .with_context(|| format!("decompress Zed thread {thread_id}"))?,
                other => anyhow::bail!("unsupported Zed thread data type: {other}"),
            };
            let thread: Value = serde_json::from_slice(&json)
                .with_context(|| format!("decode Zed thread {thread_id}"))?;
            let model = thread
                .get("model")
                .and_then(|value| value.get("model"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let project_path = folder_paths.as_deref().and_then(first_folder_path);
            let timestamp_value = Value::String(updated_at);
            let timestamp = parse_timestamp(Some(&timestamp_value));

            let mut requests = request_usages(&thread);
            if requests.is_empty()
                && let Some(counts) = thread.get("cumulative_token_usage").and_then(zed_counts)
            {
                requests.push(("cumulative".into(), counts));
            }
            for (request_id, counts) in requests {
                parsed.push(ParsedUsage {
                    counts,
                    cumulative_snapshot: None,
                    timestamp,
                    model: model.clone(),
                    session_id: Some(thread_id.clone()),
                    project_name: project_name(project_path.as_deref()),
                    project_path: project_path.clone(),
                    source_event_id: Some(format!("thread:{thread_id}:request:{request_id}")),
                    reported_cost_usd: None,
                });
            }
        }
        Ok(parsed)
    }
}

fn supported_schema(path: &Path) -> Result<bool> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let exists = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'threads'",
            [],
            |_| Ok(()),
        )
        .is_ok();
    if !exists {
        return Ok(false);
    }
    let mut statement = connection.prepare("PRAGMA table_info('threads')")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(REQUIRED_COLUMNS
        .iter()
        .all(|required| columns.iter().any(|column| column == required)))
}

fn supported_thread_count(path: &Path) -> Result<u64> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let count = connection.query_row(
        "SELECT count(*) FROM threads WHERE data_type IN ('json', 'zstd')",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(count.max(0) as u64)
}

fn request_usages(thread: &Value) -> Vec<(String, TokenCounts)> {
    let Some(value) = thread.get("request_token_usage") else {
        return Vec::new();
    };
    let mut requests = match value {
        Value::Object(entries) => entries
            .iter()
            .filter_map(|(id, usage)| zed_counts(usage).map(|counts| (id.clone(), counts)))
            .collect::<Vec<_>>(),
        Value::Array(entries) => entries
            .iter()
            .enumerate()
            .filter_map(|(index, usage)| {
                zed_counts(usage).map(|counts| (index.to_string(), counts))
            })
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    requests.sort_by(|left, right| left.0.cmp(&right.0));
    requests
}

fn zed_counts(value: &Value) -> Option<TokenCounts> {
    let input_tokens = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let output_tokens = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let cache_creation_input_tokens = value
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let cached_input_tokens = value
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let counts = TokenCounts {
        input_tokens,
        cached_input_tokens,
        cache_creation_input_tokens,
        output_tokens,
        reasoning_tokens: 0,
        total_tokens: input_tokens
            .saturating_add(cached_input_tokens)
            .saturating_add(cache_creation_input_tokens)
            .saturating_add(output_tokens),
    };
    (!counts.is_zero()).then_some(counts)
}

fn first_folder_path(value: &str) -> Option<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.starts_with('/') || value.starts_with('~') {
        return Some(PathBuf::from(value));
    }
    serde_json::from_str::<Value>(value)
        .ok()
        .and_then(|value| first_string(&value).map(PathBuf::from))
}

fn first_string(value: &Value) -> Option<&str> {
    match value {
        Value::String(value) => Some(value),
        Value::Array(values) => values.iter().find_map(first_string),
        Value::Object(values) => values.values().find_map(first_string),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::{DateTime, Utc};
    use llmeter_storage::{Database, UsageRepository};
    use rusqlite::{Connection, params};

    use super::*;
    use crate::sync::SyncEngine;

    #[test]
    fn parses_zstd_thread_requests_and_syncs_idempotently() {
        let root = std::env::temp_dir().join(format!("llmeter-zed-adapter-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("threads.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    summary TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    data_type TEXT NOT NULL,
                    data BLOB NOT NULL,
                    folder_paths TEXT
                );",
            )
            .unwrap();
        let thread = br#"{
            "updated_at":"2026-08-20T03:00:00Z",
            "model":{"provider":"zed.dev","model":"gpt-5.4"},
            "request_token_usage":{
                "request-1":{"input_tokens":10,"output_tokens":3,"cache_read_input_tokens":5},
                "request-2":{"input_tokens":7,"output_tokens":2,"cache_creation_input_tokens":4}
            }
        }"#;
        let compressed = zstd::encode_all(thread.as_slice(), 3).unwrap();
        connection
            .execute(
                "INSERT INTO threads VALUES (?1, ?2, ?3, 'zstd', ?4, ?5)",
                params![
                    "thread-1",
                    "summary",
                    "2026-08-20T03:00:00Z",
                    compressed,
                    "/tmp/project"
                ],
            )
            .unwrap();
        drop(connection);

        let database = Database::open_in_memory().unwrap();
        let engine = SyncEngine::with_adapters(
            database.clone(),
            vec![Box::new(ZedAdapter::with_database(path))],
        );
        let first = engine.sync_all().unwrap();
        assert_eq!(first.events_inserted, 2);
        assert_eq!(first.tokens_added, 31);
        let second = engine.sync_all().unwrap();
        assert_eq!(second.events_inserted, 0);
        assert_eq!(second.tokens_added, 0);

        let overview = UsageRepository::new(database)
            .get_overview(
                DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
                Utc::now() + chrono::Duration::days(1),
            )
            .unwrap();
        assert_eq!(overview.total_tokens, 31);
        let _ = fs::remove_dir_all(root);
    }
}
