use std::path::PathBuf;

use anyhow::Result;
use llmeter_core::{Provider, ProviderDetection, SourceFile, SourceFormat, SourceMetadata};
use serde_json::Value;

use super::{
    ParsedUsage, ProviderAdapter, counts_from_usage, data_status, home_dir, json_value,
    project_name, project_path, session_id, source_event_id, timestamp, walk_jsonl,
};

const PI_PARSER_VERSION: u32 = 4;

#[derive(Clone, Debug)]
pub struct PiAdapter {
    home: PathBuf,
}

impl Default for PiAdapter {
    fn default() -> Self {
        Self { home: home_dir() }
    }
}

impl PiAdapter {
    pub fn with_home(home: PathBuf) -> Self {
        Self { home }
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.home.join(".pi").join("agent").join("sessions")]
    }

    fn files(&self) -> Result<Vec<PathBuf>> {
        Ok(walk_jsonl(&self.roots()[0])?)
    }
}

impl ProviderAdapter for PiAdapter {
    fn provider(&self) -> Provider {
        Provider::Pi
    }

    fn parser_version(&self) -> u32 {
        PI_PARSER_VERSION
    }
    fn watch_roots(&self) -> Vec<PathBuf> {
        self.roots()
    }

    fn update_source_metadata(
        &self,
        _source: &SourceFile,
        line: &[u8],
        metadata: &mut SourceMetadata,
    ) -> Result<()> {
        apply_pi_metadata(&json_value(line)?, metadata);
        Ok(())
    }

    fn detect(&self) -> Result<ProviderDetection> {
        let roots = self.roots();
        Ok(data_status(
            Provider::Pi,
            roots,
            !self.files()?.is_empty(),
            None,
        ))
    }

    fn discover_sources(&self) -> Result<Vec<SourceFile>> {
        Ok(self
            .files()?
            .into_iter()
            .map(|path| SourceFile {
                session_id: path
                    .file_stem()
                    .map(|value| value.to_string_lossy().to_string()),
                path,
                provider: Provider::Pi,
                format: SourceFormat::Jsonl,
                project_path: None,
                project_name: None,
            })
            .collect())
    }

    fn parse_line(&self, source: &SourceFile, line: &[u8]) -> Result<Option<ParsedUsage>> {
        parse_pi_value(source, &json_value(line)?)
    }

    fn ingest_line(
        &self,
        source: &SourceFile,
        line: &[u8],
        metadata: &mut SourceMetadata,
    ) -> Result<Option<ParsedUsage>> {
        let value = json_value(line)?;
        apply_pi_metadata(&value, metadata);
        parse_pi_value(source, &value)
    }
}

pub(crate) fn apply_pi_metadata(value: &Value, metadata: &mut SourceMetadata) {
    if value.get("type").and_then(Value::as_str) == Some("session") {
        if let Some(path) = project_path(value) {
            metadata.project_name = project_name(Some(&path));
            metadata.project_path = Some(path);
        }
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            metadata.session_id = Some(id.to_string());
        }
    }
    if let Some(model) = pi_turn_model(value) {
        metadata.model = Some(model);
    }
}

pub(crate) fn parse_pi_value(source: &SourceFile, value: &Value) -> Result<Option<ParsedUsage>> {
    let Some(record) = pi_billable_record(value) else {
        return Ok(None);
    };
    let Some(usage) = record.get("usage").filter(|value| value.is_object()) else {
        return Ok(None);
    };
    let Some(counts) = counts_from_usage(usage, true) else {
        return Ok(None);
    };
    let project_path = project_path(value);
    Ok(Some(ParsedUsage {
        counts,
        cumulative_snapshot: None,
        timestamp: timestamp(value),
        model: pi_record_model(record),
        session_id: session_id(value).or_else(|| source.session_id.clone()),
        project_name: project_name(project_path.as_deref()),
        project_path,
        source_event_id: source_event_id(value),
        reported_cost_usd: None,
    }))
}

fn pi_billable_record(value: &Value) -> Option<&Value> {
    if value.get("usage").is_some_and(Value::is_object) {
        return Some(value);
    }
    let message = value.get("message")?;
    if message.get("role").and_then(Value::as_str) == Some("assistant")
        && message.get("usage").is_some_and(Value::is_object)
    {
        return Some(message);
    }
    None
}

fn pi_record_model(record: &Value) -> Option<String> {
    record
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn pi_turn_model(value: &Value) -> Option<String> {
    if let Some(model) = value.get("model").and_then(Value::as_str) {
        return Some(model.to_string());
    }
    let message = value.get("message")?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    pi_record_model(message)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn parses_pi_usage() {
        let adapter = PiAdapter::with_home(PathBuf::from("/tmp"));
        let source = SourceFile::new(PathBuf::from("/tmp/pi.jsonl"), Provider::Pi);
        let line = include_bytes!("../../../../fixtures/pi/basic.jsonl");
        let parsed = adapter.parse_line(&source, line).unwrap().unwrap();
        assert_eq!(parsed.counts.total_tokens, 48);
        assert_eq!(parsed.session_id.as_deref(), Some("pi-session"));
    }

    #[test]
    fn parses_pi_cache_read_and_write_usage() {
        let adapter = PiAdapter::with_home(PathBuf::from("/tmp"));
        let source = SourceFile::new(PathBuf::from("/tmp/pi.jsonl"), Provider::Pi);
        let line = br#"{"type":"message_end","sessionId":"pi-cache","model":"gpt-5.6-sol","usage":{"input":100,"output":50,"cacheRead":300,"cacheWrite":20,"totalTokens":470}}"#;

        let parsed = adapter.parse_line(&source, line).unwrap().unwrap();

        assert_eq!(parsed.counts.input_tokens, 100);
        assert_eq!(parsed.counts.cached_input_tokens, 300);
        assert_eq!(parsed.counts.cache_creation_input_tokens, 20);
        assert_eq!(parsed.counts.output_tokens, 50);
        assert_eq!(parsed.counts.total_tokens, 470);
    }

    #[test]
    fn ignores_nested_tool_search_usage() {
        let adapter = PiAdapter::with_home(PathBuf::from("/tmp"));
        let source = SourceFile::new(PathBuf::from("/tmp/pi.jsonl"), Provider::Pi);
        let line = br#"{"type":"message","message":{"role":"toolResult","toolName":"web_search","details":{"response":{"model":"gpt-5.6-luna","requestId":"resp_search","usage":{"inputTokens":24826,"outputTokens":628,"totalTokens":29038}}}}}"#;
        assert!(adapter.parse_line(&source, line).unwrap().is_none());
    }

    #[test]
    fn parses_assistant_message_usage() {
        let adapter = PiAdapter::with_home(PathBuf::from("/tmp"));
        let source = SourceFile::new(PathBuf::from("/tmp/pi.jsonl"), Provider::Pi);
        let line = br#"{"type":"message","timestamp":"2026-08-25T08:50:19Z","message":{"role":"assistant","model":"grok-4.6","usage":{"input":10,"output":3,"cacheRead":4,"totalTokens":17}}}"#;
        let parsed = adapter.parse_line(&source, line).unwrap().unwrap();
        assert_eq!(parsed.model.as_deref(), Some("grok-4.6"));
        assert_eq!(parsed.counts.input_tokens, 10);
        assert_eq!(parsed.counts.output_tokens, 3);
        assert_eq!(parsed.counts.cached_input_tokens, 4);
        assert_eq!(parsed.counts.total_tokens, 17);
    }
}
