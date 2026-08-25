use std::path::PathBuf;

use anyhow::Result;
use llmeter_core::{Provider, ProviderDetection, SourceFile, SourceFormat, SourceMetadata};

use super::{
    ParsedUsage, ProviderAdapter, data_status, home_dir, json_value, jsonl_exists,
    pi::{apply_pi_metadata, parse_pi_value},
    walk_jsonl,
};

const OMP_PARSER_VERSION: u32 = 2;

#[derive(Clone, Debug)]
pub struct OmpAdapter {
    home: PathBuf,
}

impl Default for OmpAdapter {
    fn default() -> Self {
        Self { home: home_dir() }
    }
}

impl OmpAdapter {
    pub fn with_home(home: PathBuf) -> Self {
        Self { home }
    }

    fn root(&self) -> PathBuf {
        self.home.join(".omp").join("agent").join("sessions")
    }
}

impl ProviderAdapter for OmpAdapter {
    fn provider(&self) -> Provider {
        Provider::Omp
    }

    fn parser_version(&self) -> u32 {
        OMP_PARSER_VERSION
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![self.root()]
    }

    fn detect(&self) -> Result<ProviderDetection> {
        let root = self.root();
        Ok(data_status(
            Provider::Omp,
            vec![root.clone()],
            jsonl_exists(&root)?,
            None,
        ))
    }

    fn discover_sources(&self) -> Result<Vec<SourceFile>> {
        Ok(walk_jsonl(&self.root())?
            .into_iter()
            .map(|path| {
                let session_id = path
                    .file_stem()
                    .map(|value| value.to_string_lossy().to_string());
                SourceFile {
                    path,
                    provider: Provider::Omp,
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
        apply_pi_metadata(&json_value(line)?, metadata);
        Ok(())
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::providers::PiAdapter;

    #[test]
    fn omp_root_is_not_discovered_by_pi() {
        let home = std::env::temp_dir().join(format!("llmeter-omp-not-pi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let omp_root = home.join(".omp").join("agent").join("sessions");
        std::fs::create_dir_all(&omp_root).unwrap();
        std::fs::write(omp_root.join("session.jsonl"), "{}\n").unwrap();

        let pi = PiAdapter::with_home(home.clone());
        let omp = OmpAdapter::with_home(home.clone());
        assert!(pi.discover_sources().unwrap().is_empty());
        assert_eq!(omp.discover_sources().unwrap().len(), 1);
        assert_eq!(omp.discover_sources().unwrap()[0].provider, Provider::Omp);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn parses_omp_usage_as_omp_provider() {
        let adapter = OmpAdapter::with_home(PathBuf::from("/tmp"));
        let source = SourceFile::new(PathBuf::from("/tmp/omp.jsonl"), Provider::Omp);
        let line = br#"{"type":"message_end","sessionId":"01a037cb-3329-7000-baff-3ec556899770","model":"grok-4.6","usage":{"input":10,"output":3,"totalTokens":13}}"#;
        let parsed = adapter.parse_line(&source, line).unwrap().unwrap();
        assert_eq!(
            parsed.session_id.as_deref(),
            Some("01a037cb-3329-7000-baff-3ec556899770")
        );
        assert_eq!(parsed.model.as_deref(), Some("grok-4.6"));
    }

    #[test]
    fn ignores_web_search_tool_usage() {
        let adapter = OmpAdapter::with_home(PathBuf::from("/tmp"));
        let source = SourceFile::new(PathBuf::from("/tmp/omp.jsonl"), Provider::Omp);
        let line = br#"{"type":"message","message":{"role":"toolResult","details":{"response":{"model":"gpt-5.6-luna","usage":{"inputTokens":24826,"outputTokens":628,"totalTokens":29038}}}}}"#;
        assert!(adapter.parse_line(&source, line).unwrap().is_none());
    }
}
