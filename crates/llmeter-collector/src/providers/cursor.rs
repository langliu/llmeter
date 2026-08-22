use std::path::{Path, PathBuf};

use anyhow::Result;
use llmeter_core::{Provider, ProviderDetection, SourceFile};

use super::{ProviderAdapter, data_status, home_dir};

#[derive(Clone, Debug)]
pub struct CursorAdapter {
    root: PathBuf,
}

impl Default for CursorAdapter {
    fn default() -> Self {
        let home = home_dir();
        let root = cursor_root(&home, std::env::consts::OS);
        Self { root }
    }
}

pub(crate) fn cursor_root(home: &Path, platform: &str) -> PathBuf {
    match platform {
        "macos" => home.join("Library/Application Support/Cursor"),
        "windows" => std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Roaming"))
            .join("Cursor"),
        _ => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("Cursor"),
    }
}

impl ProviderAdapter for CursorAdapter {
    fn provider(&self) -> Provider {
        Provider::Cursor
    }

    fn detect(&self) -> Result<ProviderDetection> {
        let state = self.root.join("User/globalStorage/state.vscdb");
        Ok(data_status(
            Provider::Cursor,
            vec![self.root.clone(), state.clone()],
            false,
            Some("Account usage is available from Cursor's local login state.".into()),
        ))
    }

    fn discover_sources(&self) -> Result<Vec<SourceFile>> {
        Ok(Vec::new())
    }

    fn parse_line(&self, _source: &SourceFile, _line: &[u8]) -> Result<Option<super::ParsedUsage>> {
        Ok(None)
    }
}
