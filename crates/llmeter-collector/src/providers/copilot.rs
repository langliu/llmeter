use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use llmeter_core::{
    Provider, ProviderDetection, SourceFile, SourceFormat, TokenCounts, parse_timestamp,
};
use serde_json::Value;

use super::{ParsedSnapshot, ParsedUsage, ProviderAdapter, SnapshotPolicy, data_status, home_dir};

const COPILOT_PARSER_VERSION: u32 = 1;
/// Copilot event logs are session-scoped and re-read wholesale on change;
/// refuse absurdly large files instead of stalling the sync loop.
const MAX_EVENTS_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct CopilotAdapter {
    root: PathBuf,
}

impl Default for CopilotAdapter {
    fn default() -> Self {
        let root = std::env::var_os("COPILOT_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".copilot"));
        Self { root }
    }
}

impl CopilotAdapter {
    pub fn with_home(home: PathBuf) -> Self {
        Self {
            root: home.join(".copilot"),
        }
    }

    fn sessions_root(&self) -> PathBuf {
        self.root.join("session-state")
    }

    fn event_files(&self) -> Result<Vec<PathBuf>> {
        Ok(super::walk_matching(&self.sessions_root(), |path, _| {
            path.file_name().is_some_and(|name| name == "events.jsonl")
        })?)
    }
}

/// One `session.shutdown`'s per-model counters. Copilot reports cumulative
/// totals that survive `copilot resume`, so consecutive shutdowns are diffed
/// and only the increments become events.
#[derive(Clone, Copy, Debug, Default)]
struct ModelTotals {
    input_with_cache: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
}

impl ModelTotals {
    fn parse(usage: &Value) -> Self {
        Self {
            input_with_cache: field(usage, &["inputTokens"]),
            cache_read: field(usage, &["cacheReadTokens"]),
            cache_write: field(usage, &["cacheWriteTokens"]),
            output: field(usage, &["outputTokens"]),
        }
    }

    fn delta_since(self, previous: Self) -> TokenCounts {
        // inputTokens includes both cache buckets; reasoningTokens is a
        // subset of output and therefore never added into the total.
        let cached_input_tokens = self.cache_read.saturating_sub(previous.cache_read);
        let cache_creation_input_tokens = self.cache_write.saturating_sub(previous.cache_write);
        let input_with_cache = self
            .input_with_cache
            .saturating_sub(previous.input_with_cache);
        let input_tokens = input_with_cache
            .saturating_sub(cached_input_tokens)
            .saturating_sub(cache_creation_input_tokens);
        let output_tokens = self.output.saturating_sub(previous.output);
        TokenCounts {
            input_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
            output_tokens,
            reasoning_tokens: 0,
            total_tokens: input_with_cache.saturating_add(output_tokens),
        }
    }
}

fn field(value: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
        .unwrap_or_default()
}

impl ProviderAdapter for CopilotAdapter {
    fn provider(&self) -> Provider {
        Provider::Copilot
    }

    fn parser_version(&self) -> u32 {
        COPILOT_PARSER_VERSION
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![self.sessions_root()]
    }

    fn detect(&self) -> Result<ProviderDetection> {
        let roots = vec![self.root.clone(), self.sessions_root()];
        Ok(data_status(
            Provider::Copilot,
            roots,
            !self.event_files()?.is_empty(),
            None,
        ))
    }

    fn discover_sources(&self) -> Result<Vec<SourceFile>> {
        Ok(self
            .event_files()?
            .into_iter()
            .map(|path| SourceFile {
                session_id: path
                    .parent()
                    .and_then(Path::file_name)
                    .map(|value| value.to_string_lossy().to_string()),
                project_path: None,
                project_name: None,
                provider: Provider::Copilot,
                format: SourceFormat::Snapshot,
                path,
            })
            .collect())
    }

    fn parse_line(&self, _source: &SourceFile, _line: &[u8]) -> Result<Option<ParsedUsage>> {
        Ok(None)
    }

    fn parse_snapshot(&self, source: &SourceFile) -> Result<ParsedSnapshot> {
        let empty = ParsedSnapshot {
            usages: Vec::new(),
            policy: SnapshotPolicy::Upsert,
            scope: None,
        };
        if fs::metadata(&source.path)
            .map(|metadata| metadata.len())
            .unwrap_or(0)
            > MAX_EVENTS_BYTES
        {
            return Ok(empty);
        }
        let bytes = fs::read(&source.path)?;
        let session = source
            .session_id
            .clone()
            .unwrap_or_else(|| "session".to_string());
        let mut usages = Vec::new();
        let mut previous: HashMap<String, ModelTotals> = HashMap::new();
        for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
            let Ok(value) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) != Some("session.shutdown") {
                continue;
            }
            let Some(metrics) = value
                .pointer("/data/modelMetrics")
                .and_then(Value::as_object)
            else {
                continue;
            };
            let timestamp = parse_timestamp(value.get("timestamp"));
            for (model, metric) in metrics {
                let Some(usage) = metric.get("usage") else {
                    continue;
                };
                let current = ModelTotals::parse(usage);
                let counts = current.delta_since(previous.get(model).copied().unwrap_or_default());
                previous.insert(model.clone(), current);
                if counts.is_zero() {
                    continue;
                }
                usages.push(ParsedUsage {
                    counts,
                    cumulative_snapshot: None,
                    timestamp,
                    model: Some(model.clone()),
                    session_id: Some(session.clone()),
                    project_path: None,
                    project_name: None,
                    // Shutdown counters are cumulative, so the shutdown's
                    // position in the file keys the delta stably.
                    source_event_id: Some(format!("{session}:{index}:{model}")),
                    reported_cost_usd: None,
                });
            }
        }
        Ok(ParsedSnapshot {
            usages,
            policy: SnapshotPolicy::Upsert,
            scope: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use chrono::Utc;

    use super::*;

    // Real session.shutdown payload shape (Copilot CLI 1.0.68, trimmed).
    const SHUTDOWN_LINE: &[u8] = br#"{"type":"session.shutdown","timestamp":"2026-04-15T09:52:27.352Z","data":{"shutdownType":"routine","sessionStartTime":1784102040274,"modelMetrics":{"claude-sonnet-5":{"requests":{"count":2,"cost":2},"usage":{"inputTokens":71282,"outputTokens":345,"cacheReadTokens":35495,"cacheWriteTokens":35783,"reasoningTokens":31}}}}}"#;

    #[test]
    fn diffs_cumulative_shutdown_counters_per_model() {
        let home =
            std::env::temp_dir().join(format!("llmeter-copilot-adapter-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let session = home.join(".copilot").join("session-state").join("abc-123");
        fs::create_dir_all(&session).unwrap();
        let mut first = SHUTDOWN_LINE.to_vec();
        first.push(b'\n');
        // A second shutdown after a resume adds 1k uncached input + 100 output.
        let second = br#"{"type":"session.shutdown","timestamp":"2026-04-15T11:00:00.000Z","data":{"modelMetrics":{"claude-sonnet-5":{"usage":{"inputTokens":72282,"outputTokens":445,"cacheReadTokens":35495,"cacheWriteTokens":35783,"reasoningTokens":31}}}}}"#;
        let mut second = second.to_vec();
        second.push(b'\n');
        let mut file = first.clone();
        file.extend_from_slice(&second);
        fs::write(session.join("events.jsonl"), file).unwrap();

        let adapter = CopilotAdapter::with_home(home.clone());
        let detection = adapter.detect().unwrap();
        assert_eq!(detection.status, llmeter_core::ProviderStatus::DataFound);
        let source = adapter.discover_sources().unwrap().remove(0);
        assert_eq!(source.session_id.as_deref(), Some("abc-123"));

        let snapshot = adapter.parse_snapshot(&source).unwrap();
        assert_eq!(snapshot.usages.len(), 2);
        let first = &snapshot.usages[0];
        // inputTokens includes the cache buckets.
        assert_eq!(first.counts.input_tokens, 4);
        assert_eq!(first.counts.cached_input_tokens, 35_495);
        assert_eq!(first.counts.cache_creation_input_tokens, 35_783);
        assert_eq!(first.counts.output_tokens, 345);
        assert_eq!(first.counts.total_tokens, 71_627);
        assert_eq!(first.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(first.session_id.as_deref(), Some("abc-123"));
        assert_eq!(
            first.timestamp,
            chrono::DateTime::parse_from_rfc3339("2026-04-15T09:52:27.352Z")
                .unwrap()
                .with_timezone(&Utc)
        );

        let second = &snapshot.usages[1];
        assert_eq!(second.counts.input_tokens, 1_000);
        assert_eq!(second.counts.cached_input_tokens, 0);
        assert_eq!(second.counts.output_tokens, 100);
        assert_eq!(second.counts.total_tokens, 1_100);
        assert_ne!(
            second.source_event_id, first.source_event_id,
            "resumed shutdown deltas need distinct ids"
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn ignores_non_shutdown_events() {
        let home =
            std::env::temp_dir().join(format!("llmeter-copilot-events-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let session = home.join(".copilot").join("session-state").join("s");
        fs::create_dir_all(&session).unwrap();
        fs::write(
            session.join("events.jsonl"),
            concat!(
                r#"{"type":"user.message","timestamp":"2026-04-15T09:00:00.000Z"}"#,
                "\n",
                r#"{"type":"assistant.message","timestamp":"2026-04-15T09:00:05.000Z","data":{"outputTokens":42}}"#,
                "\n",
                r#"{"type":"session.shutdown","timestamp":"2026-04-15T09:00:10.000Z","data":{}}"#,
                "\n",
            ),
        )
        .unwrap();
        let adapter = CopilotAdapter::with_home(home.clone());
        let source = adapter
            .discover_sources()
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let snapshot = adapter.parse_snapshot(&source).unwrap();
        assert!(snapshot.usages.is_empty());
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn fixture_without_sessions_reports_not_installed() {
        let home =
            std::env::temp_dir().join(format!("llmeter-copilot-absent-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let adapter = CopilotAdapter::with_home(PathBuf::from(&home));
        assert_eq!(
            adapter.detect().unwrap().status,
            llmeter_core::ProviderStatus::NotInstalled
        );
        assert!(adapter.discover_sources().unwrap().is_empty());
    }
}
