use std::{fs, path::PathBuf};

use anyhow::Result;
use llmeter_core::{
    Provider, ProviderDetection, SourceFile, SourceFormat, TokenCounts, parse_timestamp,
};
use serde_json::Value;

use super::{
    ParsedSnapshot, ParsedUsage, ProviderAdapter, data_status, editor_global_storage_dirs, home_dir,
};

const CLINE_PARSER_VERSION: u32 = 1;
/// Cline records the task's working directory under
/// `cwdOnTaskInitialization`; the Roo Code fork renamed it to `workspace`.
pub(crate) const HISTORY_CWD_KEYS: &[&str] = &["cwdOnTaskInitialization", "workspace"];
const CLINE_VSCODE_STORAGE_ID: &str = "saoudrizwan.claude-dev";

/// Cline's per-task history record, shared (with small field variations) by
/// the Roo Code fork. `tokensIn` is uncached input; the cache buckets and
/// output are reported separately and `totalCost` is USD.
#[derive(Clone, Debug)]
pub(crate) struct HistoryItem {
    pub id: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub counts: TokenCounts,
    pub cost_usd: Option<f64>,
    pub model: Option<String>,
    pub project_path: Option<PathBuf>,
}

pub(crate) fn parse_history_item(value: &Value, cwd_keys: &[&str]) -> Option<HistoryItem> {
    let number = |key: &str| value.get(key).and_then(Value::as_f64).unwrap_or_default();
    let tokens_in = number("tokensIn");
    let tokens_out = number("tokensOut");
    let cache_writes = number("cacheWrites");
    let cache_reads = number("cacheReads");
    if tokens_in == 0.0 && tokens_out == 0.0 && cache_writes == 0.0 && cache_reads == 0.0 {
        return None;
    }
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    let input_tokens = tokens_in as u64;
    let cached_input_tokens = cache_reads as u64;
    let cache_creation_input_tokens = cache_writes as u64;
    let output_tokens = tokens_out as u64;
    Some(HistoryItem {
        id,
        timestamp: parse_timestamp(value.get("ts")),
        counts: TokenCounts {
            input_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
            output_tokens,
            reasoning_tokens: 0,
            total_tokens: input_tokens
                .saturating_add(cached_input_tokens)
                .saturating_add(cache_creation_input_tokens)
                .saturating_add(output_tokens),
        },
        cost_usd: number("totalCost").max(0.0).into(),
        model: value
            .get("modelId")
            .and_then(Value::as_str)
            .map(str::to_string),
        project_path: cwd_keys
            .iter()
            .find_map(|key| value.get(*key).and_then(Value::as_str))
            .map(PathBuf::from),
    })
}

pub(crate) fn history_item_usage(item: &HistoryItem, session_id: Option<String>) -> ParsedUsage {
    ParsedUsage {
        counts: item.counts,
        cumulative_snapshot: None,
        timestamp: item.timestamp,
        model: item.model.clone(),
        session_id,
        project_name: super::project_name(item.project_path.as_deref()),
        project_path: item.project_path.clone(),
        source_event_id: Some(item.id.clone()),
        reported_cost_usd: item.cost_usd.filter(|cost| *cost > 0.0),
    }
}

/// Current Cline installs keep task history under `~/.cline/data`; installs
/// from before that SDK migration still write the same JSON array into the
/// VS Code extension's globalStorage. Both are read.
#[derive(Clone, Debug)]
pub struct ClineAdapter {
    roots: Vec<PathBuf>,
}

impl Default for ClineAdapter {
    fn default() -> Self {
        // Cline resolves CLINE_DATA_DIR as the data dir itself but CLINE_DIR
        // as the parent that gains a `/data` segment.
        let data_root = std::env::var_os("CLINE_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("CLINE_DIR").map(|dir| PathBuf::from(dir).join("data")))
            .unwrap_or_else(|| home_dir().join(".cline").join("data"));
        let mut roots = vec![data_root];
        roots.extend(
            editor_global_storage_dirs()
                .into_iter()
                .map(|storage| storage.join(CLINE_VSCODE_STORAGE_ID)),
        );
        Self { roots }
    }
}

impl ClineAdapter {
    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    fn history_files(&self) -> Vec<PathBuf> {
        self.roots
            .iter()
            .map(|root| root.join("state").join("taskHistory.json"))
            .filter(|path| path.is_file())
            .collect()
    }
}

impl ProviderAdapter for ClineAdapter {
    fn provider(&self) -> Provider {
        Provider::Cline
    }

    fn parser_version(&self) -> u32 {
        CLINE_PARSER_VERSION
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn detect(&self) -> Result<ProviderDetection> {
        Ok(data_status(
            Provider::Cline,
            self.roots.clone(),
            !self.history_files().is_empty(),
            None,
        ))
    }

    fn discover_sources(&self) -> Result<Vec<SourceFile>> {
        Ok(self
            .history_files()
            .into_iter()
            .map(|path| SourceFile {
                path,
                provider: Provider::Cline,
                format: SourceFormat::Snapshot,
                session_id: None,
                project_path: None,
                project_name: None,
            })
            .collect())
    }

    fn parse_line(&self, _source: &SourceFile, _line: &[u8]) -> Result<Option<ParsedUsage>> {
        Ok(None)
    }

    fn parse_snapshot(&self, source: &SourceFile) -> Result<ParsedSnapshot> {
        let bytes = fs::read(&source.path)?;
        let items: Vec<Value> = match serde_json::from_slice(&bytes) {
            Ok(items) => items,
            Err(error) => {
                tracing::warn!(
                    path = %source.path.display(),
                    error = %error,
                    "skipping malformed Cline task history"
                );
                Vec::new()
            }
        };
        let usages = items
            .iter()
            .filter_map(|item| parse_history_item(item, HISTORY_CWD_KEYS))
            .map(|item| {
                let session_id = Some(item.id.clone());
                history_item_usage(&item, session_id)
            })
            .collect();
        Ok(ParsedSnapshot {
            usages,
            ..ParsedSnapshot::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use llmeter_core::ProviderStatus;

    use super::*;

    #[test]
    fn reads_task_history_totals_per_task() {
        let home = std::env::temp_dir().join(format!("llmeter-cline-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let state = home.join(".cline").join("data").join("state");
        fs::create_dir_all(&state).unwrap();
        fs::write(
            state.join("taskHistory.json"),
            concat!(
                r#"[{"id":"task-1","ts":1784102040274,"task":"fix bug","tokensIn":4200,"tokensOut":180,"cacheWrites":900,"cacheReads":12000,"totalCost":0.0125,"cwdOnTaskInitialization":"/tmp/project","modelId":"claude-sonnet-5","apiProvider":"anthropic"},"#,
                r#"{"id":"task-2","ts":1784103040274,"task":"empty","tokensIn":0,"tokensOut":0,"cacheWrites":0,"cacheReads":0,"totalCost":0}]"#
            ),
        )
        .unwrap();

        let adapter = ClineAdapter::with_roots(vec![home.join(".cline").join("data")]);
        assert_eq!(adapter.detect().unwrap().status, ProviderStatus::DataFound);
        let source = adapter.discover_sources().unwrap().remove(0);
        let snapshot = adapter.parse_snapshot(&source).unwrap();
        assert_eq!(snapshot.usages.len(), 1);
        let usage = &snapshot.usages[0];
        assert_eq!(usage.counts.input_tokens, 4_200);
        assert_eq!(usage.counts.cached_input_tokens, 12_000);
        assert_eq!(usage.counts.cache_creation_input_tokens, 900);
        assert_eq!(usage.counts.output_tokens, 180);
        assert_eq!(usage.counts.total_tokens, 17_280);
        assert_eq!(usage.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(usage.session_id.as_deref(), Some("task-1"));
        assert_eq!(usage.source_event_id.as_deref(), Some("task-1"));
        assert_eq!(usage.reported_cost_usd, Some(0.0125));
        assert_eq!(
            usage.project_path.as_deref(),
            Some(std::path::Path::new("/tmp/project"))
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn reads_legacy_vscode_global_storage_too() {
        let home =
            std::env::temp_dir().join(format!("llmeter-cline-legacy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let legacy = home
            .join("Code")
            .join("User")
            .join("globalStorage")
            .join(CLINE_VSCODE_STORAGE_ID)
            .join("state");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(
            legacy.join("taskHistory.json"),
            r#"[{"id":"task-7","ts":1784102040274,"tokensIn":10,"tokensOut":2,"cacheWrites":0,"cacheReads":0,"totalCost":0.0001,"cwdOnTaskInitialization":"/tmp/old"}]"#,
        )
        .unwrap();

        let adapter = ClineAdapter::with_roots(vec![
            home.join(".cline").join("data"),
            legacy.parent().unwrap().to_path_buf(),
        ]);
        assert_eq!(adapter.detect().unwrap().status, ProviderStatus::DataFound);
        let sources = adapter.discover_sources().unwrap();
        assert_eq!(sources.len(), 1);
        let snapshot = adapter.parse_snapshot(&sources[0]).unwrap();
        assert_eq!(snapshot.usages.len(), 1);
        assert_eq!(snapshot.usages[0].session_id.as_deref(), Some("task-7"));
        assert_eq!(snapshot.usages[0].counts.total_tokens, 12);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn missing_history_reports_not_installed() {
        let home =
            std::env::temp_dir().join(format!("llmeter-cline-absent-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let adapter = ClineAdapter::with_roots(vec![home.join(".cline").join("data")]);
        assert_eq!(
            adapter.detect().unwrap().status,
            ProviderStatus::NotInstalled
        );
        assert!(adapter.discover_sources().unwrap().is_empty());
    }
}
