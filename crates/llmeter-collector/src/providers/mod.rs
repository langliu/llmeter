use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use chrono::{DateTime, Utc};
use llmeter_core::{
    Provider, ProviderDetection, SourceFile, SourceFormat, SourceMetadata, TokenCounts,
    UsageSnapshot, parse_timestamp,
};
use serde_json::Value;

use llmeter_storage::Database;

pub(crate) mod antigravity;
mod claude;
mod cline;
mod codex;
mod copilot;
mod cursor;
mod grok;
mod hermes;
mod kilo;
mod opencode;
mod pi;
mod qoder;
mod roo;
mod trae;
mod zcode;
mod zed;

pub use antigravity::AntigravityAdapter;
pub use claude::ClaudeAdapter;
pub use cline::ClineAdapter;
pub use codex::CodexAdapter;
pub use copilot::CopilotAdapter;
pub use cursor::CursorAdapter;
pub(crate) use cursor::{cursor_root, cursor_session_cookie};
pub use grok::GrokAdapter;
pub use hermes::HermesAdapter;
pub use kilo::KiloAdapter;
pub use opencode::OpenCodeAdapter;
pub use pi::PiCompatibleAdapter;
pub use qoder::QoderAdapter;
pub(crate) use qoder::qoder_root;
pub use roo::RooAdapter;
pub use trae::{TRAE_CN_USAGE_SETTING, TraeAdapter};
pub(crate) use trae::{has_trae_cn_auth, read_entitlement, trae_cn_root, trae_root};
pub use zcode::ZCodeAdapter;
pub use zed::ZedAdapter;

pub const PARSER_VERSION: u32 = 1;

#[derive(Clone, Debug)]
pub struct ParsedUsage {
    pub counts: TokenCounts,
    pub cumulative_snapshot: Option<UsageSnapshot>,
    pub timestamp: DateTime<Utc>,
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub project_path: Option<PathBuf>,
    pub project_name: Option<String>,
    pub source_event_id: Option<String>,
    pub reported_cost_usd: Option<f64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum SnapshotPolicy {
    #[default]
    Upsert,
    /// Replace every stored event for this provider.
    ReplaceAll,
    /// Replace this provider's events inside the authoritative remote window.
    ReplaceSince(DateTime<Utc>),
}

#[derive(Clone, Debug)]
pub struct ParsedSnapshot {
    pub usages: Vec<ParsedUsage>,
    pub policy: SnapshotPolicy,
    /// Stable identity of the signed-in account that produced this snapshot.
    /// Local file parsers leave this unset.
    pub scope: Option<String>,
}

pub(crate) fn snapshot_scope(provider: Provider, identity: &str) -> String {
    let value = format!("{provider}\u{0}{identity}");
    blake3::hash(value.as_bytes()).to_hex().to_string()
}

pub trait ProviderAdapter: Send + Sync {
    fn provider(&self) -> Provider;
    fn parser_version(&self) -> u32 {
        PARSER_VERSION
    }
    fn watch_roots(&self) -> Vec<PathBuf> {
        Vec::new()
    }
    fn detect(&self) -> Result<ProviderDetection>;
    /// Pre-flight probe used by the sync loop to bail on unsupported storage
    /// versions. Adapters without a schema version (plain JSONL readers)
    /// return `None` by default so their `detect()` tree walk does not
    /// duplicate `discover_sources()`.
    fn sync_detection(&self) -> Result<Option<ProviderDetection>> {
        Ok(None)
    }
    fn discover_sources(&self) -> Result<Vec<SourceFile>>;
    fn update_source_metadata(
        &self,
        _source: &SourceFile,
        _line: &[u8],
        _metadata: &mut SourceMetadata,
    ) -> Result<()> {
        Ok(())
    }
    fn begin_source(&self, _source: &SourceFile, _from_beginning: bool) {}

    fn parse_line(&self, source: &SourceFile, line: &[u8]) -> Result<Option<ParsedUsage>>;
    fn ingest_line(
        &self,
        source: &SourceFile,
        line: &[u8],
        metadata: &mut SourceMetadata,
    ) -> Result<Option<ParsedUsage>> {
        self.update_source_metadata(source, line, metadata)?;
        self.parse_line(source, line)
    }
    fn parse_sqlite(&self, _source: &SourceFile) -> Result<Vec<ParsedUsage>> {
        Err(anyhow::anyhow!(
            "SQLite source is not supported by this adapter"
        ))
    }
    fn parse_snapshot(&self, _source: &SourceFile) -> Result<ParsedSnapshot> {
        Err(anyhow::anyhow!(
            "Snapshot source is not supported by this adapter"
        ))
    }
    fn uses_remote_snapshot(&self) -> bool {
        false
    }
}

pub fn default_adapters(database: &Database) -> Vec<Box<dyn ProviderAdapter>> {
    vec![
        Box::new(CodexAdapter::default()),
        Box::new(ClaudeAdapter::default()),
        Box::new(CursorAdapter::default()),
        Box::new(QoderAdapter::default()),
        Box::new(TraeAdapter::with_database(database.clone())),
        Box::new(OpenCodeAdapter::default()),
        Box::new(PiCompatibleAdapter::pi(home_dir())),
        Box::new(PiCompatibleAdapter::omp(home_dir())),
        Box::new(ZedAdapter::default()),
        Box::new(GrokAdapter::default()),
        Box::new(HermesAdapter::default()),
        Box::new(AntigravityAdapter::default()),
        Box::new(ZCodeAdapter::default()),
        Box::new(CopilotAdapter::default()),
        Box::new(ClineAdapter::default()),
        Box::new(RooAdapter::default()),
        Box::new(KiloAdapter::default()),
    ]
}

pub(crate) fn json_value(line: &[u8]) -> Result<Value> {
    Ok(serde_json::from_slice(line)?)
}

pub(crate) fn nested<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        let object = current.as_object()?;
        current = object.get(*key).or_else(|| {
            object
                .iter()
                .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
                .map(|(_, value)| value)
        })?;
    }
    Some(current)
}

pub(crate) fn first_string(value: &Value, keys: &[&str]) -> Option<String> {
    if let Some(object) = value.as_object() {
        for key in keys {
            if let Some(found) = object
                .get(*key)
                .or_else(|| {
                    object
                        .iter()
                        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
                        .map(|(_, value)| value)
                })
                .and_then(Value::as_str)
            {
                return Some(found.to_string());
            }
        }
        for child in object.values() {
            if let Some(found) = first_string(child, keys) {
                return Some(found);
            }
        }
    } else if let Some(array) = value.as_array() {
        for child in array {
            if let Some(found) = first_string(child, keys) {
                return Some(found);
            }
        }
    }
    None
}

pub(crate) fn object_for_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    if let Some(object) = value.as_object() {
        for (candidate, child) in object {
            if candidate.eq_ignore_ascii_case(key) && child.is_object() {
                return Some(child);
            }
        }
        for child in object.values() {
            if let Some(found) = object_for_key(child, key) {
                return Some(found);
            }
        }
    } else if let Some(array) = value.as_array() {
        for child in array {
            if let Some(found) = object_for_key(child, key) {
                return Some(found);
            }
        }
    }
    None
}

pub(crate) fn object_with_usage(value: &Value) -> Option<&Value> {
    const USAGE_KEYS: &[&str] = &[
        "input_tokens",
        "inputTokens",
        "output_tokens",
        "outputTokens",
        "cache_read_input_tokens",
        "cacheRead",
        "cache_read",
        "cache_creation_input_tokens",
        "cacheWrite",
        "cache_write",
        "total_tokens",
        "totalTokens",
    ];
    if let Some(object) = value.as_object() {
        let has_usage_key = USAGE_KEYS.iter().any(|key| {
            object
                .keys()
                .any(|candidate| candidate.eq_ignore_ascii_case(key))
        });
        if has_usage_key {
            return Some(value);
        }
        for child in object.values() {
            if let Some(found) = object_with_usage(child) {
                return Some(found);
            }
        }
    } else if let Some(array) = value.as_array() {
        for child in array {
            if let Some(found) = object_with_usage(child) {
                return Some(found);
            }
        }
    }
    None
}

pub(crate) fn counts_from_usage(
    value: &Value,
    include_cached_in_total: bool,
) -> Option<TokenCounts> {
    let counts = UsageFields::default().extract(value);
    if counts.is_zero() {
        return None;
    }
    let mut counts = counts;
    if counts.total_tokens == 0 {
        counts.total_tokens = counts
            .input_tokens
            .saturating_add(counts.output_tokens)
            .saturating_add(counts.reasoning_tokens)
            .saturating_add(counts.cache_creation_input_tokens);
        if include_cached_in_total {
            counts.total_tokens = counts
                .total_tokens
                .saturating_add(counts.cached_input_tokens);
        }
    }
    Some(counts)
}

const INPUT_KEYS: &[&str] = &["input_tokens", "inputTokens", "input"];
const CACHED_KEYS: &[&str] = &[
    "cached_input_tokens",
    "cachedInputTokens",
    "cache_read_input_tokens",
    "cacheReadInputTokens",
    "cacheRead",
    "cache_read",
];
const CACHE_CREATION_KEYS: &[&str] = &[
    "cache_creation_input_tokens",
    "cacheCreationInputTokens",
    "cache_write_input_tokens",
    "cacheWriteInputTokens",
    "cache_creation",
    "cacheWrite",
    "cache_write",
];
const OUTPUT_KEYS: &[&str] = &["output_tokens", "outputTokens", "output"];
const REASONING_KEYS: &[&str] = &[
    "reasoning_output_tokens",
    "reasoning_tokens",
    "reasoningTokens",
    "reasoning",
];
const TOTAL_KEYS: &[&str] = &["total_tokens", "totalTokens", "total"];

/// Extracts all six token counters in a single traversal. Equivalent to
/// running `first_number` once per field, but each node is visited once
/// instead of once per field.
#[derive(Default)]
struct UsageFields {
    input: Option<u64>,
    cached: Option<u64>,
    cache_creation: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
    total: Option<u64>,
}

impl UsageFields {
    fn extract(mut self, value: &Value) -> TokenCounts {
        self.visit(value);
        TokenCounts {
            input_tokens: self.input.unwrap_or_default(),
            cached_input_tokens: self.cached.unwrap_or_default(),
            cache_creation_input_tokens: self.cache_creation.unwrap_or_default(),
            output_tokens: self.output.unwrap_or_default(),
            reasoning_tokens: self.reasoning.unwrap_or_default(),
            total_tokens: self.total.unwrap_or_default(),
        }
    }

    fn visit(&mut self, value: &Value) {
        match value {
            Value::Object(object) => {
                if self.input.is_none() {
                    self.input = number_for(object, INPUT_KEYS);
                }
                if self.cached.is_none() {
                    self.cached = number_for(object, CACHED_KEYS);
                }
                if self.cache_creation.is_none() {
                    self.cache_creation = number_for(object, CACHE_CREATION_KEYS);
                }
                if self.output.is_none() {
                    self.output = number_for(object, OUTPUT_KEYS);
                }
                if self.reasoning.is_none() {
                    self.reasoning = number_for(object, REASONING_KEYS);
                }
                if self.total.is_none() {
                    self.total = number_for(object, TOTAL_KEYS);
                }
                if self.complete() {
                    return;
                }
                for child in object.values() {
                    if self.complete() {
                        return;
                    }
                    self.visit(child);
                }
            }
            Value::Array(array) => {
                for child in array {
                    if self.complete() {
                        return;
                    }
                    self.visit(child);
                }
            }
            _ => {}
        }
    }

    fn complete(&self) -> bool {
        self.input.is_some()
            && self.cached.is_some()
            && self.cache_creation.is_some()
            && self.output.is_some()
            && self.reasoning.is_some()
            && self.total.is_some()
    }
}

fn number_for(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<u64> {
    for key in keys {
        if let Some(found) = object
            .get(*key)
            .or_else(|| {
                object
                    .iter()
                    .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
                    .map(|(_, value)| value)
            })
            .and_then(as_u64)
        {
            return Some(found);
        }
    }
    None
}

pub(crate) fn usage_snapshot(value: &Value) -> Option<UsageSnapshot> {
    counts_from_usage(value, false).map(UsageSnapshot::from)
}

pub(crate) fn source_event_id(value: &Value) -> Option<String> {
    first_string(
        value,
        &[
            "event_id",
            "eventId",
            "message_id",
            "messageId",
            "request_id",
            "requestId",
            "uuid",
        ],
    )
}

pub(crate) fn model(value: &Value) -> Option<String> {
    first_string(
        value,
        &["model", "model_name", "modelName", "modelId", "model_id"],
    )
}

pub(crate) fn session_id(value: &Value) -> Option<String> {
    first_string(
        value,
        &[
            "session_id",
            "sessionId",
            "conversation_id",
            "conversationId",
        ],
    )
}

pub(crate) fn project_path(value: &Value) -> Option<PathBuf> {
    first_string(
        value,
        &[
            "cwd",
            "project_path",
            "projectPath",
            "working_directory",
            "workingDirectory",
        ],
    )
    .map(PathBuf::from)
}

pub(crate) fn project_name(path: Option<&Path>) -> Option<String> {
    path.and_then(Path::file_name)
        .map(|value| value.to_string_lossy().to_string())
        .filter(|value| !value.is_empty())
}

pub(crate) fn timestamp(value: &Value) -> DateTime<Utc> {
    let timestamp = nested(value, &["timestamp"])
        .or_else(|| nested(value, &["created_at"]))
        .or_else(|| nested(value, &["createdAt"]))
        .or_else(|| nested(value, &["payload", "timestamp"]));
    parse_timestamp(timestamp)
}

pub(crate) fn walk_jsonl(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    walk_matching(root, |path, _| {
        path.extension()
            .is_some_and(|extension| extension == "jsonl")
    })
}

pub(crate) fn jsonl_exists(root: &Path) -> std::io::Result<bool> {
    if !root.exists() {
        return Ok(false);
    }
    exists_matching(root, |path, _| {
        path.extension()
            .is_some_and(|extension| extension == "jsonl")
    })
}

/// Recursively walks `root`, skipping symlinks, collecting matching file
/// paths. Shared by every filesystem-based provider so walk semantics stay
/// identical (symlink handling, recursion order, sorted results).
pub(crate) fn walk_matching(
    root: &Path,
    filter: impl Fn(&Path, &fs::Metadata) -> bool,
) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !root.exists() {
        return Ok(files);
    }
    walk_matching_inner(root, &filter, &mut files)?;
    files.sort();
    Ok(files)
}

fn walk_matching_inner(
    path: &Path,
    filter: &impl Fn(&Path, &fs::Metadata) -> bool,
    files: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_file() {
        if filter(path, &metadata) {
            files.push(path.to_path_buf());
        }
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        walk_matching_inner(&entry?.path(), filter, files)?;
    }
    Ok(())
}

/// Early-exit variant of [`walk_matching`] that stops at the first match.
pub(crate) fn exists_matching(
    root: &Path,
    filter: impl Fn(&Path, &fs::Metadata) -> bool,
) -> std::io::Result<bool> {
    exists_matching_inner(root, &filter)
}

fn exists_matching_inner(
    path: &Path,
    filter: &impl Fn(&Path, &fs::Metadata) -> bool,
) -> std::io::Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(false);
    }
    if metadata.is_file() {
        return Ok(filter(path, &metadata));
    }
    if !metadata.is_dir() {
        return Ok(false);
    }
    for entry in fs::read_dir(path)? {
        if exists_matching_inner(&entry?.path(), filter)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Maps discovered JSONL files to session sources keyed by file stem.
pub(crate) fn jsonl_session_sources(
    files: Vec<PathBuf>,
    provider: Provider,
    project_name: impl Fn(&Path) -> Option<String>,
) -> Vec<SourceFile> {
    files
        .into_iter()
        .map(|path| {
            let session_id = path
                .file_stem()
                .map(|value| value.to_string_lossy().to_string());
            SourceFile {
                session_id,
                project_name: project_name(&path),
                path,
                provider,
                format: SourceFormat::Jsonl,
                project_path: None,
            }
        })
        .collect()
}

pub(crate) fn deduplicate_paths(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for path in paths {
        let canonical = path.canonicalize().unwrap_or(path);
        if seen.insert(canonical.clone()) {
            result.push(canonical);
        }
    }
    result.sort();
    result
}

/// Size + mtime fingerprint of a file, used to skip unchanged re-reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StorageFingerprint {
    pub(crate) size: u64,
    pub(crate) modified: Option<std::time::SystemTime>,
}

pub(crate) fn storage_fingerprint(path: &Path) -> Option<StorageFingerprint> {
    let metadata = fs::metadata(path).ok()?;
    Some(StorageFingerprint {
        size: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

/// In-process memo keyed by file path, invalidated when the file fingerprint
/// changes. Bounds memory by dropping everything past 1024 entries.
#[derive(Debug)]
pub(crate) struct PathMemo<T> {
    entries: std::sync::Mutex<HashMap<PathBuf, (Option<StorageFingerprint>, T)>>,
}

impl<T: Clone> PathMemo<T> {
    pub(crate) fn new() -> Self {
        Self {
            entries: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn get(&self, path: &Path) -> Option<T> {
        let fingerprint = storage_fingerprint(path);
        let mut guard = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if guard.len() > 1024 {
            guard.clear();
        }
        match guard.get(path) {
            Some((cached, value)) if *cached == fingerprint => Some(value.clone()),
            _ => None,
        }
    }

    pub(crate) fn insert(&self, path: &Path, value: T) {
        let fingerprint = storage_fingerprint(path);
        let mut guard = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        guard.insert(path.to_path_buf(), (fingerprint, value));
    }

    pub(crate) fn get_or_compute(&self, path: &Path, compute: impl FnOnce() -> T) -> T {
        if let Some(value) = self.get(path) {
            return value;
        }
        let value = compute();
        self.insert(path, value.clone());
        value
    }
}

pub(crate) fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn data_status(
    provider: Provider,
    roots: Vec<PathBuf>,
    has_data: bool,
    detail: Option<String>,
) -> ProviderDetection {
    let status = if has_data {
        llmeter_core::ProviderStatus::DataFound
    } else if roots.iter().any(|root| root.exists()) {
        llmeter_core::ProviderStatus::Installed
    } else {
        llmeter_core::ProviderStatus::NotInstalled
    };
    ProviderDetection {
        provider,
        status,
        roots,
        detail,
    }
}

fn as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn counts_from_usage_reads_flat_and_nested_objects() {
        let flat = json!({
            "input_tokens": 10,
            "cache_read_input_tokens": 5,
            "cache_creation_input_tokens": 2,
            "output_tokens": 7,
            "reasoning_tokens": 3,
            "total_tokens": 27,
        });
        assert_eq!(
            counts_from_usage(&flat, false).map(|counts| counts.total_tokens),
            Some(27)
        );

        // Usage nested under an intermediate object with camelCase aliases.
        let nested = json!({
            "payload": {
                "usage": {
                    "inputTokens": 10,
                    "cacheRead": 5,
                    "cacheWrite": 2,
                    "outputTokens": 7,
                    "reasoning": 3,
                }
            }
        });
        let counts = counts_from_usage(&nested, false).expect("nested usage");
        assert_eq!(counts.input_tokens, 10);
        assert_eq!(counts.cached_input_tokens, 5);
        assert_eq!(counts.cache_creation_input_tokens, 2);
        assert_eq!(counts.output_tokens, 7);
        assert_eq!(counts.reasoning_tokens, 3);
        // Total falls back to input+output+reasoning+cache_creation.
        assert_eq!(counts.total_tokens, 22);
        assert_eq!(
            counts_from_usage(&nested, true).unwrap().total_tokens,
            27,
            "cached tokens join the total when included"
        );
    }

    #[test]
    fn counts_from_usage_skips_non_numeric_values_and_prefers_closer_nodes() {
        // A string under a usage key must be ignored, not counted.
        let ignored = json!({ "input_tokens": "many", "output_tokens": 4 });
        let counts = counts_from_usage(&ignored, false).expect("usage found");
        assert_eq!(counts.input_tokens, 0);
        assert_eq!(counts.output_tokens, 4);

        // The first matching node in traversal order wins. serde_json maps
        // iterate alphabetically by default, so "a" is visited before "b".
        let shadowed = json!({
            "a": { "input_tokens": 1 },
            "b": { "input_tokens": 999 },
        });
        assert_eq!(counts_from_usage(&shadowed, false).unwrap().input_tokens, 1);
    }
}
