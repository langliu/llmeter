use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use llmeter_core::{
    Provider, ProviderDetection, ProviderStatus, SourceFile, SourceFormat, TokenCounts,
};
use rusqlite::Connection;
use serde_json::Value;

use super::{ParsedUsage, ProviderAdapter, data_status, home_dir, project_name};
use crate::sqlite::{open_read_only, table_has_columns};

const KILO_PARSER_VERSION: u32 = 1;

const PART_COLUMNS: &[&str] = &["id", "message_id", "session_id", "time_created", "data"];
const MESSAGE_COLUMNS: &[&str] = &["id", "session_id", "data"];
const SESSION_COLUMNS: &[&str] = &["id", "directory"];

#[derive(Clone, Debug)]
pub struct KiloAdapter {
    root: PathBuf,
}

impl Default for KiloAdapter {
    fn default() -> Self {
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".local").join("share"));
        Self {
            root: data_home.join("kilo"),
        }
    }
}

impl KiloAdapter {
    pub fn with_data_home(data_home: PathBuf) -> Self {
        Self {
            root: data_home.join("kilo"),
        }
    }

    fn database_path(&self) -> PathBuf {
        self.root.join("kilo.db")
    }
}

impl ProviderAdapter for KiloAdapter {
    fn provider(&self) -> Provider {
        Provider::Kilo
    }

    fn parser_version(&self) -> u32 {
        KILO_PARSER_VERSION
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn detect(&self) -> Result<ProviderDetection> {
        let roots = vec![self.root.clone()];
        let database = self.database_path();
        if !database.is_file() {
            return Ok(data_status(Provider::Kilo, roots, false, None));
        }
        let connection = open_read_only(&database)?;
        if supported_schema(&connection)? {
            Ok(data_status(
                Provider::Kilo,
                roots,
                true,
                Some(format!(
                    "step-finish usage parts supported: {}",
                    database.display()
                )),
            ))
        } else {
            Ok(ProviderDetection {
                provider: Provider::Kilo,
                status: ProviderStatus::UnsupportedVersion,
                roots,
                detail: Some(format!(
                    "Kilo database detected but no supported session schema was found: {}",
                    database.display()
                )),
            })
        }
    }

    fn sync_detection(&self) -> Result<Option<ProviderDetection>> {
        self.detect().map(Some)
    }

    fn discover_sources(&self) -> Result<Vec<SourceFile>> {
        let database = self.database_path();
        if !database.is_file() {
            return Ok(Vec::new());
        }
        let connection = open_read_only(&database)?;
        if !supported_schema(&connection)? {
            return Ok(Vec::new());
        }
        Ok(vec![SourceFile {
            path: database,
            provider: Provider::Kilo,
            format: SourceFormat::Sqlite,
            session_id: None,
            project_path: None,
            project_name: None,
        }])
    }

    fn parse_line(&self, _source: &SourceFile, _line: &[u8]) -> Result<Option<ParsedUsage>> {
        Ok(None)
    }

    /// Every `step-finish` part is one billed request. `tokens.input`
    /// excludes the cache buckets (verified against real databases), the
    /// owning message carries the model, and the session row the cwd.
    fn parse_sqlite(&self, source: &SourceFile) -> Result<Vec<ParsedUsage>> {
        let connection = open_read_only(&source.path)?;
        let mut statement = connection.prepare(
            "SELECT p.id, p.session_id, p.time_created, p.data, m.data, s.directory
             FROM part p
             LEFT JOIN message m ON m.id = p.message_id
             LEFT JOIN session s ON s.id = p.session_id
             ORDER BY p.time_created, p.id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut usages = Vec::new();
        for row in rows {
            let (id, session_id, time_created, part_data, message_data, directory) = row?;
            let Ok(part) = serde_json::from_str::<Value>(&part_data) else {
                continue;
            };
            if part.get("type").and_then(Value::as_str) != Some("step-finish") {
                continue;
            }
            let Some(tokens) = part.get("tokens") else {
                continue;
            };
            let message = message_data
                .as_deref()
                .and_then(|data| serde_json::from_str::<Value>(data).ok());
            let counts = kilo_counts(tokens);
            if counts.is_zero() {
                continue;
            }
            let project_path = directory.map(PathBuf::from).or_else(|| {
                message
                    .as_ref()
                    .and_then(|value| value.pointer("/path/cwd"))
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
            });
            let timestamp =
                DateTime::<Utc>::from_timestamp_millis(time_created).unwrap_or_else(Utc::now);
            usages.push(ParsedUsage {
                counts,
                cumulative_snapshot: None,
                timestamp,
                model: message
                    .as_ref()
                    .and_then(|value| value.get("modelID"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                session_id: Some(session_id),
                project_name: project_name(project_path.as_deref()),
                project_path,
                source_event_id: Some(id),
                reported_cost_usd: message
                    .as_ref()
                    .and_then(|value| value.get("cost"))
                    .and_then(Value::as_f64)
                    .filter(|cost| *cost > 0.0),
            });
        }
        Ok(usages)
    }
}

fn supported_schema(connection: &Connection) -> Result<bool> {
    Ok(table_has_columns(connection, "part", PART_COLUMNS)?
        && table_has_columns(connection, "message", MESSAGE_COLUMNS)?
        && table_has_columns(connection, "session", SESSION_COLUMNS)?)
}

fn kilo_counts(tokens: &Value) -> TokenCounts {
    let number = |key: &str| tokens.get(key).and_then(Value::as_u64).unwrap_or_default();
    let cache = |key: &str| {
        tokens
            .pointer("/cache")
            .and_then(|cache| cache.get(key))
            .and_then(Value::as_u64)
            .unwrap_or_default()
    };
    let input_tokens = number("input");
    let cached_input_tokens = cache("read");
    let cache_creation_input_tokens = cache("write");
    let output_tokens = number("output");
    let reasoning_tokens = number("reasoning");
    let total_tokens = number("total").max(
        input_tokens
            .saturating_add(cached_input_tokens)
            .saturating_add(cache_creation_input_tokens)
            .saturating_add(output_tokens)
            .saturating_add(reasoning_tokens),
    );
    TokenCounts {
        input_tokens,
        cached_input_tokens,
        cache_creation_input_tokens,
        output_tokens,
        reasoning_tokens,
        total_tokens,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use rusqlite::Connection;

    use super::*;

    fn create_database(home: &Path) -> PathBuf {
        let root = home.join("kilo");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("kilo.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session (
                    id TEXT PRIMARY KEY,
                    directory TEXT NOT NULL,
                    title TEXT NOT NULL
                );
                CREATE TABLE message (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    time_created INTEGER NOT NULL,
                    time_updated INTEGER NOT NULL,
                    data TEXT NOT NULL
                );
                CREATE TABLE part (
                    id TEXT PRIMARY KEY,
                    message_id TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    time_created INTEGER NOT NULL,
                    time_updated INTEGER NOT NULL,
                    data TEXT NOT NULL
                );
                INSERT INTO session VALUES
                    ('ses_main', '/tmp/kilo-project', 'main'),
                    ('ses_other', '/tmp/other', 'other');
                INSERT INTO message VALUES
                    ('msg_1', 'ses_main', 1774840068568, 1774840080216,
                     '{\"role\":\"assistant\",\"modelID\":\"xiaomi/mimo-v2-pro:free\",\"providerID\":\"kilo\",\"path\":{\"cwd\":\"/tmp/kilo-project\",\"root\":\"/tmp/kilo-project\"},\"cost\":0.0031,\"tokens\":{\"total\":26615,\"input\":286,\"output\":25,\"reasoning\":0,\"cache\":{\"read\":26304,\"write\":0}}}');
                INSERT INTO part VALUES
                    ('prt_1', 'msg_1', 'ses_main', 1774840068568, 1774840068568,
                     '{\"type\":\"step-finish\",\"reason\":\"tool-calls\",\"cost\":0.0031,\"tokens\":{\"total\":26615,\"input\":286,\"output\":25,\"reasoning\":0,\"cache\":{\"read\":26304,\"write\":0}}}'),
                    ('prt_2', 'msg_1', 'ses_main', 1774840080219, 1774840080219,
                     '{\"type\":\"text\",\"text\":\"hello\"}'),
                    ('prt_3', 'msg_1', 'ses_other', 1774840100000, 1774840100000,
                     '{\"type\":\"step-finish\",\"reason\":\"other\",\"tokens\":{\"total\":0,\"input\":0,\"output\":0}}');
                ",
            )
            .unwrap();
        drop(connection);
        path
    }

    #[test]
    fn reads_step_finish_parts_with_model_and_project() {
        let home = std::env::temp_dir().join(format!("llmeter-kilo-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        create_database(&home);

        let adapter = KiloAdapter::with_data_home(home.clone());
        assert_eq!(adapter.detect().unwrap().status, ProviderStatus::DataFound);
        let source = adapter.discover_sources().unwrap().remove(0);
        assert_eq!(source.format, SourceFormat::Sqlite);

        let parsed = adapter.parse_sqlite(&source).unwrap();
        // The text part and the zero-token step-finish are skipped.
        assert_eq!(parsed.len(), 1);
        let usage = &parsed[0];
        assert_eq!(usage.counts.input_tokens, 286);
        assert_eq!(usage.counts.cached_input_tokens, 26_304);
        assert_eq!(usage.counts.cache_creation_input_tokens, 0);
        assert_eq!(usage.counts.output_tokens, 25);
        assert_eq!(usage.counts.total_tokens, 26_615);
        assert_eq!(usage.model.as_deref(), Some("xiaomi/mimo-v2-pro:free"));
        assert_eq!(usage.session_id.as_deref(), Some("ses_main"));
        assert_eq!(usage.source_event_id.as_deref(), Some("prt_1"));
        assert_eq!(usage.reported_cost_usd, Some(0.0031));
        assert_eq!(
            usage.project_path.as_deref(),
            Some(Path::new("/tmp/kilo-project"))
        );
        assert_eq!(usage.project_name.as_deref(), Some("kilo-project"));
        assert_eq!(
            usage.timestamp,
            DateTime::<Utc>::from_timestamp_millis(1_774_840_068_568).unwrap()
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn foreign_schema_reports_unsupported_version() {
        let home = std::env::temp_dir().join(format!("llmeter-kilo-odd-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let root = home.join("kilo");
        fs::create_dir_all(&root).unwrap();
        let connection = Connection::open(root.join("kilo.db")).unwrap();
        connection
            .execute("CREATE TABLE part (id TEXT PRIMARY KEY)", [])
            .unwrap();
        drop(connection);
        let adapter = KiloAdapter::with_data_home(home.clone());
        assert_eq!(
            adapter.detect().unwrap().status,
            ProviderStatus::UnsupportedVersion
        );
        assert!(adapter.discover_sources().unwrap().is_empty());
        let _ = fs::remove_dir_all(home);
    }
}
