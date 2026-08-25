use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use chrono::{DateTime, Utc};
use llmeter_core::{
    Provider, ProviderDetection, ProviderStatus, SourceFile, SourceFormat, TokenCounts,
    parse_timestamp,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use super::{ParsedUsage, ProviderAdapter, home_dir, json_value, project_name};

const ZED_PARSER_VERSION: u32 = 6;
const REQUIRED_COLUMNS: &[&str] = &["id", "updated_at", "data_type", "data", "folder_paths"];
const TELEMETRY_USAGE_MARKER: &str = "Agent Thread Completion Usage Updated";

type PromptTimes = HashMap<String, Vec<DateTime<Utc>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct PromptLogFingerprint {
    path: PathBuf,
    identity: Option<String>,
    size: u64,
    modified_at: Option<i64>,
}

pub struct ZedAdapter {
    databases: Vec<PathBuf>,
    telemetry_logs: Vec<PathBuf>,
    prompt_times: Mutex<Option<Arc<PromptTimes>>>,
    prompt_logs: Mutex<Vec<PromptLogFingerprint>>,
    prompt_index: Mutex<HashMap<PathBuf, HashMap<String, usize>>>,
    prompt_stamps: Mutex<HashMap<(PathBuf, String, String), DateTime<Utc>>>,
    telemetry_ids: Mutex<Option<(Vec<PromptLogFingerprint>, HashSet<String>)>>,
}

impl Default for ZedAdapter {
    fn default() -> Self {
        let home = home_dir();
        Self::new(
            vec![
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
            vec![
                home.join("Library")
                    .join("Logs")
                    .join("Zed")
                    .join("telemetry.log"),
                home.join(".local")
                    .join("share")
                    .join("zed")
                    .join("logs")
                    .join("telemetry.log"),
            ],
        )
    }
}

impl ZedAdapter {
    fn new(databases: Vec<PathBuf>, telemetry_logs: Vec<PathBuf>) -> Self {
        Self {
            databases,
            telemetry_logs,
            prompt_times: Mutex::new(None),
            prompt_logs: Mutex::new(Vec::new()),
            prompt_index: Mutex::new(HashMap::new()),
            prompt_stamps: Mutex::new(HashMap::new()),
            telemetry_ids: Mutex::new(None),
        }
    }

    pub fn with_database(path: PathBuf) -> Self {
        Self::new(vec![path], Vec::new())
    }

    fn existing_databases(&self) -> Vec<PathBuf> {
        existing_files(&self.databases)
    }

    fn existing_telemetry(&self) -> Vec<PathBuf> {
        existing_files(&self.telemetry_logs)
    }

    fn debug_logs(&self) -> Vec<PathBuf> {
        existing_files(
            &self
                .telemetry_logs
                .iter()
                .filter_map(|path| path.parent().map(|parent| parent.join("Zed.log")))
                .collect::<Vec<_>>(),
        )
    }

    fn log_roots(&self) -> Vec<PathBuf> {
        self.telemetry_logs
            .iter()
            .filter_map(|path| path.parent().map(Path::to_path_buf))
            .collect()
    }

    fn telemetry_usage_thread_ids(&self) -> HashSet<String> {
        let logs = self.existing_telemetry();
        let fingerprint = prompt_log_fingerprints(&logs);
        if let Some((previous, ids)) = self
            .telemetry_ids
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            && previous == &fingerprint
        {
            return ids.clone();
        }
        let mut ids = HashSet::new();
        for path in logs {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            for line in text.lines() {
                if !line.contains(TELEMETRY_USAGE_MARKER) {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if let Some(thread_id) = value
                    .get("event_properties")
                    .and_then(|properties| properties.get("thread_id"))
                    .and_then(Value::as_str)
                {
                    ids.insert(thread_id.to_string());
                }
            }
        }
        *self
            .telemetry_ids
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some((fingerprint, ids.clone()));
        ids
    }

    fn refresh_prompt_times(&self) {
        let logs = self.debug_logs();
        let fingerprint = prompt_log_fingerprints(&logs);
        let previous = self
            .prompt_logs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let cached = self
            .prompt_times
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some();
        if cached && previous == fingerprint {
            return;
        }
        *self
            .prompt_times
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(Arc::new(all_prompt_request_times(&logs)));
        if prompt_logs_rewound(&previous, &fingerprint) {
            self.reset_prompt_indexes(None);
        }
        *self
            .prompt_logs
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = fingerprint;
    }

    fn prompt_times(&self) -> Arc<HashMap<String, Vec<DateTime<Utc>>>> {
        let mut cache = self
            .prompt_times
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if cache.is_none() {
            *cache = Some(Arc::new(all_prompt_request_times(&self.debug_logs())));
        }
        cache.clone().unwrap_or_default()
    }

    fn reset_prompt_indexes(&self, source: Option<&Path>) {
        match source {
            Some(source) => {
                self.prompt_index
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(source);
            }
            None => {
                self.prompt_index
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clear();
            }
        }
    }

    fn reset_prompt_stamps(&self, source: Option<&Path>) {
        match source {
            Some(source) => {
                self.prompt_stamps
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .retain(|(path, _, _), _| path != source);
            }
            None => {
                self.prompt_stamps
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clear();
            }
        }
    }

    fn reset_prompt_state(&self, source: Option<&Path>) {
        self.reset_prompt_indexes(source);
        self.reset_prompt_stamps(source);
    }

    fn next_prompt_time(&self, source: &Path, thread_id: &str) -> DateTime<Utc> {
        let times = self.prompt_times();
        let list = times.get(thread_id).map(Vec::as_slice).unwrap_or(&[]);
        let mut indexes = self
            .prompt_index
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let index = indexes
            .entry(source.to_path_buf())
            .or_default()
            .entry(thread_id.to_string())
            .or_insert(0);
        let timestamp = list
            .get(*index)
            .copied()
            .or_else(|| list.last().copied())
            .unwrap_or_else(Utc::now);
        if *index < list.len() {
            *index += 1;
        }
        timestamp
    }

    fn take_prompt_time(
        &self,
        source: &Path,
        thread_id: &str,
        prompt_id: Option<&str>,
    ) -> DateTime<Utc> {
        let Some(prompt_id) = prompt_id.filter(|value| !value.is_empty()) else {
            return self.next_prompt_time(source, thread_id);
        };
        let key = (
            source.to_path_buf(),
            thread_id.to_string(),
            prompt_id.to_string(),
        );
        if let Some(timestamp) = self
            .prompt_stamps
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .copied()
        {
            return timestamp;
        }
        let timestamp = self.next_prompt_time(source, thread_id);
        self.prompt_stamps
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(key, timestamp);
        timestamp
    }
}

fn existing_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|path| path.is_file())
        .cloned()
        .collect()
}

impl ProviderAdapter for ZedAdapter {
    fn provider(&self) -> Provider {
        Provider::Zed
    }

    fn parser_version(&self) -> u32 {
        ZED_PARSER_VERSION
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        self.databases
            .iter()
            .filter_map(|path| path.parent().map(Path::to_path_buf))
            .chain(self.log_roots())
            .collect()
    }

    fn detect(&self) -> Result<ProviderDetection> {
        let mut roots = self
            .databases
            .iter()
            .flat_map(|path| [path.parent().map(Path::to_path_buf), Some(path.clone())])
            .flatten()
            .collect::<Vec<_>>();
        roots.extend(self.log_roots());
        roots.extend(self.existing_telemetry());
        let databases = self.existing_databases();
        let telemetry = self.existing_telemetry();
        if databases.is_empty() && telemetry.is_empty() {
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
        let has_thread_data = databases
            .iter()
            .map(|path| supported_thread_count(path))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .any(|count| count > 0);
        let has_data = has_thread_data || !telemetry.is_empty();
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
        self.refresh_prompt_times();
        let mut sources = self
            .existing_telemetry()
            .into_iter()
            .map(|path| SourceFile {
                path,
                provider: Provider::Zed,
                format: SourceFormat::Jsonl,
                session_id: None,
                project_path: None,
                project_name: None,
            })
            .collect::<Vec<_>>();
        for path in self.existing_databases() {
            match supported_schema(&path) {
                Ok(true) => sources.push(SourceFile {
                    path,
                    provider: Provider::Zed,
                    format: SourceFormat::Sqlite,
                    session_id: None,
                    project_path: None,
                    project_name: None,
                }),
                Ok(false) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(sources)
    }

    fn begin_source(&self, source: &SourceFile, from_beginning: bool) {
        if from_beginning {
            self.reset_prompt_state(Some(&source.path));
        }
    }

    fn parse_line(&self, source: &SourceFile, line: &[u8]) -> Result<Option<ParsedUsage>> {
        let value = json_value(line)?;
        parse_telemetry_usage(&value, |thread_id, prompt_id| {
            self.take_prompt_time(&source.path, thread_id, prompt_id)
        })
    }

    fn parse_sqlite(&self, source: &SourceFile) -> Result<Vec<ParsedUsage>> {
        let skip_threads = self.telemetry_usage_thread_ids();
        let prompt_times = self.prompt_times();
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
            if skip_threads.contains(&thread_id) {
                continue;
            }
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
            let fallback = parse_timestamp(Some(&Value::String(updated_at)));
            let request_times = request_timestamps(
                &user_message_ids(&thread),
                prompt_times
                    .get(&thread_id)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
                fallback,
            );

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
                    timestamp: request_times.get(&request_id).copied().unwrap_or(fallback),
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

fn all_prompt_request_times(logs: &[PathBuf]) -> HashMap<String, Vec<DateTime<Utc>>> {
    const MARKER: &str = "Received prompt request for session: ";
    let mut times = HashMap::<String, Vec<DateTime<Utc>>>::new();
    for path in logs {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            let Some(marker_at) = line.find(MARKER) else {
                continue;
            };
            let Some(stamp) = line.split_whitespace().next() else {
                continue;
            };
            let thread_id = line[marker_at + MARKER.len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default();
            if thread_id.is_empty() {
                continue;
            }
            times
                .entry(thread_id.to_string())
                .or_default()
                .push(parse_timestamp(Some(&Value::String(stamp.to_string()))));
        }
    }
    for list in times.values_mut() {
        list.sort();
    }
    times
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

fn user_message_ids(thread: &Value) -> Vec<String> {
    thread
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|message| {
            message
                .get("User")
                .and_then(|user| user.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

fn request_timestamps(
    user_ids: &[String],
    prompt_times: &[DateTime<Utc>],
    fallback: DateTime<Utc>,
) -> HashMap<String, DateTime<Utc>> {
    let fallback = prompt_times.first().copied().unwrap_or(fallback);
    let mut times = HashMap::new();
    let mut user_index = user_ids.len();
    let mut prompt_index = prompt_times.len();
    while user_index > 0 {
        user_index -= 1;
        let timestamp = if prompt_index > 0 {
            prompt_index -= 1;
            prompt_times[prompt_index]
        } else {
            fallback
        };
        times.insert(user_ids[user_index].clone(), timestamp);
    }
    times
}

fn parse_telemetry_usage(
    value: &Value,
    fallback_time: impl FnOnce(&str, Option<&str>) -> DateTime<Utc>,
) -> Result<Option<ParsedUsage>> {
    if value.get("event_type").and_then(Value::as_str)
        != Some("Agent Thread Completion Usage Updated")
    {
        return Ok(None);
    }
    let properties = value.get("event_properties").unwrap_or(value);
    let Some(counts) = zed_counts(properties) else {
        return Ok(None);
    };
    let thread_id = properties
        .get("thread_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let prompt_id = properties.get("prompt_id").and_then(Value::as_str);
    let model = properties
        .get("model")
        .and_then(Value::as_str)
        .map(|model| model.rsplit('/').next().unwrap_or(model).to_string());
    let elapsed_ms = value
        .get("milliseconds_since_first_event")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let timestamp = value
        .get("timestamp")
        .or_else(|| properties.get("timestamp"))
        .map(|value| parse_timestamp(Some(value)))
        .or_else(|| {
            thread_id
                .as_deref()
                .map(|thread_id| fallback_time(thread_id, prompt_id))
        })
        .unwrap_or_else(Utc::now);
    Ok(Some(ParsedUsage {
        counts,
        cumulative_snapshot: None,
        timestamp,
        model,
        session_id: thread_id.clone(),
        project_name: None,
        project_path: None,
        source_event_id: Some(format!(
            "telemetry:{}:{}:{elapsed_ms}:{}:{}",
            thread_id.as_deref().unwrap_or("thread"),
            prompt_id.unwrap_or("prompt"),
            counts.input_tokens,
            counts.output_tokens
        )),
        reported_cost_usd: None,
    }))
}

fn log_identity(metadata: &std::fs::Metadata) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!("{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        match (metadata.volume_serial_number(), metadata.file_index()) {
            (Some(volume), Some(index)) => Some(format!("{volume}:{index}")),
            _ => metadata
                .created()
                .ok()
                .map(|created| format!("{created:?}")),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        metadata
            .created()
            .ok()
            .map(|created| format!("{created:?}"))
    }
}

fn log_modified_at(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
}

fn prompt_log_fingerprints(logs: &[PathBuf]) -> Vec<PromptLogFingerprint> {
    logs.iter()
        .map(|path| {
            let meta = std::fs::metadata(path).ok();
            PromptLogFingerprint {
                path: path.clone(),
                identity: meta.as_ref().and_then(log_identity),
                size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                modified_at: meta.as_ref().and_then(log_modified_at),
            }
        })
        .collect()
}

fn prompt_logs_rewound(old: &[PromptLogFingerprint], new: &[PromptLogFingerprint]) -> bool {
    if old.is_empty() {
        return false;
    }
    old.iter().any(
        |previous| match new.iter().find(|next| next.path == previous.path) {
            None => true,
            Some(next) => {
                next.identity != previous.identity
                    || next.size < previous.size
                    || (next.size == previous.size && next.modified_at != previous.modified_at)
            }
        },
    )
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

    #[test]
    fn request_timestamps_align_from_latest_prompt() {
        let users = vec!["older".into(), "old".into(), "today".into()];
        let times = vec![
            DateTime::parse_from_rfc3339("2026-08-21T19:00:00+08:00")
                .unwrap()
                .with_timezone(&Utc),
            DateTime::parse_from_rfc3339("2026-08-25T15:14:54+08:00")
                .unwrap()
                .with_timezone(&Utc),
        ];
        let fallback = DateTime::parse_from_rfc3339("2026-08-25T07:15:26Z")
            .unwrap()
            .with_timezone(&Utc);
        let map = request_timestamps(&users, &times, fallback);
        assert_eq!(map["older"], times[0]);
        assert_eq!(map["old"], times[0]);
        assert_eq!(map["today"], times[1]);
    }

    #[test]
    fn prefers_telemetry_completions_over_thread_snapshots() {
        let root =
            std::env::temp_dir().join(format!("llmeter-zed-telemetry-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let db_path = root.join("threads.db");
        let connection = Connection::open(&db_path).unwrap();
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
            "model":{"provider":"zed.dev","model":"glm-5.3"},
            "request_token_usage":{
                "request-1":{"input_tokens":10,"output_tokens":3}
            }
        }"#;
        let compressed = zstd::encode_all(thread.as_slice(), 3).unwrap();
        connection
            .execute(
                "INSERT INTO threads VALUES (?1, ?2, ?3, 'zstd', ?4, ?5)",
                params![
                    "thread-1",
                    "summary",
                    "2026-08-25T07:15:26Z",
                    compressed,
                    "/tmp/project"
                ],
            )
            .unwrap();
        drop(connection);

        let telemetry = root.join("telemetry.log");
        fs::write(
            &telemetry,
            r#"{"event_type":"App Opened"}
{"event_type":"Agent Thread Completion Usage Updated","milliseconds_since_first_event":100,"timestamp":"2026-08-25T07:10:00Z","event_properties":{"thread_id":"thread-1","prompt_id":"prompt-1","model":"openai/glm-5.3","input_tokens":54713,"output_tokens":3604,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}
{"event_type":"Agent Thread Completion Usage Updated","milliseconds_since_first_event":200,"timestamp":"2026-08-25T07:12:00Z","event_properties":{"thread_id":"thread-1","prompt_id":"prompt-1","model":"openai/glm-5.3","input_tokens":58372,"output_tokens":39,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}
"#,
        )
        .unwrap();

        let database = Database::open_in_memory().unwrap();
        let engine = SyncEngine::with_adapters(
            database.clone(),
            vec![Box::new(ZedAdapter::new(vec![db_path], vec![telemetry]))],
        );
        let first = engine.sync_all().unwrap();
        assert_eq!(first.events_inserted, 2);
        assert_eq!(first.tokens_added, 54713 + 3604 + 58372 + 39);

        let overview = UsageRepository::new(database)
            .get_overview(
                DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
                Utc::now() + chrono::Duration::days(1),
            )
            .unwrap();
        assert_eq!(overview.total_tokens, 54713 + 3604 + 58372 + 39);
        assert_eq!(overview.input_tokens, 54713 + 58372);
        assert_eq!(overview.output_tokens, 3604 + 39);
        let _ = fs::remove_dir_all(root);
    }

    fn write_thread_db(path: &std::path::Path, thread_id: &str, request_id: &str, tokens: u64) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS threads (
                    id TEXT PRIMARY KEY,
                    summary TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    data_type TEXT NOT NULL,
                    data BLOB NOT NULL,
                    folder_paths TEXT
                );",
            )
            .unwrap();
        let thread = format!(
            r#"{{"model":{{"provider":"zed.dev","model":"glm-5.3"}},"request_token_usage":{{"{request_id}":{{"input_tokens":{tokens},"output_tokens":0}}}}}}"#
        );
        let compressed = zstd::encode_all(thread.as_bytes(), 3).unwrap();
        connection
            .execute(
                "INSERT OR REPLACE INTO threads VALUES (?1, ?2, ?3, 'zstd', ?4, ?5)",
                params![
                    thread_id,
                    "summary",
                    "2026-08-25T07:15:26Z",
                    compressed,
                    "/tmp/project"
                ],
            )
            .unwrap();
    }

    fn write_zed_log(dir: &std::path::Path, entries: &[(&str, &str)]) {
        let body = entries
            .iter()
            .map(|(timestamp, session)| {
                format!(
                    "{timestamp} INFO  [agent] Received prompt request for session: {session}\n"
                )
            })
            .collect::<String>();
        fs::write(dir.join("Zed.log"), body).unwrap();
    }

    fn usage_line(thread_id: &str, prompt_id: &str, input: u64, output: u64) -> String {
        format!(
            r#"{{"event_type":"Agent Thread Completion Usage Updated","milliseconds_since_first_event":1,"event_properties":{{"thread_id":"{thread_id}","prompt_id":"{prompt_id}","model":"glm-5.3","input_tokens":{input},"output_tokens":{output}}}}}"#
        )
    }

    fn rfc3339(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn write_thread_db_with_users(path: &std::path::Path, thread_id: &str, users: &[(&str, u64)]) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS threads (
                    id TEXT PRIMARY KEY,
                    summary TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    data_type TEXT NOT NULL,
                    data BLOB NOT NULL,
                    folder_paths TEXT
                );",
            )
            .unwrap();
        let requests = users
            .iter()
            .map(|(id, tokens)| format!(r#""{id}":{{"input_tokens":{tokens},"output_tokens":0}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let messages = users
            .iter()
            .map(|(id, _)| format!(r#"{{"User":{{"id":"{id}","content":"x"}}}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let thread = format!(
            r#"{{"model":{{"provider":"zed.dev","model":"glm-5.3"}},"request_token_usage":{{{requests}}},"messages":[{messages}]}}"#
        );
        let compressed = zstd::encode_all(thread.as_bytes(), 3).unwrap();
        connection
            .execute(
                "INSERT OR REPLACE INTO threads VALUES (?1, ?2, ?3, 'zstd', ?4, ?5)",
                params![
                    thread_id,
                    "summary",
                    "2026-08-25T07:15:26Z",
                    compressed,
                    "/tmp/project"
                ],
            )
            .unwrap();
    }

    #[test]
    fn empty_telemetry_falls_back_to_sqlite() {
        let root = std::env::temp_dir().join(format!(
            "llmeter-zed-empty-telemetry-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let db_path = root.join("threads.db");
        write_thread_db(&db_path, "thread-keep", "request-1", 40);
        let telemetry = root.join("telemetry.log");
        fs::write(&telemetry, r#"{"event_type":"App Opened"}"#).unwrap();

        let database = Database::open_in_memory().unwrap();
        let engine = SyncEngine::with_adapters(
            database.clone(),
            vec![Box::new(ZedAdapter::new(vec![db_path], vec![telemetry]))],
        );
        let first = engine.sync_all().unwrap();
        assert_eq!(first.events_inserted, 1);
        assert_eq!(first.tokens_added, 40);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sqlite_keeps_threads_missing_from_telemetry() {
        let root = std::env::temp_dir().join(format!("llmeter-zed-merge-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let db_path = root.join("threads.db");
        write_thread_db(&db_path, "thread-1", "request-1", 10);
        write_thread_db(&db_path, "thread-keep", "request-2", 40);
        let telemetry = root.join("telemetry.log");
        fs::write(
            &telemetry,
            r#"{"event_type":"Agent Thread Completion Usage Updated","timestamp":"2026-08-25T07:10:00Z","milliseconds_since_first_event":1,"event_properties":{"thread_id":"thread-1","prompt_id":"p1","model":"glm-5.3","input_tokens":20,"output_tokens":5}}
"#,
        )
        .unwrap();

        let database = Database::open_in_memory().unwrap();
        let engine = SyncEngine::with_adapters(
            database.clone(),
            vec![Box::new(ZedAdapter::new(vec![db_path], vec![telemetry]))],
        );
        let first = engine.sync_all().unwrap();
        assert_eq!(first.events_inserted, 2);
        assert_eq!(first.tokens_added, 65);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn undated_telemetry_uses_prompt_log_in_order() {
        let root = std::env::temp_dir().join(format!("llmeter-zed-undated-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        write_zed_log(
            &root,
            &[
                ("2026-08-21T19:00:00+08:00", "thread-1"),
                ("2026-08-25T15:14:54+08:00", "thread-1"),
            ],
        );
        let telemetry = root.join("telemetry.log");
        fs::write(&telemetry, "").unwrap();
        let adapter = ZedAdapter::new(vec![], vec![telemetry.clone()]);
        let source = SourceFile::new(telemetry, Provider::Zed);
        adapter.discover_sources().unwrap();
        adapter.begin_source(&source, true);
        let first = adapter
            .parse_line(&source, usage_line("thread-1", "p1", 10, 1).as_bytes())
            .unwrap()
            .unwrap();
        let second = adapter
            .parse_line(&source, usage_line("thread-1", "p2", 20, 2).as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(first.timestamp, rfc3339("2026-08-21T19:00:00+08:00"));
        assert_eq!(second.timestamp, rfc3339("2026-08-25T15:14:54+08:00"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn telemetry_rewind_reuses_first_prompt_time() {
        let root = std::env::temp_dir().join(format!("llmeter-zed-rewind-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        write_zed_log(
            &root,
            &[
                ("2026-08-21T19:00:00+08:00", "thread-1"),
                ("2026-08-25T15:14:54+08:00", "thread-1"),
            ],
        );
        let telemetry = root.join("telemetry.log");
        fs::write(&telemetry, "").unwrap();
        let adapter = ZedAdapter::new(vec![], vec![telemetry.clone()]);
        let source = SourceFile::new(telemetry, Provider::Zed);
        adapter.discover_sources().unwrap();
        adapter.begin_source(&source, true);
        let _ = adapter
            .parse_line(&source, usage_line("thread-1", "p1", 10, 1).as_bytes())
            .unwrap();
        let _ = adapter
            .parse_line(&source, usage_line("thread-1", "p2", 20, 2).as_bytes())
            .unwrap();
        adapter.begin_source(&source, true);
        let again = adapter
            .parse_line(&source, usage_line("thread-1", "p1", 10, 1).as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(again.timestamp, rfc3339("2026-08-21T19:00:00+08:00"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prompt_log_growth_is_visible_after_rediscover() {
        let root =
            std::env::temp_dir().join(format!("llmeter-zed-log-growth-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let db_path = root.join("threads.db");
        write_thread_db_with_users(&db_path, "thread-1", &[("user-old", 10), ("user-new", 20)]);
        write_zed_log(&root, &[("2026-08-21T19:00:00+08:00", "thread-1")]);
        let telemetry = root.join("telemetry.log");
        fs::write(&telemetry, r#"{"event_type":"App Opened"}"#).unwrap();
        let adapter = ZedAdapter::new(vec![db_path.clone()], vec![telemetry]);
        let source = SourceFile {
            path: db_path,
            provider: Provider::Zed,
            format: SourceFormat::Sqlite,
            session_id: None,
            project_path: None,
            project_name: None,
        };
        adapter.discover_sources().unwrap();
        let first = adapter.parse_sqlite(&source).unwrap();
        assert_eq!(first.len(), 2);
        assert!(
            first
                .iter()
                .all(|usage| usage.timestamp == rfc3339("2026-08-21T19:00:00+08:00"))
        );

        write_zed_log(
            &root,
            &[
                ("2026-08-21T19:00:00+08:00", "thread-1"),
                ("2026-08-25T15:14:54+08:00", "thread-1"),
            ],
        );
        adapter.discover_sources().unwrap();
        let second = adapter.parse_sqlite(&source).unwrap();
        let old = second
            .iter()
            .find(|usage| {
                usage.source_event_id.as_deref() == Some("thread:thread-1:request:user-old")
            })
            .unwrap();
        let new = second
            .iter()
            .find(|usage| {
                usage.source_event_id.as_deref() == Some("thread:thread-1:request:user-new")
            })
            .unwrap();
        assert_eq!(old.timestamp, rfc3339("2026-08-21T19:00:00+08:00"));
        assert_eq!(new.timestamp, rfc3339("2026-08-25T15:14:54+08:00"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn same_prompt_id_reuses_prompt_time() {
        let root =
            std::env::temp_dir().join(format!("llmeter-zed-prompt-id-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        write_zed_log(
            &root,
            &[
                ("2026-08-21T19:00:00+08:00", "thread-1"),
                ("2026-08-25T15:14:54+08:00", "thread-1"),
            ],
        );
        let telemetry = root.join("telemetry.log");
        fs::write(&telemetry, "").unwrap();
        let adapter = ZedAdapter::new(vec![], vec![telemetry.clone()]);
        let source = SourceFile::new(telemetry, Provider::Zed);
        adapter.discover_sources().unwrap();
        adapter.begin_source(&source, true);
        let first = adapter
            .parse_line(&source, usage_line("thread-1", "p1", 10, 1).as_bytes())
            .unwrap()
            .unwrap();
        let repeat = adapter
            .parse_line(&source, usage_line("thread-1", "p1", 11, 2).as_bytes())
            .unwrap()
            .unwrap();
        let next = adapter
            .parse_line(&source, usage_line("thread-1", "p2", 20, 3).as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(first.timestamp, rfc3339("2026-08-21T19:00:00+08:00"));
        assert_eq!(repeat.timestamp, first.timestamp);
        assert_eq!(next.timestamp, rfc3339("2026-08-25T15:14:54+08:00"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn shrinking_prompt_log_resets_pairing_without_telemetry_rewind() {
        let root =
            std::env::temp_dir().join(format!("llmeter-zed-log-shrink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        write_zed_log(
            &root,
            &[
                ("2026-08-21T19:00:00+08:00", "thread-1"),
                ("2026-08-22T12:00:00+08:00", "thread-1"),
                ("2026-08-25T15:14:54+08:00", "thread-1"),
            ],
        );
        let telemetry = root.join("telemetry.log");
        fs::write(&telemetry, "").unwrap();
        let adapter = ZedAdapter::new(vec![], vec![telemetry.clone()]);
        let source = SourceFile::new(telemetry, Provider::Zed);
        adapter.discover_sources().unwrap();
        adapter.begin_source(&source, true);
        for prompt in ["p1", "p2", "p3"] {
            let _ = adapter
                .parse_line(&source, usage_line("thread-1", prompt, 10, 1).as_bytes())
                .unwrap();
        }
        write_zed_log(
            &root,
            &[
                ("2026-08-21T19:00:00+08:00", "thread-1"),
                ("2026-08-22T12:00:00+08:00", "thread-1"),
            ],
        );
        adapter.discover_sources().unwrap();
        let late = adapter
            .parse_line(&source, usage_line("thread-1", "p3", 12, 2).as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(late.timestamp, rfc3339("2026-08-25T15:14:54+08:00"));
        let next = adapter
            .parse_line(&source, usage_line("thread-1", "p4", 30, 4).as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(next.timestamp, rfc3339("2026-08-21T19:00:00+08:00"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prompt_log_append_is_not_rewound() {
        let path = PathBuf::from("/tmp/Zed.log");
        let old = vec![PromptLogFingerprint {
            path: path.clone(),
            identity: Some("1:2".into()),
            size: 10,
            modified_at: Some(1),
        }];
        let appended = vec![PromptLogFingerprint {
            path: path.clone(),
            identity: Some("1:2".into()),
            size: 40,
            modified_at: Some(2),
        }];
        let rewritten = vec![PromptLogFingerprint {
            path: path.clone(),
            identity: Some("1:2".into()),
            size: 10,
            modified_at: Some(3),
        }];
        let shrunk = vec![PromptLogFingerprint {
            path: path.clone(),
            identity: Some("1:2".into()),
            size: 8,
            modified_at: Some(4),
        }];
        let replaced = vec![PromptLogFingerprint {
            path,
            identity: Some("1:9".into()),
            size: 40,
            modified_at: Some(5),
        }];
        assert!(!prompt_logs_rewound(&old, &appended));
        assert!(prompt_logs_rewound(&old, &rewritten));
        assert!(prompt_logs_rewound(&old, &shrunk));
        assert!(prompt_logs_rewound(&old, &replaced));
        assert!(prompt_logs_rewound(&old, &[]));
    }

    #[test]
    fn sync_engine_truncation_redates_from_first_prompt() {
        let root =
            std::env::temp_dir().join(format!("llmeter-zed-sync-truncate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        write_zed_log(
            &root,
            &[
                ("2026-08-21T19:00:00+08:00", "thread-1"),
                ("2026-08-25T15:14:54+08:00", "thread-1"),
            ],
        );
        let telemetry = root.join("telemetry.log");
        fs::write(
            &telemetry,
            format!(
                "{}\n{}\n",
                usage_line("thread-1", "p1", 10, 1),
                usage_line("thread-1", "p2", 20, 2)
            ),
        )
        .unwrap();
        let database = Database::open_in_memory().unwrap();
        let engine = SyncEngine::with_adapters(
            database.clone(),
            vec![Box::new(ZedAdapter::new(vec![], vec![telemetry.clone()]))],
        );
        let first = engine.sync_all().unwrap();
        assert_eq!(first.events_inserted, 2);

        fs::write(
            &telemetry,
            format!("{}\n", usage_line("thread-1", "p1", 10, 1)),
        )
        .unwrap();
        let second = engine.sync_all().unwrap();
        assert_eq!(second.events_inserted, 1);
        let recent = UsageRepository::new(database)
            .get_recent_activity(8)
            .unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].timestamp, rfc3339("2026-08-21T19:00:00+08:00"));
        let _ = fs::remove_dir_all(root);
    }
}
