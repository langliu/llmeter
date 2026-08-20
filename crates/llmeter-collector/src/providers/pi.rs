use std::path::PathBuf;

use anyhow::Result;
use llmeter_core::{Provider, ProviderDetection, SourceFile, SourceFormat, SourceMetadata};
use serde_json::Value;

use super::{
    ParsedUsage, ProviderAdapter, counts_from_usage, data_status, deduplicate_paths, home_dir,
    json_value, model, object_for_key, object_with_usage, project_name, project_path, session_id,
    source_event_id, timestamp, walk_jsonl,
};

const PI_PARSER_VERSION: u32 = 3;

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
        vec![
            self.home.join(".pi").join("agent").join("sessions"),
            self.home.join(".omp").join("agent").join("sessions"),
        ]
    }

    fn files(&self) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for root in self.roots() {
            files.extend(walk_jsonl(&root)?);
        }
        Ok(deduplicate_paths(files))
    }
}

impl ProviderAdapter for PiAdapter {
    fn provider(&self) -> Provider {
        Provider::Pi
    }

    fn parser_version(&self) -> u32 {
        PI_PARSER_VERSION
    }

    fn update_source_metadata(
        &self,
        _source: &SourceFile,
        line: &[u8],
        metadata: &mut SourceMetadata,
    ) -> Result<()> {
        let value = json_value(line)?;
        if value.get("type").and_then(Value::as_str) == Some("session") {
            if let Some(path) = project_path(&value) {
                metadata.project_name = project_name(Some(&path));
                metadata.project_path = Some(path);
            }
            if let Some(id) = value.get("id").and_then(Value::as_str) {
                metadata.session_id = Some(id.to_string());
            }
        }
        if let Some(model) = model(&value) {
            metadata.model = Some(model);
        }
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
        let value = json_value(line)?;
        let usage = object_for_key(&value, "usage").or_else(|| object_with_usage(&value));
        let Some(usage) = usage else {
            return Ok(None);
        };
        let Some(counts) = counts_from_usage(usage, true) else {
            return Ok(None);
        };
        let project_path = project_path(&value);
        Ok(Some(ParsedUsage {
            counts,
            cumulative_snapshot: None,
            timestamp: timestamp(&value),
            model: model(&value),
            session_id: session_id(&value).or_else(|| source.session_id.clone()),
            project_name: project_name(project_path.as_deref()),
            project_path,
            source_event_id: source_event_id(&value),
            reported_cost_usd: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn parses_pi_usage_and_keeps_provider_unified() {
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
}
