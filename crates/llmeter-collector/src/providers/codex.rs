use std::path::PathBuf;

use anyhow::Result;
use llmeter_core::{Provider, ProviderDetection, SourceFile, SourceFormat, SourceMetadata};
use serde_json::Value;

use super::{
    ParsedUsage, ProviderAdapter, counts_from_usage, data_status, home_dir, json_value,
    jsonl_exists, model, nested, project_name, project_path, session_id, source_event_id,
    timestamp, usage_snapshot, walk_jsonl,
};

const CODEX_PARSER_VERSION: u32 = 2;

#[derive(Clone, Debug)]
pub struct CodexAdapter {
    home: PathBuf,
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self { home: home_dir() }
    }
}

impl CodexAdapter {
    pub fn with_home(home: PathBuf) -> Self {
        Self { home }
    }

    fn codex_root(&self) -> PathBuf {
        self.home.join(".codex")
    }

    fn apply_metadata(&self, value: &Value, metadata: &mut SourceMetadata) {
        let payload = nested(value, &["payload"]).unwrap_or(value);
        let record_type = nested(value, &["type"]).and_then(Value::as_str);
        let direct_model = nested(payload, &["model"]).and_then(Value::as_str);

        if !matches!(record_type, Some("session_meta" | "turn_context")) && direct_model.is_none() {
            return;
        }

        if let Some(model) = direct_model {
            metadata.model = Some(model.to_string());
        }
        if let Some(session_id) = session_id(payload).or_else(|| {
            (record_type == Some("session_meta"))
                .then(|| nested(payload, &["id"]).and_then(Value::as_str))
                .flatten()
                .map(str::to_string)
        }) {
            metadata.session_id = Some(session_id);
        }
        if let Some(path) = project_path(payload) {
            metadata.project_name = project_name(Some(&path));
            metadata.project_path = Some(path);
        }
    }

    fn parse_value(&self, source: &SourceFile, value: &Value) -> Result<Option<ParsedUsage>> {
        let payload = nested(value, &["payload"]).unwrap_or(value);
        let info = nested(payload, &["info"]).unwrap_or(payload);

        // Codex emits last_token_usage alongside total_token_usage. The last
        // snapshot is already the per-turn delta and is preferred when both
        // exist, avoiding a second cumulative-delta calculation.
        let last_usage = nested(info, &["last_token_usage"])
            .or_else(|| nested(info, &["last_usage"]))
            .or_else(|| nested(payload, &["last_token_usage"]))
            .or_else(|| nested(value, &["last_token_usage"]))
            .and_then(|usage| counts_from_usage(usage, false));
        let total_usage = nested(info, &["total_token_usage"])
            .or_else(|| nested(info, &["total_usage"]))
            .or_else(|| nested(payload, &["total_token_usage"]))
            .or_else(|| nested(value, &["total_token_usage"]))
            .and_then(usage_snapshot);
        let generic_usage = super::object_with_usage(value);

        let (counts, cumulative_snapshot) = if let Some(counts) = last_usage {
            (counts, None)
        } else if let Some(snapshot) = total_usage {
            (snapshot.into(), Some(snapshot))
        } else if let Some(usage) = generic_usage.and_then(counts_from_codex_usage) {
            (usage.0, usage.1)
        } else {
            return Ok(None);
        };

        let project_path = project_path(value);
        Ok(Some(ParsedUsage {
            counts,
            cumulative_snapshot,
            timestamp: timestamp(value),
            model: model(payload).or_else(|| model(value)),
            session_id: session_id(value).or_else(|| source.session_id.clone()),
            project_name: project_name(project_path.as_deref()),
            project_path,
            source_event_id: source_event_id(payload).or_else(|| source_event_id(value)),
            reported_cost_usd: None,
        }))
    }
}

impl ProviderAdapter for CodexAdapter {
    fn provider(&self) -> Provider {
        Provider::Codex
    }

    fn parser_version(&self) -> u32 {
        CODEX_PARSER_VERSION
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        let root = self.codex_root();
        vec![root.join("sessions"), root.join("archived_sessions")]
    }

    fn detect(&self) -> Result<ProviderDetection> {
        let root = self.codex_root();
        let sessions = root.join("sessions");
        let archived = root.join("archived_sessions");
        Ok(data_status(
            Provider::Codex,
            vec![root, sessions.clone(), archived.clone()],
            jsonl_exists(&sessions)? || jsonl_exists(&archived)?,
            None,
        ))
    }

    fn discover_sources(&self) -> Result<Vec<SourceFile>> {
        let root = self.codex_root();
        let mut files = walk_jsonl(&root.join("sessions"))?;
        files.extend(walk_jsonl(&root.join("archived_sessions"))?);
        files.sort();
        Ok(files
            .into_iter()
            .map(|path| {
                let session_id = path
                    .file_stem()
                    .map(|value| value.to_string_lossy().to_string());
                SourceFile {
                    path,
                    provider: Provider::Codex,
                    format: SourceFormat::Jsonl,
                    session_id,
                    project_path: None,
                    project_name: None,
                }
            })
            .collect())
    }

    fn update_source_metadata(
        &self,
        _source: &SourceFile,
        line: &[u8],
        metadata: &mut SourceMetadata,
    ) -> Result<()> {
        self.apply_metadata(&json_value(line)?, metadata);
        Ok(())
    }

    fn parse_line(&self, source: &SourceFile, line: &[u8]) -> Result<Option<ParsedUsage>> {
        self.parse_value(source, &json_value(line)?)
    }

    fn ingest_line(
        &self,
        source: &SourceFile,
        line: &[u8],
        metadata: &mut SourceMetadata,
    ) -> Result<Option<ParsedUsage>> {
        let value = json_value(line)?;
        self.apply_metadata(&value, metadata);
        self.parse_value(source, &value)
    }
}

fn counts_from_codex_usage(
    value: &Value,
) -> Option<(
    llmeter_core::TokenCounts,
    Option<llmeter_core::UsageSnapshot>,
)> {
    let counts = counts_from_usage(value, false)?;
    let is_cumulative = value.as_object().is_some_and(|object| {
        object
            .keys()
            .any(|key| key.eq_ignore_ascii_case("token_count"))
    });
    if is_cumulative {
        let snapshot = llmeter_core::UsageSnapshot::from(counts);
        Some((snapshot.into(), Some(snapshot)))
    } else {
        Some((counts, None))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn prefers_last_usage_over_total_usage() {
        let adapter = CodexAdapter::with_home(PathBuf::from("/tmp/does-not-matter"));
        let source = SourceFile {
            path: PathBuf::from("/tmp/session.jsonl"),
            provider: Provider::Codex,
            format: SourceFormat::Jsonl,
            session_id: Some("session".into()),
            project_path: None,
            project_name: None,
        };
        let line = br#"{"type":"event_msg","timestamp":"2026-08-14T00:00:00Z","payload":{"model":"gpt-5.4","info":{"last_token_usage":{"input_tokens":10,"output_tokens":3,"total_tokens":13},"total_token_usage":{"input_tokens":100,"output_tokens":30,"total_tokens":130}}}}"#;
        let parsed = adapter.parse_line(&source, line).unwrap().unwrap();
        assert_eq!(parsed.counts.total_tokens, 13);
        assert!(parsed.cumulative_snapshot.is_none());
    }
}
