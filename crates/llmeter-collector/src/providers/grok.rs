use std::{fs, path::PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use llmeter_core::{Provider, ProviderDetection, SourceFile, SourceFormat, TokenCounts};
use serde_json::Value;

use super::{
    ParsedUsage, ProviderAdapter, data_status, home_dir, json_value, nested, project_name,
};

const GROK_PARSER_VERSION: u32 = 2;
const USD_TICKS_PER_DOLLAR: f64 = 10_000_000_000.0;

#[derive(Clone, Debug)]
pub struct GrokAdapter {
    root: PathBuf,
}

impl Default for GrokAdapter {
    fn default() -> Self {
        let root = std::env::var_os("GROK_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".grok"));
        Self { root }
    }
}

impl GrokAdapter {
    pub fn with_home(home: PathBuf) -> Self {
        Self {
            root: home.join(".grok"),
        }
    }

    fn sessions_root(&self) -> PathBuf {
        self.root.join("sessions")
    }

    fn files(&self) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        collect_updates(&self.sessions_root(), &mut files)?;
        files.sort();
        Ok(files)
    }
}

impl ProviderAdapter for GrokAdapter {
    fn provider(&self) -> Provider {
        Provider::Grok
    }

    fn parser_version(&self) -> u32 {
        GROK_PARSER_VERSION
    }
    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![self.sessions_root()]
    }

    fn detect(&self) -> Result<ProviderDetection> {
        let sessions = self.sessions_root();
        Ok(data_status(
            Provider::Grok,
            vec![self.root.clone(), sessions],
            !self.files()?.is_empty(),
            None,
        ))
    }

    fn discover_sources(&self) -> Result<Vec<SourceFile>> {
        self.files()?
            .into_iter()
            .map(|path| {
                let session_dir = path
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("Grok updates file has no session directory"))?;
                let summary = read_summary(session_dir.join("summary.json"));
                let session_id = summary
                    .as_ref()
                    .and_then(|value| nested(value, &["info", "id"]))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        session_dir
                            .file_name()
                            .map(|value| value.to_string_lossy().to_string())
                    });
                let project_path = summary
                    .as_ref()
                    .and_then(|value| nested(value, &["info", "cwd"]))
                    .and_then(Value::as_str)
                    .map(PathBuf::from);
                Ok(SourceFile {
                    path,
                    provider: Provider::Grok,
                    format: SourceFormat::Jsonl,
                    session_id,
                    project_name: project_name(project_path.as_deref()),
                    project_path,
                })
            })
            .collect()
    }

    fn parse_line(&self, source: &SourceFile, line: &[u8]) -> Result<Option<ParsedUsage>> {
        let value = json_value(line)?;
        let Some(update) = nested(&value, &["params", "update"]) else {
            return Ok(None);
        };
        let Some(usage) = update.get("usage") else {
            return Ok(None);
        };
        let Some(counts) = grok_counts(usage) else {
            return Ok(None);
        };
        let model_usage = usage.get("modelUsage").and_then(Value::as_object);
        let model = model_usage
            .filter(|models| models.len() == 1)
            .and_then(|models| models.keys().next().cloned());
        let reported_cost_usd = ticks(usage)
            .or_else(|| model_usage.and_then(single_model_ticks))
            .filter(|cost| *cost > 0.0);

        Ok(Some(ParsedUsage {
            counts,
            cumulative_snapshot: None,
            timestamp: grok_timestamp(&value),
            model,
            session_id: source.session_id.clone(),
            project_path: source.project_path.clone(),
            project_name: source.project_name.clone(),
            source_event_id: update
                .get("prompt_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            reported_cost_usd,
        }))
    }
}

fn collect_updates(path: &std::path::Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_file() {
        if path.file_name().is_some_and(|name| name == "updates.jsonl") {
            files.push(path.to_path_buf());
        }
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            collect_updates(&entry?.path(), files)?;
        }
    }
    Ok(())
}

fn read_summary(path: PathBuf) -> Option<Value> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

fn grok_counts(usage: &Value) -> Option<TokenCounts> {
    let full_input = number(usage, &["inputTokens", "input_tokens"]);
    let cached_input_tokens = number(
        usage,
        &[
            "cachedReadTokens",
            "cacheReadInputTokens",
            "cache_read_input_tokens",
        ],
    );
    let cache_creation_input_tokens = number(
        usage,
        &[
            "cachedWriteTokens",
            "cacheCreationInputTokens",
            "cache_creation_input_tokens",
        ],
    );
    let output_tokens = number(usage, &["outputTokens", "output_tokens"]);
    let reasoning_tokens = number(usage, &["reasoningTokens", "reasoning_tokens"]);
    let input_tokens = full_input
        .saturating_sub(cached_input_tokens)
        .saturating_sub(cache_creation_input_tokens);
    let total_tokens = number(usage, &["totalTokens", "total_tokens"]).max(
        input_tokens
            .saturating_add(cached_input_tokens)
            .saturating_add(cache_creation_input_tokens)
            .saturating_add(output_tokens),
    );
    let counts = TokenCounts {
        input_tokens,
        cached_input_tokens,
        cache_creation_input_tokens,
        output_tokens,
        reasoning_tokens,
        total_tokens,
    };
    (!counts.is_zero()).then_some(counts)
}

fn number(value: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
        .unwrap_or_default()
}

fn ticks(value: &Value) -> Option<f64> {
    let value = number(value, &["costUsdTicks", "totalCostUsdTicks"]);
    (value > 0).then_some(value as f64 / USD_TICKS_PER_DOLLAR)
}

fn single_model_ticks(models: &serde_json::Map<String, Value>) -> Option<f64> {
    (models.len() == 1)
        .then(|| models.values().next())
        .flatten()
        .and_then(ticks)
}

fn grok_timestamp(value: &Value) -> DateTime<Utc> {
    value
        .get("timestamp")
        .and_then(Value::as_i64)
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        .or_else(|| {
            nested(value, &["params", "_meta", "agentTimestampMs"])
                .and_then(Value::as_i64)
                .and_then(DateTime::from_timestamp_millis)
        })
        .unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use chrono::Utc;
    use llmeter_core::{FileCursor, UsageEvent};
    use llmeter_storage::{Database, UsageRepository};

    use super::*;
    use crate::sync::SyncEngine;

    const GROK_USAGE_LINE: &[u8] = br#"{"timestamp":1786949023,"params":{"_meta":{"agentTimestampMs":1786949023318},"update":{"sessionUpdate":"plan","prompt_id":"prompt-1","usage":{"inputTokens":64257,"outputTokens":2144,"totalTokens":66401,"cachedReadTokens":51072,"reasoningTokens":249,"costUsdTicks":126890500,"modelUsage":{"grok-4.6-build":{"inputTokens":64257,"outputTokens":2144,"totalTokens":66401,"cachedReadTokens":51072,"reasoningTokens":249,"modelCalls":3}}}}}}"#;

    #[test]
    fn parses_acp_prompt_usage_without_double_counting_cache() {
        let adapter = GrokAdapter::with_home(PathBuf::from("/tmp"));
        let source = SourceFile {
            path: PathBuf::from("/tmp/updates.jsonl"),
            provider: Provider::Grok,
            format: SourceFormat::Jsonl,
            session_id: Some("grok-session".into()),
            project_path: Some(PathBuf::from("/tmp/project")),
            project_name: Some("project".into()),
        };
        let parsed = adapter
            .parse_line(&source, GROK_USAGE_LINE)
            .unwrap()
            .unwrap();
        assert_eq!(parsed.counts.input_tokens, 13_185);
        assert_eq!(parsed.counts.cached_input_tokens, 51_072);
        assert_eq!(parsed.counts.output_tokens, 2_144);
        assert_eq!(parsed.counts.total_tokens, 66_401);
        assert_eq!(parsed.model.as_deref(), Some("grok-4.6-build"));
        assert_eq!(parsed.source_event_id.as_deref(), Some("prompt-1"));
        assert_eq!(parsed.reported_cost_usd, Some(0.01268905));
    }

    #[test]
    fn discovers_only_session_updates_and_reads_summary_metadata() {
        let home =
            std::env::temp_dir().join(format!("llmeter-grok-adapter-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let session = home
            .join(".grok")
            .join("sessions")
            .join("%2Ftmp%2Fproject")
            .join("session-id");
        fs::create_dir_all(&session).unwrap();
        fs::write(
            session.join("summary.json"),
            r#"{"info":{"id":"session-id","cwd":"/tmp/project"}}"#,
        )
        .unwrap();
        fs::write(session.join("updates.jsonl"), "\n").unwrap();
        fs::write(session.join("chat_history.jsonl"), "\n").unwrap();

        let sources = GrokAdapter::with_home(home.clone())
            .discover_sources()
            .unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].session_id.as_deref(), Some("session-id"));
        assert_eq!(sources[0].project_name.as_deref(), Some("project"));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn parser_upgrade_rebuilds_reported_costs() {
        let home = std::env::temp_dir().join(format!(
            "llmeter-grok-parser-upgrade-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&home);
        let session = home
            .join(".grok")
            .join("sessions")
            .join("%2Ftmp%2Fproject")
            .join("session-id");
        fs::create_dir_all(&session).unwrap();
        fs::write(
            session.join("summary.json"),
            r#"{"info":{"id":"session-id","cwd":"/tmp/project"}}"#,
        )
        .unwrap();
        let source_path = session.join("updates.jsonl");
        let mut line = GROK_USAGE_LINE.to_vec();
        line.push(b'\n');
        fs::write(&source_path, line).unwrap();

        let database = Database::open_in_memory().unwrap();
        database
            .insert_usage_events(&[UsageEvent {
                id: "old-grok-event".into(),
                provider: Provider::Grok,
                model: Some("grok-4.6-build".into()),
                session_id: Some("session-id".into()),
                project_path: Some(PathBuf::from("/tmp/project")),
                project_name: Some("project".into()),
                timestamp: Utc::now(),
                input_tokens: 999,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                output_tokens: 0,
                reasoning_tokens: 0,
                total_tokens: 999,
                reported_cost_usd: None,
                estimated_cost_usd: None,
                source_file: Some(source_path.clone()),
                source_event_id: Some("old-prompt".into()),
            }])
            .unwrap();
        database
            .upsert_cursor(&FileCursor::new(source_path, Provider::Grok, 1))
            .unwrap();

        let engine = SyncEngine::with_adapters(
            database.clone(),
            vec![Box::new(GrokAdapter::with_home(home.clone()))],
        );
        let result = engine.sync_all().unwrap();
        assert_eq!(result.events_inserted, 1);

        let overview = UsageRepository::new(database)
            .get_overview(
                chrono::DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
                Utc::now() + chrono::Duration::days(365),
            )
            .unwrap();
        assert_eq!(overview.total_tokens, 66_401);
        assert_eq!(overview.estimated_cost_usd, Some(0.01268905));
        let _ = fs::remove_dir_all(home);
    }
}
