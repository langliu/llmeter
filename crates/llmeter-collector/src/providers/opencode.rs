use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use chrono::{DateTime, Utc};
use llmeter_core::{Provider, ProviderDetection, SourceFile, SourceFormat, TokenCounts};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::Value;

use super::{
    ParsedUsage, ProviderAdapter, counts_from_usage, data_status, deduplicate_paths, home_dir,
    json_value, model, object_for_key, object_with_usage, project_name, project_path, session_id,
    source_event_id, timestamp, walk_jsonl,
};

const OPENCODE_PARSER_VERSION: u32 = 2;
const SESSION_TABLES: &[&str] = &["session_v2", "session"];
const REQUIRED_SESSION_COLUMNS: &[&str] = &[
    "id",
    "directory",
    "time_created",
    "time_updated",
    "model",
    "tokens_input",
    "tokens_output",
    "tokens_reasoning",
    "tokens_cache_read",
    "tokens_cache_write",
];

#[derive(Clone, Debug)]
pub struct OpenCodeAdapter {
    home: PathBuf,
}

impl Default for OpenCodeAdapter {
    fn default() -> Self {
        Self { home: home_dir() }
    }
}

impl OpenCodeAdapter {
    pub fn with_home(home: PathBuf) -> Self {
        Self { home }
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![
            self.home.join(".local").join("share").join("opencode"),
            self.home.join(".config").join("opencode"),
            self.home
                .join("Library")
                .join("Application Support")
                .join("opencode"),
            self.home.join(".opencode"),
        ]
    }

    fn jsonl_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for root in self.roots() {
            files.extend(walk_jsonl(&root)?);
        }
        Ok(deduplicate_paths(files))
    }

    fn sqlite_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for root in self.roots() {
            collect_extensions(&root, &mut files)?;
        }
        Ok(deduplicate_paths(files))
    }

    fn supported_sqlite_source(&self) -> Result<Option<(PathBuf, String)>> {
        for path in self.sqlite_files()? {
            if let Some(table) = find_session_table(&path)? {
                return Ok(Some((path, table)));
            }
        }
        Ok(None)
    }
}

impl ProviderAdapter for OpenCodeAdapter {
    fn provider(&self) -> Provider {
        Provider::OpenCode
    }

    fn parser_version(&self) -> u32 {
        OPENCODE_PARSER_VERSION
    }
    fn watch_roots(&self) -> Vec<PathBuf> {
        self.roots()
    }

    fn detect(&self) -> Result<ProviderDetection> {
        let roots = self.roots();
        let jsonl = self.jsonl_files()?;
        let sqlite = self.sqlite_files()?;
        if let Some((path, table)) = self.supported_sqlite_source()? {
            return Ok(data_status(
                Provider::OpenCode,
                roots,
                true,
                Some(format!(
                    "SQLite session table supported: {table} in {}",
                    path.display()
                )),
            ));
        }
        if !jsonl.is_empty() {
            return Ok(data_status(Provider::OpenCode, roots, true, None));
        }
        if let Some(path) = sqlite.first() {
            return Ok(ProviderDetection {
                provider: Provider::OpenCode,
                status: llmeter_core::ProviderStatus::UnsupportedVersion,
                roots,
                detail: Some(format!(
                    "SQLite storage detected but no supported session token schema was found: {}",
                    inspect_schema(path)?
                )),
            });
        }
        Ok(data_status(Provider::OpenCode, roots, false, None))
    }

    fn discover_sources(&self) -> Result<Vec<SourceFile>> {
        if let Some((path, _table)) = self.supported_sqlite_source()? {
            return Ok(vec![SourceFile {
                path,
                provider: Provider::OpenCode,
                format: SourceFormat::Sqlite,
                session_id: None,
                project_path: None,
                project_name: None,
            }]);
        }
        Ok(self
            .jsonl_files()?
            .into_iter()
            .map(|path| SourceFile {
                session_id: path
                    .file_stem()
                    .map(|value| value.to_string_lossy().to_string()),
                path,
                provider: Provider::OpenCode,
                format: SourceFormat::Jsonl,
                project_path: None,
                project_name: None,
            })
            .collect())
    }

    fn parse_line(&self, source: &SourceFile, line: &[u8]) -> Result<Option<ParsedUsage>> {
        let value = json_value(line)?;
        let usage = object_for_key(&value, "usage").or_else(|| object_with_usage(&value));
        let Some(usage) = usage else {
            return Ok(None);
        };
        let Some(counts) = counts_from_usage(usage, true) else {
            return Ok(None);
        };
        let project_path = project_path(&value);
        Ok(Some(ParsedUsage {
            counts,
            cumulative_snapshot: None,
            timestamp: timestamp(&value),
            model: opencode_model(&value),
            session_id: session_id(&value).or_else(|| source.session_id.clone()),
            project_name: project_name(project_path.as_deref())
                .or_else(|| source.project_name.clone()),
            project_path,
            source_event_id: source_event_id(&value),
            reported_cost_usd: None,
        }))
    }

    fn parse_sqlite(&self, source: &SourceFile) -> Result<Vec<ParsedUsage>> {
        let table = find_session_table(&source.path)?
            .ok_or_else(|| anyhow::anyhow!("OpenCode session token schema is unsupported"))?;
        let connection =
            Connection::open_with_flags(&source.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let query = format!(
            "SELECT id, directory, model, tokens_input, tokens_output, tokens_reasoning,
                    tokens_cache_read, tokens_cache_write, time_created, time_updated
             FROM {table} ORDER BY time_updated, id"
        );
        let mut statement = connection.prepare(&query)?;
        let rows = statement.query_map([], |row| {
            let session_id: String = row.get(0)?;
            let directory: String = row.get(1)?;
            let raw_model: Option<String> = row.get(2)?;
            let input_tokens = non_negative(row.get::<_, i64>(3)?);
            let output_tokens = non_negative(row.get::<_, i64>(4)?);
            let reasoning_tokens = non_negative(row.get::<_, i64>(5)?);
            let cached_input_tokens = non_negative(row.get::<_, i64>(6)?);
            let cache_creation_input_tokens = non_negative(row.get::<_, i64>(7)?);
            let created_at = non_negative(row.get::<_, i64>(8)?);
            let updated_at = non_negative(row.get::<_, i64>(9)?);
            let timestamp_millis = if updated_at > 0 {
                updated_at
            } else {
                created_at
            };
            let timestamp = i64::try_from(timestamp_millis)
                .ok()
                .and_then(DateTime::<Utc>::from_timestamp_millis)
                .unwrap_or_else(Utc::now);
            let counts = TokenCounts {
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
            };
            let project_path = PathBuf::from(directory);
            Ok(ParsedUsage {
                counts,
                cumulative_snapshot: None,
                timestamp,
                model: normalize_model_text(raw_model.as_deref()),
                session_id: Some(session_id.clone()),
                project_name: project_name(Some(&project_path)),
                project_path: Some(project_path),
                source_event_id: Some(format!("{table}:{session_id}")),
                reported_cost_usd: None,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

fn collect_extensions(root: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_file() {
        if root.extension().is_some_and(|extension| {
            extension == "db" || extension == "sqlite" || extension == "sqlite3"
        }) {
            files.push(root.to_path_buf());
        }
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(root)? {
            collect_extensions(&entry?.path(), files)?;
        }
    }
    Ok(())
}

fn find_session_table(path: &Path) -> Result<Option<String>> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    for table in SESSION_TABLES {
        let table_exists: Option<String> = connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![table],
                |row| row.get(0),
            )
            .ok();
        if table_exists.is_none() {
            continue;
        }
        let columns = table_columns(&connection, table)?;
        if REQUIRED_SESSION_COLUMNS
            .iter()
            .all(|required| columns.iter().any(|column| column == required))
        {
            return Ok(Some((*table).to_string()));
        }
    }
    Ok(None)
}

fn table_columns(connection: &Connection, table: &str) -> Result<Vec<String>> {
    let escaped = table.replace('\'', "''");
    let pragma = format!("PRAGMA table_info('{escaped}')");
    let mut statement = connection.prepare(&pragma)?;
    statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn inspect_schema(path: &Path) -> Result<String> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_master
         WHERE type IN ('table', 'view')
           AND (name LIKE '%session%' OR name LIKE '%message%')
         ORDER BY name LIMIT 32",
    )?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if names.is_empty() {
        Ok("no session/message tables".into())
    } else {
        Ok(format!("candidate tables: {}", names.join(", ")))
    }
}

fn opencode_model(value: &Value) -> Option<String> {
    object_for_key(value, "model")
        .and_then(|model| model.get("id").and_then(Value::as_str))
        .map(str::to_string)
        .or_else(|| model(value).and_then(|model| normalize_model_text(Some(&model))))
}

fn normalize_model_text(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    if value.starts_with('{') {
        return serde_json::from_str::<Value>(value)
            .ok()
            .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_string));
    }
    Some(value.to_string())
}

fn non_negative(value: i64) -> u64 {
    value.max(0) as u64
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::*;

    #[test]
    fn parses_jsonl_usage() {
        let adapter = OpenCodeAdapter::with_home(PathBuf::from("/tmp"));
        let source = SourceFile::new(PathBuf::from("/tmp/opencode.jsonl"), Provider::OpenCode);
        let line = include_bytes!("../../../../fixtures/opencode/basic.jsonl");
        let parsed = adapter.parse_line(&source, line).unwrap().unwrap();
        assert_eq!(parsed.counts.total_tokens, 60);
        assert_eq!(parsed.session_id.as_deref(), Some("opencode-session"));
        assert_eq!(parsed.model.as_deref(), Some("gpt-5.4"));
    }

    #[test]
    fn parses_supported_sqlite_session_snapshots() {
        let home =
            std::env::temp_dir().join(format!("llmeter-opencode-sqlite-{}", std::process::id()));
        let root = home.join(".local").join("share").join("opencode");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("opencode.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session_v2 (
                    id TEXT PRIMARY KEY,
                    directory TEXT NOT NULL,
                    model TEXT,
                    tokens_input INTEGER NOT NULL DEFAULT 0,
                    tokens_output INTEGER NOT NULL DEFAULT 0,
                    tokens_reasoning INTEGER NOT NULL DEFAULT 0,
                    tokens_cache_read INTEGER NOT NULL DEFAULT 0,
                    tokens_cache_write INTEGER NOT NULL DEFAULT 0,
                    time_created INTEGER NOT NULL,
                    time_updated INTEGER NOT NULL
                );
                INSERT INTO session_v2 VALUES
                    ('s1', '/tmp/project',
                     '{\"id\":\"gpt-5.6-sol\",\"providerID\":\"openai\",\"variant\":\"high\"}',
                     10, 3, 1, 5, 2, 1786700000000, 1786700001000);",
            )
            .unwrap();
        drop(connection);

        let adapter = OpenCodeAdapter::with_home(home.clone());
        let source = adapter.discover_sources().unwrap().remove(0);
        assert_eq!(source.format, SourceFormat::Sqlite);
        let parsed = adapter.parse_sqlite(&source).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].counts.total_tokens, 21);
        assert_eq!(parsed[0].session_id.as_deref(), Some("s1"));
        assert_eq!(parsed[0].model.as_deref(), Some("gpt-5.6-sol"));
        let _ = fs::remove_dir_all(home);
    }
}
