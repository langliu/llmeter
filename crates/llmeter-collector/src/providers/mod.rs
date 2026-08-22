use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use chrono::{DateTime, Utc};
use llmeter_core::{
    Provider, ProviderDetection, SourceFile, SourceMetadata, TokenCounts, UsageSnapshot,
    parse_timestamp,
};
use serde_json::Value;

mod claude;
mod codex;
mod cursor;
mod grok;
mod hermes;
mod opencode;
mod pi;
mod qoder;
mod trae;
mod zed;

pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use cursor::CursorAdapter;
pub(crate) use cursor::cursor_root;
pub use grok::GrokAdapter;
pub use hermes::HermesAdapter;
pub use opencode::OpenCodeAdapter;
pub use pi::PiAdapter;
pub use qoder::QoderAdapter;
pub(crate) use qoder::qoder_root;
pub use trae::TraeAdapter;
pub(crate) use trae::{has_trae_cn_auth, read_entitlement, trae_cn_root, trae_root};
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

pub trait ProviderAdapter: Send + Sync {
    fn provider(&self) -> Provider;
    fn parser_version(&self) -> u32 {
        PARSER_VERSION
    }
    fn detect(&self) -> Result<ProviderDetection>;
    fn discover_sources(&self) -> Result<Vec<SourceFile>>;
    fn update_source_metadata(
        &self,
        _source: &SourceFile,
        _line: &[u8],
        _metadata: &mut SourceMetadata,
    ) -> Result<()> {
        Ok(())
    }
    fn parse_line(&self, source: &SourceFile, line: &[u8]) -> Result<Option<ParsedUsage>>;
    fn parse_sqlite(&self, _source: &SourceFile) -> Result<Vec<ParsedUsage>> {
        Err(anyhow::anyhow!(
            "SQLite source is not supported by this adapter"
        ))
    }
}

pub fn default_adapters() -> Vec<Box<dyn ProviderAdapter>> {
    vec![
        Box::new(CodexAdapter::default()),
        Box::new(ClaudeAdapter::default()),
        Box::new(CursorAdapter::default()),
        Box::new(QoderAdapter::default()),
        Box::new(TraeAdapter::default()),
        Box::new(OpenCodeAdapter::default()),
        Box::new(PiAdapter::default()),
        Box::new(ZedAdapter::default()),
        Box::new(GrokAdapter::default()),
        Box::new(HermesAdapter::default()),
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

pub(crate) fn first_number(value: &Value, keys: &[&str]) -> Option<u64> {
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
                .and_then(as_u64)
            {
                return Some(found);
            }
        }
        for child in object.values() {
            if let Some(found) = first_number(child, keys) {
                return Some(found);
            }
        }
    } else if let Some(array) = value.as_array() {
        for child in array {
            if let Some(found) = first_number(child, keys) {
                return Some(found);
            }
        }
    }
    None
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
    let counts = TokenCounts {
        input_tokens: first_number(value, &["input_tokens", "inputTokens", "input"])
            .unwrap_or_default(),
        cached_input_tokens: first_number(
            value,
            &[
                "cached_input_tokens",
                "cachedInputTokens",
                "cache_read_input_tokens",
                "cacheReadInputTokens",
                "cacheRead",
                "cache_read",
            ],
        )
        .unwrap_or_default(),
        cache_creation_input_tokens: first_number(
            value,
            &[
                "cache_creation_input_tokens",
                "cacheCreationInputTokens",
                "cache_write_input_tokens",
                "cacheWriteInputTokens",
                "cache_creation",
                "cacheWrite",
                "cache_write",
            ],
        )
        .unwrap_or_default(),
        output_tokens: first_number(value, &["output_tokens", "outputTokens", "output"])
            .unwrap_or_default(),
        reasoning_tokens: first_number(
            value,
            &[
                "reasoning_output_tokens",
                "reasoning_tokens",
                "reasoningTokens",
                "reasoning",
            ],
        )
        .unwrap_or_default(),
        total_tokens: first_number(value, &["total_tokens", "totalTokens", "total"])
            .unwrap_or_default(),
    };
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
    let mut files = Vec::new();
    if !root.exists() {
        return Ok(files);
    }
    walk_jsonl_inner(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn walk_jsonl_inner(path: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_file() {
        if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            files.push(path.to_path_buf());
        }
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        walk_jsonl_inner(&entry?.path(), files)?;
    }
    Ok(())
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
