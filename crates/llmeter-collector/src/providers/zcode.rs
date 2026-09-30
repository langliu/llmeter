use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use llmeter_core::{
    Provider, ProviderDetection, ProviderStatus, SourceFile, SourceFormat, TokenCounts,
};
use rusqlite::Connection;

use super::{ParsedUsage, ProviderAdapter, data_status, home_dir, project_name};
use crate::sqlite::{open_read_only, table_has_columns};

const ZCODE_PARSER_VERSION: u32 = 1;

const REQUIRED_USAGE_COLUMNS: &[&str] = &[
    "id",
    "session_id",
    "model_id",
    "status",
    "started_at",
    "input_tokens",
    "output_tokens",
    "reasoning_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
    "computed_total_tokens",
];

#[derive(Clone, Debug)]
pub struct ZCodeAdapter {
    root: PathBuf,
}

impl Default for ZCodeAdapter {
    fn default() -> Self {
        let root = std::env::var_os("ZCODE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".zcode"));
        Self { root }
    }
}

impl ZCodeAdapter {
    pub fn with_home(home: PathBuf) -> Self {
        Self {
            root: home.join(".zcode"),
        }
    }

    fn database_path(&self) -> PathBuf {
        self.root.join("cli").join("db").join("db.sqlite")
    }
}

/// Schema support flags for the ZCode database. The real tables carry more
/// columns than REQUIRED_USAGE_COLUMNS names; support only requires that every
/// required column exists, not an exact match.
#[derive(Debug, Eq, PartialEq)]
struct UsageSchema {
    /// `model_usage` exists with every required usage column.
    model_usage_supported: bool,
    /// `session` exists with the id + directory columns the project join needs.
    session_join_supported: bool,
}

fn inspect_schema(connection: &Connection) -> Result<UsageSchema> {
    Ok(UsageSchema {
        model_usage_supported: table_has_columns(
            connection,
            "model_usage",
            REQUIRED_USAGE_COLUMNS,
        )?,
        session_join_supported: table_has_columns(connection, "session", &["id", "directory"])?,
    })
}

impl ProviderAdapter for ZCodeAdapter {
    fn provider(&self) -> Provider {
        Provider::ZCode
    }

    fn parser_version(&self) -> u32 {
        ZCODE_PARSER_VERSION
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn detect(&self) -> Result<ProviderDetection> {
        let roots = vec![self.root.clone()];
        let database = self.database_path();
        if !database.is_file() {
            return Ok(data_status(Provider::ZCode, roots, false, None));
        }
        let connection = open_read_only(&database)?;
        let schema = inspect_schema(&connection)?;
        if schema.model_usage_supported {
            Ok(data_status(
                Provider::ZCode,
                roots,
                true,
                Some(format!(
                    "Token usage table supported: model_usage in {}",
                    database.display()
                )),
            ))
        } else {
            Ok(ProviderDetection {
                provider: Provider::ZCode,
                status: ProviderStatus::UnsupportedVersion,
                roots,
                detail: Some(format!(
                    "ZCode database detected but no supported model_usage schema was found: {}",
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
        if !inspect_schema(&connection)?.model_usage_supported {
            return Ok(Vec::new());
        }
        Ok(vec![SourceFile {
            path: database,
            provider: Provider::ZCode,
            format: SourceFormat::Sqlite,
            session_id: None,
            project_path: None,
            project_name: None,
        }])
    }

    fn parse_line(&self, _source: &SourceFile, _line: &[u8]) -> Result<Option<ParsedUsage>> {
        Ok(None)
    }

    fn parse_sqlite(&self, source: &SourceFile) -> Result<Vec<ParsedUsage>> {
        let connection = open_read_only(&source.path)?;
        let schema = inspect_schema(&connection)?;
        if !schema.model_usage_supported {
            anyhow::bail!("ZCode model_usage schema is unsupported");
        }
        let directory_expression = if schema.session_join_supported {
            "s.directory"
        } else {
            "NULL"
        };
        let join = if schema.session_join_supported {
            "LEFT JOIN session s ON s.id = u.session_id"
        } else {
            ""
        };
        let query = format!(
            "SELECT u.id, u.session_id, u.model_id, u.status, u.started_at,
                    u.input_tokens, u.output_tokens, u.reasoning_tokens,
                    u.cache_creation_input_tokens, u.cache_read_input_tokens,
                    u.computed_total_tokens, {directory_expression}
             FROM model_usage u
             {join}
             WHERE u.status <> 'running'
             ORDER BY u.started_at, u.id"
        );
        let mut statement = connection.prepare(&query)?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, Option<String>>(11)?,
            ))
        })?;
        let mut usages = Vec::new();
        for row in rows {
            let (
                id,
                session_id,
                model,
                started_at,
                input,
                output,
                reasoning,
                cache_creation,
                cache_read,
                total,
                directory,
            ) = row?;
            let (input, output, reasoning, cache_creation, cache_read) = (
                non_negative(input),
                non_negative(output),
                non_negative(reasoning),
                non_negative(cache_creation),
                non_negative(cache_read),
            );
            // ZCode's inputTokens already includes cached reads and its
            // computedTotalTokens is the authoritative input + output sum.
            if input == 0 && output == 0 && reasoning == 0 && cache_creation == 0 && cache_read == 0
            {
                continue;
            }
            let total = if total > 0 {
                non_negative(total)
            } else {
                input.saturating_add(output)
            };
            // An out-of-range started_at would drop the row and silently
            // undercount; stamping such rows with the sync time keeps their
            // tokens attributable.
            let timestamp =
                DateTime::<Utc>::from_timestamp_millis(started_at).unwrap_or_else(Utc::now);
            let project_path = directory.map(PathBuf::from);
            usages.push(ParsedUsage {
                counts: TokenCounts {
                    input_tokens: input,
                    cached_input_tokens: cache_read,
                    cache_creation_input_tokens: cache_creation,
                    output_tokens: output,
                    reasoning_tokens: reasoning,
                    total_tokens: total,
                },
                cumulative_snapshot: None,
                timestamp,
                model: model.filter(|model| !model.trim().is_empty()),
                session_id,
                project_name: project_name(project_path.as_deref()),
                project_path,
                source_event_id: Some(id),
                reported_cost_usd: None,
            });
        }
        Ok(usages)
    }
}

fn non_negative(value: i64) -> u64 {
    value.max(0) as u64
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use super::*;

    fn create_database(home: &Path) -> PathBuf {
        let database = home.join(".zcode").join("cli").join("db");
        fs::create_dir_all(&database).unwrap();
        let path = database.join("db.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session (
                    id TEXT PRIMARY KEY,
                    directory TEXT NOT NULL,
                    title TEXT NOT NULL
                );
                CREATE TABLE model_usage (
                    id TEXT PRIMARY KEY,
                    logical_request_id TEXT NOT NULL DEFAULT '',
                    session_id TEXT NOT NULL,
                    turn_id TEXT,
                    model_id TEXT NOT NULL,
                    provider_id TEXT NOT NULL DEFAULT '',
                    query_source TEXT NOT NULL DEFAULT '',
                    status TEXT NOT NULL,
                    started_at INTEGER NOT NULL,
                    completed_at INTEGER,
                    duration_ms INTEGER,
                    input_tokens INTEGER NOT NULL DEFAULT 0,
                    output_tokens INTEGER NOT NULL DEFAULT 0,
                    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
                    computed_total_tokens INTEGER NOT NULL DEFAULT 0,
                    raw_usage_json TEXT
                );",
            )
            .unwrap();
        connection
            .execute_batch(
                "INSERT INTO session VALUES
                    ('sess_main', '/tmp/llmeter-project', 'Main session'),
                    ('sess_sub', '/tmp/llmeter-project/subagent', 'Subagent');
                INSERT INTO model_usage
                    (id, session_id, model_id, status, started_at,
                     input_tokens, output_tokens, reasoning_tokens,
                     cache_creation_input_tokens, cache_read_input_tokens,
                     computed_total_tokens)
                VALUES
                    ('usage_1', 'sess_main', 'GLM-5.3', 'completed', 1790773590614,
                     25531, 79, 0, 0, 19456, 25610),
                    ('usage_2', 'sess_main', 'GLM-5.3', 'running', 1790773591000,
                     500, 10, 0, 0, 400, 510),
                    ('usage_3', 'sess_main', 'GLM-5.3', 'cancelled', 1790773592000,
                     0, 0, 0, 0, 0, 0),
                    ('usage_4', 'sess_sub', 'GLM-4.6', 'completed', 1790773600000,
                     100, 20, 0, 0, 60, 120);",
            )
            .unwrap();
        drop(connection);
        path
    }

    #[test]
    fn parses_completed_usage_rows_with_project_metadata() {
        let home = std::env::temp_dir().join(format!("llmeter-zcode-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        create_database(&home);

        let adapter = ZCodeAdapter::with_home(home.clone());
        let source = adapter.discover_sources().unwrap().remove(0);
        assert_eq!(source.format, SourceFormat::Sqlite);
        assert_eq!(source.provider, Provider::ZCode);

        let parsed = adapter.parse_sqlite(&source).unwrap();
        // The running row is still mutable and the cancelled row carries no
        // usage, so only the completed model requests are reported.
        assert_eq!(parsed.len(), 2);
        let main = &parsed[0];
        assert_eq!(main.source_event_id.as_deref(), Some("usage_1"));
        assert_eq!(main.session_id.as_deref(), Some("sess_main"));
        assert_eq!(main.model.as_deref(), Some("GLM-5.3"));
        assert_eq!(main.counts.input_tokens, 25531);
        assert_eq!(main.counts.cached_input_tokens, 19456);
        assert_eq!(main.counts.output_tokens, 79);
        assert_eq!(main.counts.total_tokens, 25610);
        assert_eq!(
            main.project_path.as_deref(),
            Some(Path::new("/tmp/llmeter-project"))
        );
        assert_eq!(main.project_name.as_deref(), Some("llmeter-project"));
        assert_eq!(
            main.timestamp,
            DateTime::<Utc>::from_timestamp_millis(1790773590614).unwrap()
        );
        let subagent = &parsed[1];
        assert_eq!(subagent.session_id.as_deref(), Some("sess_sub"));
        assert_eq!(subagent.counts.total_tokens, 120);
        assert_eq!(subagent.project_name.as_deref(), Some("subagent"));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn detect_reports_data_found_and_unsupported_schemas() {
        let home =
            std::env::temp_dir().join(format!("llmeter-zcode-detect-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let adapter = ZCodeAdapter::with_home(home.clone());
        assert_eq!(
            adapter.detect().unwrap().status,
            ProviderStatus::NotInstalled
        );

        create_database(&home);
        let detection = adapter.detect().unwrap();
        assert_eq!(detection.status, ProviderStatus::DataFound);
        assert_eq!(adapter.discover_sources().unwrap().len(), 1);

        let database = adapter.database_path();
        fs::remove_file(&database).unwrap();
        let connection = Connection::open(&database).unwrap();
        connection
            .execute("CREATE TABLE session (id TEXT PRIMARY KEY)", [])
            .unwrap();
        drop(connection);
        assert_eq!(
            adapter.detect().unwrap().status,
            ProviderStatus::UnsupportedVersion
        );
        assert!(adapter.discover_sources().unwrap().is_empty());
        let _ = fs::remove_dir_all(home);
    }
}
