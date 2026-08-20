use std::{fmt, path::PathBuf, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Codex,
    Claude,
    OpenCode,
    Pi,
    Zed,
    Grok,
}

impl Provider {
    pub const ALL: [Self; 6] = [
        Self::Codex,
        Self::Claude,
        Self::OpenCode,
        Self::Pi,
        Self::Zed,
        Self::Grok,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::OpenCode => "opencode",
            Self::Pi => "pi",
            Self::Zed => "zed",
            Self::Grok => "grok",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
            Self::OpenCode => "OpenCode",
            Self::Pi => "Pi",
            Self::Zed => "Zed",
            Self::Grok => "Grok",
        }
    }

    pub fn resume_command(self, session_ref: &str) -> Option<String> {
        match self {
            Self::Codex => Some(format!("codex resume {session_ref}")),
            Self::Claude => Some(format!("claude --resume {session_ref}")),
            Self::OpenCode => Some(format!("opencode --session {session_ref}")),
            Self::Pi => Some(format!("pi --session {session_ref}")),
            Self::Grok => Some(format!("grok --resume {session_ref}")),
            Self::Zed => None,
        }
    }

    pub fn resume_ref(self, session_id: Option<&str>, source_file: Option<&str>) -> Option<String> {
        let stem = source_file.and_then(|path| {
            std::path::Path::new(path)
                .file_stem()
                .map(|value| value.to_string_lossy().into_owned())
        });
        for candidate in [session_id.map(str::to_string), stem].into_iter().flatten() {
            if candidate.is_empty() {
                continue;
            }
            if let Some(uuid) = trailing_uuid(&candidate) {
                return Some(uuid);
            }
            return Some(candidate);
        }
        None
    }
}

fn trailing_uuid(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    const UUID_LEN: usize = 36;
    if bytes.len() < UUID_LEN {
        return None;
    }
    let start = bytes.len() - UUID_LEN;
    let candidate = &value[start..];
    let mut chars = candidate.chars();
    for index in 0..UUID_LEN {
        let next = chars.next()?;
        let expected_hyphen = matches!(index, 8 | 13 | 18 | 23);
        if expected_hyphen {
            if next != '-' {
                return None;
            }
        } else if !next.is_ascii_hexdigit() {
            return None;
        }
    }
    if start > 0 {
        let prefix = value.as_bytes()[start - 1];
        if prefix.is_ascii_alphanumeric() {
            return None;
        }
    }
    Some(candidate.to_ascii_lowercase())
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Provider {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "codex" => Ok(Self::Codex),
            "claude" | "claude_code" | "claude-code" => Ok(Self::Claude),
            "opencode" | "open_code" | "open-code" => Ok(Self::OpenCode),
            "pi" | "pi-mono" => Ok(Self::Pi),
            "zed" => Ok(Self::Zed),
            "grok" | "grok-build" | "grok_build" => Ok(Self::Grok),
            other => Err(format!("unsupported provider: {other}")),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceFormat {
    Jsonl,
    Sqlite,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceMetadata {
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub project_path: Option<PathBuf>,
    pub project_name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceFile {
    pub path: PathBuf,
    pub provider: Provider,
    pub format: SourceFormat,
    pub session_id: Option<String>,
    pub project_path: Option<PathBuf>,
    pub project_name: Option<String>,
}

impl SourceFile {
    pub fn new(path: PathBuf, provider: Provider) -> Self {
        Self {
            path,
            provider,
            format: SourceFormat::Jsonl,
            session_id: None,
            project_path: None,
            project_name: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProviderStatus {
    NotInstalled,
    Installed,
    DataFound,
    UnsupportedVersion,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderDetection {
    pub provider: Provider,
    pub status: ProviderStatus,
    pub roots: Vec<PathBuf>,
    pub detail: Option<String>,
}

impl ProviderDetection {
    pub fn not_installed(provider: Provider) -> Self {
        Self {
            provider,
            status: ProviderStatus::NotInstalled,
            roots: Vec::new(),
            detail: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SyncResult {
    pub files_scanned: usize,
    pub events_seen: usize,
    pub events_inserted: usize,
    pub tokens_added: u64,
    pub warnings: Vec<String>,
    pub duration_ms: u128,
}

impl SyncResult {
    pub fn merge(&mut self, other: Self) {
        self.files_scanned += other.files_scanned;
        self.events_seen += other.events_seen;
        self.events_inserted += other.events_inserted;
        self.tokens_added = self.tokens_added.saturating_add(other.tokens_added);
        self.warnings.extend(other.warnings);
        self.duration_ms = self.duration_ms.saturating_add(other.duration_ms);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsageEvent {
    pub id: String,
    pub provider: Provider,
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub project_path: Option<PathBuf>,
    pub project_name: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub estimated_cost_usd: Option<f64>,
    pub source_file: Option<PathBuf>,
    pub source_event_id: Option<String>,
}

impl UsageEvent {
    pub fn token_counts(&self) -> crate::TokenCounts {
        crate::TokenCounts {
            input_tokens: self.input_tokens,
            cached_input_tokens: self.cached_input_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
            output_tokens: self.output_tokens,
            reasoning_tokens: self.reasoning_tokens,
            total_tokens: self.total_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_ref_prefers_trailing_uuid() {
        assert_eq!(
            Provider::Codex
                .resume_ref(
                    Some("rollout-2026-08-14T15-01-51-019fff13-bd73-7c71-a844-4dbe59993141"),
                    None,
                )
                .as_deref(),
            Some("019fff13-bd73-7c71-a844-4dbe59993141"),
        );
        assert_eq!(
            Provider::Pi
                .resume_ref(
                    Some("2026-08-14T08-29-17-175Z_019fff63-c7f7-7bc7-be2d-788e8136ab63"),
                    None,
                )
                .as_deref(),
            Some("019fff63-c7f7-7bc7-be2d-788e8136ab63"),
        );
        assert_eq!(
            Provider::OpenCode
                .resume_command("ses_01108a3edffeJgAoTLxwgsAYkm")
                .as_deref(),
            Some("opencode --session ses_01108a3edffeJgAoTLxwgsAYkm")
        );
        assert_eq!(Provider::Zed.resume_command("thread-id"), None);
    }

    #[test]
    fn serialized_usage_event_has_no_content_fields() {
        let event = UsageEvent {
            id: "event".into(),
            provider: Provider::Codex,
            model: Some("gpt-5.4".into()),
            session_id: Some("session".into()),
            project_path: Some(PathBuf::from("/tmp/project")),
            project_name: Some("project".into()),
            timestamp: Utc::now(),
            input_tokens: 10,
            cached_input_tokens: 2,
            cache_creation_input_tokens: 0,
            output_tokens: 3,
            reasoning_tokens: 1,
            total_tokens: 14,
            estimated_cost_usd: None,
            source_file: Some(PathBuf::from("/tmp/session.jsonl")),
            source_event_id: None,
        };
        let serialized = serde_json::to_string(&event).unwrap();
        for forbidden in [
            "prompt",
            "response",
            "reasoning_content",
            "source_code",
            "body",
        ] {
            assert!(
                !serialized.contains(forbidden),
                "found forbidden field: {forbidden}"
            );
        }
    }
}
