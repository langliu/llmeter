use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use llmeter_core::{
    Provider, ProviderDetection, ProviderStatus, SourceFile, SourceFormat, TokenCounts,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use super::{ParsedUsage, ProviderAdapter, deduplicate_paths, home_dir, project_name};

const QODER_PARSER_VERSION: u32 = 1;
const QODER_USAGE_SQL: &str = "SELECT cm.rowid, cm.id, cm.session_id, cm.request_id, cm.token_info,
            cm.model_info, cm.gmt_create, cr.extra, cs.preferred_model_info,
            cs.project_uri, cs.project_name
     FROM chat_message cm
     LEFT JOIN chat_record cr ON cr.request_id = cm.request_id
     LEFT JOIN chat_session cs ON cs.session_id = cm.session_id
     WHERE cm.role = 'assistant' AND cm.token_info IS NOT NULL
       AND trim(cm.token_info) NOT IN ('', '{}')
     ORDER BY cm.gmt_create, cm.rowid";

#[derive(Clone, Debug)]
pub struct QoderAdapter {
    databases: Vec<PathBuf>,
}

impl Default for QoderAdapter {
    fn default() -> Self {
        let home = home_dir();
        let databases = [false, true]
            .map(|china| qoder_database_path(&home, std::env::consts::OS, china))
            .into_iter()
            .collect();
        Self { databases }
    }
}

pub(crate) fn qoder_root(home: &Path, platform: &str, china: bool) -> PathBuf {
    let (directory, env_prefix) = if china {
        ("QoderCN", "QODER_CN")
    } else {
        ("Qoder", "QODER")
    };
    std::env::var_os(format!("{env_prefix}_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| match platform {
            "macos" => home.join("Library/Application Support").join(directory),
            "windows" => std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData/Roaming"))
                .join(directory),
            _ => home.join(".config").join(directory),
        })
}

fn qoder_database_path(home: &Path, platform: &str, china: bool) -> PathBuf {
    let env_prefix = if china { "QODER_CN" } else { "QODER" };
    std::env::var_os(format!("{env_prefix}_DB_PATH"))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            qoder_root(home, platform, china).join("SharedClientCache/cache/db/local.db")
        })
}

impl QoderAdapter {
    pub fn with_databases(databases: Vec<PathBuf>) -> Self {
        Self { databases }
    }

    fn existing_databases(&self) -> Vec<PathBuf> {
        deduplicate_paths(self.databases.iter().filter(|path| path.is_file()).cloned())
    }
}

impl ProviderAdapter for QoderAdapter {
    fn provider(&self) -> Provider {
        Provider::Qoder
    }

    fn parser_version(&self) -> u32 {
        QODER_PARSER_VERSION
    }

    fn detect(&self) -> Result<ProviderDetection> {
        let databases = self.existing_databases();
        let roots = self.databases.clone();
        if databases.is_empty() {
            return Ok(ProviderDetection {
                provider: Provider::Qoder,
                status: if roots
                    .iter()
                    .any(|path| path.ancestors().nth(4).is_some_and(std::path::Path::exists))
                {
                    ProviderStatus::Installed
                } else {
                    ProviderStatus::NotInstalled
                },
                roots,
                detail: None,
            });
        }
        if let Some(path) = databases
            .iter()
            .find(|path| !supported_schema(path).unwrap_or(false))
        {
            return Ok(ProviderDetection {
                provider: Provider::Qoder,
                status: ProviderStatus::UnsupportedVersion,
                roots,
                detail: Some(format!(
                    "Unsupported Qoder database schema: {}",
                    path.display()
                )),
            });
        }
        let has_data = databases
            .iter()
            .any(|path| usage_count(path).unwrap_or(0) > 0);
        Ok(ProviderDetection {
            provider: Provider::Qoder,
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
            .filter(|path| supported_schema(path).unwrap_or(false))
            .map(|path| {
                Ok(SourceFile {
                    path,
                    provider: Provider::Qoder,
                    format: SourceFormat::Sqlite,
                    session_id: None,
                    project_path: None,
                    project_name: None,
                })
            })
            .collect()
    }

    fn parse_line(&self, _source: &SourceFile, _line: &[u8]) -> Result<Option<ParsedUsage>> {
        Ok(None)
    }

    fn parse_sqlite(&self, source: &SourceFile) -> Result<Vec<ParsedUsage>> {
        let connection =
            Connection::open_with_flags(&source.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .with_context(|| format!("open Qoder database {}", source.path.display()))?;
        let mut statement = connection.prepare(QODER_USAGE_SQL)?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
            ))
        })?;
        let mut parsed = Vec::new();
        for row in rows {
            let (
                row_id,
                id,
                session_id,
                request_id,
                token_info,
                model_info,
                created_at,
                record_extra,
                preferred,
                project_uri,
                project_label,
            ) = row?;
            let Some(counts) = qoder_counts(&token_info) else {
                continue;
            };
            let model = qoder_model(
                model_info.as_deref(),
                record_extra.as_deref(),
                preferred.as_deref(),
            );
            let project_path = project_uri.as_deref().and_then(file_uri_path);
            let timestamp = if created_at > 10_000_000_000 {
                Utc.timestamp_millis_opt(created_at)
                    .single()
                    .unwrap_or_else(Utc::now)
            } else {
                Utc.timestamp_opt(created_at, 0)
                    .single()
                    .unwrap_or_else(Utc::now)
            };
            let event_key = id.clone().unwrap_or_else(|| format!("row:{row_id}"));
            parsed.push(ParsedUsage {
                counts,
                cumulative_snapshot: None,
                timestamp,
                model: Some(model),
                session_id: session_id.clone(),
                project_name: project_label
                    .filter(|value| !value.trim().is_empty())
                    .or_else(|| project_name(project_path.as_deref())),
                project_path,
                source_event_id: Some(format!(
                    "{}:{}:{}",
                    blake3::hash(source.path.to_string_lossy().as_bytes()).to_hex(),
                    request_id.or(session_id).unwrap_or_default(),
                    event_key
                )),
                reported_cost_usd: None,
            });
        }
        Ok(parsed)
    }
}

fn supported_schema(path: &Path) -> Result<bool> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(connection.prepare(QODER_USAGE_SQL).is_ok())
}

fn usage_count(path: &Path) -> Result<u64> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let count = connection.query_row(
        "SELECT COUNT(*) FROM chat_message WHERE role='assistant' AND token_info IS NOT NULL AND trim(token_info) NOT IN ('', '{}')",
        [], |row| row.get::<_, i64>(0),
    )?;
    Ok(u64::try_from(count).unwrap_or_default())
}

fn qoder_counts(raw: &str) -> Option<TokenCounts> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let prompt = value.get("prompt_tokens")?.as_u64()?;
    let cached = value
        .get("cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default()
        .min(prompt);
    let output = value.get("completion_tokens")?.as_u64()?;
    Some(TokenCounts {
        input_tokens: prompt.saturating_sub(cached),
        cached_input_tokens: cached,
        cache_creation_input_tokens: 0,
        output_tokens: output,
        reasoning_tokens: 0,
        total_tokens: prompt.saturating_add(output),
    })
}

fn qoder_model(
    model_info: Option<&str>,
    record_extra: Option<&str>,
    preferred: Option<&str>,
) -> String {
    let values = [model_info, record_extra, preferred]
        .map(|raw| raw.and_then(|raw| serde_json::from_str::<Value>(raw).ok()));
    values[0]
        .as_ref()
        .and_then(|v| v.get("model_key").or_else(|| v.get("modelKey")))
        .and_then(Value::as_str)
        .or_else(|| {
            values[1]
                .as_ref()
                .and_then(|v| {
                    v.pointer("/modelConfig/key")
                        .or_else(|| v.pointer("/model_config/key"))
                })
                .and_then(Value::as_str)
        })
        .or_else(|| {
            values[2]
                .as_ref()
                .and_then(|v| {
                    v.get("model_key")
                        .or_else(|| v.get("modelKey"))
                        .or_else(|| v.get("preferred_model"))
                        .or_else(|| v.get("preferredModel"))
                })
                .and_then(Value::as_str)
        })
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("qoder-agent")
        .to_string()
}

fn file_uri_path(raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(path) = raw.strip_prefix("file://") {
        return Some(PathBuf::from(percent_decode(path)));
    }
    Some(PathBuf::from(raw))
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2]))
        {
            output.push(high * 16 + low);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn separates_cached_input_from_qoder_prompt_total() {
        let counts = qoder_counts(
            r#"{"prompt_tokens":58299,"cached_tokens":57853,"completion_tokens":2812}"#,
        )
        .unwrap();
        assert_eq!(counts.input_tokens, 446);
        assert_eq!(counts.cached_input_tokens, 57_853);
        assert_eq!(counts.total_tokens, 61_111);
    }

    #[test]
    fn extracts_qoder_model_from_fallback_shapes() {
        assert_eq!(
            qoder_model(Some("{}"), Some(r#"{"modelConfig":{"key":"quest"}}"#), None),
            "quest"
        );
    }

    #[test]
    fn parses_qoder_sqlite_usage_without_double_counting_cache() {
        let directory = std::env::temp_dir().join(format!(
            "llmeter-qoder-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let database = directory.join("local.db");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE chat_message (
                    id TEXT, session_id TEXT, request_id TEXT, role TEXT,
                    token_info TEXT, model_info TEXT, gmt_create INTEGER
                );
                CREATE TABLE chat_record (request_id TEXT, extra TEXT);
                CREATE TABLE chat_session (
                    session_id TEXT, preferred_model_info TEXT,
                    project_uri TEXT, project_name TEXT
                );
                INSERT INTO chat_message VALUES (
                    'message-1', 'session-1', 'request-1', 'assistant',
                    '{"prompt_tokens":100,"cached_tokens":80,"completion_tokens":10}',
                    '{"model_key":"quest-ultimate"}', 1784681696263
                );
                INSERT INTO chat_record VALUES ('request-1', '{}');
                INSERT INTO chat_session VALUES (
                    'session-1', '{}', 'file:///tmp/qoder%20project', 'Qoder Project'
                );
                "#,
            )
            .unwrap();
        drop(connection);
        let adapter = QoderAdapter::with_databases(vec![database.clone()]);
        let source = adapter.discover_sources().unwrap().remove(0);

        let rows = adapter.parse_sqlite(&source).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].counts.input_tokens, 20);
        assert_eq!(rows[0].counts.cached_input_tokens, 80);
        assert_eq!(rows[0].counts.total_tokens, 110);
        assert_eq!(rows[0].model.as_deref(), Some("quest-ultimate"));
        assert_eq!(
            rows[0].project_path.as_deref(),
            Some(Path::new("/tmp/qoder project"))
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
