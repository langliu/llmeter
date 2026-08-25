use std::path::PathBuf;

use anyhow::Result;
use llmeter_core::{Provider, ProviderDetection, SourceFile, SourceFormat};

use super::{
    PARSER_VERSION, ParsedUsage, ProviderAdapter, counts_from_usage, data_status, home_dir,
    json_value, jsonl_exists, model, object_for_key, project_name, project_path, session_id,
    source_event_id, timestamp, walk_jsonl,
};

#[derive(Clone, Debug)]
pub struct ClaudeAdapter {
    home: PathBuf,
}

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self { home: home_dir() }
    }
}

impl ClaudeAdapter {
    pub fn with_home(home: PathBuf) -> Self {
        Self { home }
    }

    fn projects_root(&self) -> PathBuf {
        self.home.join(".claude").join("projects")
    }
}

impl ProviderAdapter for ClaudeAdapter {
    fn provider(&self) -> Provider {
        Provider::Claude
    }

    fn parser_version(&self) -> u32 {
        PARSER_VERSION
    }
    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![self.projects_root()]
    }

    fn detect(&self) -> Result<ProviderDetection> {
        let root = self.home.join(".claude");
        let projects = self.projects_root();
        Ok(data_status(
            Provider::Claude,
            vec![root, projects.clone()],
            jsonl_exists(&projects)?,
            None,
        ))
    }

    fn discover_sources(&self) -> Result<Vec<SourceFile>> {
        let root = self.projects_root();
        Ok(walk_jsonl(&root)?
            .into_iter()
            .map(|path| {
                let project_name = path
                    .parent()
                    .and_then(|value| value.file_name())
                    .map(|value| value.to_string_lossy().to_string());
                SourceFile {
                    session_id: path
                        .file_stem()
                        .map(|value| value.to_string_lossy().to_string()),
                    path,
                    provider: Provider::Claude,
                    format: SourceFormat::Jsonl,
                    project_path: None,
                    project_name,
                }
            })
            .collect())
    }

    fn parse_line(&self, source: &SourceFile, line: &[u8]) -> Result<Option<ParsedUsage>> {
        let value = json_value(line)?;
        let usage = value
            .get("message")
            .and_then(|message| message.get("usage"))
            .or_else(|| value.get("usage"))
            .or_else(|| object_for_key(&value, "usage"));
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
            project_path: project_path.clone(),
            project_name: project_name(project_path.as_deref())
                .or_else(|| source.project_name.clone()),
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
    fn parses_claude_cache_fields() {
        let adapter = ClaudeAdapter::with_home(PathBuf::from("/tmp"));
        let source = SourceFile::new(PathBuf::from("/tmp/session.jsonl"), Provider::Claude);
        let line = br#"{"type":"assistant","sessionId":"s1","timestamp":"2026-08-14T00:00:00Z","cwd":"/tmp/demo","message":{"model":"claude-sonnet-4","usage":{"input_tokens":10,"cache_read_input_tokens":5,"cache_creation_input_tokens":2,"output_tokens":3}}}"#;
        let parsed = adapter.parse_line(&source, line).unwrap().unwrap();
        assert_eq!(parsed.counts.input_tokens, 10);
        assert_eq!(parsed.counts.cached_input_tokens, 5);
        assert_eq!(parsed.counts.cache_creation_input_tokens, 2);
        assert_eq!(parsed.counts.total_tokens, 20);
    }
}
