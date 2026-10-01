use std::{fs, path::PathBuf};

use anyhow::Result;
use llmeter_core::{Provider, ProviderDetection, ProviderStatus, SourceFile, SourceFormat};

use super::{
    ParsedSnapshot, ParsedUsage, ProviderAdapter,
    cline::{HISTORY_CWD_KEYS, history_item_usage, parse_history_item},
    data_status, editor_global_storage_dirs, walk_matching,
};

const ROO_PARSER_VERSION: u32 = 1;
const ROO_STORAGE_ID: &str = "rooveterinaryinc.roo-cline";

#[derive(Clone, Debug)]
pub struct RooAdapter {
    roots: Vec<PathBuf>,
}

impl Default for RooAdapter {
    fn default() -> Self {
        Self {
            roots: editor_global_storage_dirs()
                .into_iter()
                .map(|storage| storage.join(ROO_STORAGE_ID))
                .collect(),
        }
    }
}

impl RooAdapter {
    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    fn history_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for root in &self.roots {
            files.extend(walk_matching(&root.join("tasks"), |path, _| {
                path.file_name()
                    .is_some_and(|name| name == "history_item.json")
            })?);
        }
        Ok(files)
    }
}

impl ProviderAdapter for RooAdapter {
    fn provider(&self) -> Provider {
        Provider::Roo
    }

    fn parser_version(&self) -> u32 {
        ROO_PARSER_VERSION
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn detect(&self) -> Result<ProviderDetection> {
        let files = self.history_files()?;
        let mut detection = data_status(Provider::Roo, self.roots.clone(), !files.is_empty(), None);
        if detection.status == ProviderStatus::DataFound {
            detection.detail = Some(format!(
                "{} task histories across supported editors",
                files.len()
            ));
        }
        Ok(detection)
    }

    fn discover_sources(&self) -> Result<Vec<SourceFile>> {
        Ok(self
            .history_files()?
            .into_iter()
            .map(|path| SourceFile {
                // The task id is the enclosing directory name; the JSON id
                // inside is authoritative once parsed.
                session_id: path
                    .parent()
                    .and_then(std::path::Path::file_name)
                    .map(|value| value.to_string_lossy().to_string()),
                project_path: None,
                project_name: None,
                provider: Provider::Roo,
                format: SourceFormat::Snapshot,
                path,
            })
            .collect())
    }

    fn parse_line(&self, _source: &SourceFile, _line: &[u8]) -> Result<Option<ParsedUsage>> {
        Ok(None)
    }

    fn parse_snapshot(&self, source: &SourceFile) -> Result<ParsedSnapshot> {
        let bytes = fs::read(&source.path)?;
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            tracing::warn!(
                path = %source.path.display(),
                "skipping malformed Roo Code history item"
            );
            return Ok(ParsedSnapshot::default());
        };
        let Some(item) = parse_history_item(&value, HISTORY_CWD_KEYS) else {
            return Ok(ParsedSnapshot::default());
        };
        let session_id = if item.id.is_empty() {
            source.session_id.clone()
        } else {
            Some(item.id.clone())
        };
        Ok(ParsedSnapshot {
            usages: vec![history_item_usage(&item, session_id)],
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
    fn reads_per_task_history_items_from_editor_storage() {
        let home = std::env::temp_dir().join(format!("llmeter-roo-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let task = home
            .join("Code")
            .join("User")
            .join("globalStorage")
            .join(ROO_STORAGE_ID)
            .join("tasks")
            .join("task-9");
        fs::create_dir_all(&task).unwrap();
        fs::write(
            task.join("history_item.json"),
            r#"{"id":"task-9","ts":1784102040274,"task":"refactor","tokensIn":800,"tokensOut":90,"cacheWrites":100,"cacheReads":3000,"totalCost":0.004,"workspace":"/tmp/roo-project","mode":"code","apiConfigName":"default"}"#,
        )
        .unwrap();
        // A task directory without usage yet must not break discovery.
        fs::create_dir_all(task.parent().unwrap().join("task-10")).unwrap();

        let roo_storage = home
            .join("Code")
            .join("User")
            .join("globalStorage")
            .join(ROO_STORAGE_ID);
        let adapter = RooAdapter::with_roots(vec![roo_storage.clone()]);
        assert_eq!(adapter.detect().unwrap().status, ProviderStatus::DataFound);
        let sources = adapter.discover_sources().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].session_id.as_deref(), Some("task-9"));
        assert_eq!(
            sources[0].path,
            roo_storage
                .join("tasks")
                .join("task-9")
                .join("history_item.json")
        );

        let snapshot = adapter.parse_snapshot(&sources[0]).unwrap();
        let usage = &snapshot.usages[0];
        assert_eq!(usage.counts.input_tokens, 800);
        assert_eq!(usage.counts.cached_input_tokens, 3_000);
        assert_eq!(usage.counts.cache_creation_input_tokens, 100);
        assert_eq!(usage.counts.output_tokens, 90);
        assert_eq!(usage.counts.total_tokens, 3_990);
        assert_eq!(usage.session_id.as_deref(), Some("task-9"));
        assert_eq!(usage.reported_cost_usd, Some(0.004));
        assert_eq!(
            usage.project_path.as_deref(),
            Some(std::path::Path::new("/tmp/roo-project"))
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn absent_editor_storage_reports_not_installed() {
        let home = std::env::temp_dir().join(format!("llmeter-roo-absent-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let adapter = RooAdapter::with_roots(vec![home.join("Code")]);
        assert_eq!(
            adapter.detect().unwrap().status,
            ProviderStatus::NotInstalled
        );
        assert!(adapter.discover_sources().unwrap().is_empty());
    }
}
