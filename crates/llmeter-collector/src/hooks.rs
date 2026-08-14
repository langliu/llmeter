use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::Utc;
use llmeter_core::Provider;
use serde_json::{Value, json};

use crate::providers::home_dir;

const MANAGED_BEGIN: &str = "# LLMeter managed hook: begin";
const MANAGED_END: &str = "# LLMeter managed hook: end";
const HOOK_MARKER: &str = "--llmeter-hook";

pub fn data_dir() -> PathBuf {
    if let Ok(value) = std::env::var("LLMETER_DATA_DIR") {
        return PathBuf::from(value);
    }
    dirs::data_dir()
        .unwrap_or_else(|| home_dir().join("Library").join("Application Support"))
        .join("LLMeter")
}

pub fn signal_path() -> PathBuf {
    data_dir().join("state").join("sync.signal")
}

pub fn emit_signal(provider: Provider) -> Result<()> {
    let signal = signal_path();
    if let Some(parent) = signal.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(signal)?;
    writeln!(file, "{} {}", Utc::now().timestamp_millis(), provider)?;
    Ok(())
}

pub fn codex_config_path() -> PathBuf {
    home_dir().join(".codex").join("config.toml")
}

pub fn claude_settings_path() -> PathBuf {
    home_dir().join(".claude").join("settings.json")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookStatus {
    pub installed: bool,
    pub conflict: bool,
    pub detail: String,
}

pub fn codex_hook_status() -> Result<HookStatus> {
    let path = codex_config_path();
    if !path.exists() {
        return Ok(HookStatus {
            installed: false,
            conflict: false,
            detail: "config.toml not found".into(),
        });
    }
    let text = fs::read_to_string(&path)?;
    Ok(HookStatus {
        installed: text.contains(MANAGED_BEGIN),
        conflict: text
            .lines()
            .any(|line| line.trim_start().starts_with("notify"))
            && !text.contains(MANAGED_BEGIN),
        detail: "Codex notify is preserved when an existing notify entry is present".into(),
    })
}

pub fn claude_hook_status() -> Result<HookStatus> {
    let path = claude_settings_path();
    if !path.exists() {
        return Ok(HookStatus {
            installed: false,
            conflict: false,
            detail: "settings.json not found".into(),
        });
    }
    let value: Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let command = managed_command(Provider::Claude);
    Ok(HookStatus {
        installed: value.to_string().contains(&command),
        conflict: false,
        detail: "SessionEnd hook is appended without replacing other hooks".into(),
    })
}

pub fn install_codex_hook(executable: &Path) -> Result<HookStatus> {
    let path = codex_config_path();
    let current = if path.exists() {
        fs::read_to_string(&path)?
    } else {
        String::new()
    };
    let command = shell_quote(executable);
    if current.contains(MANAGED_BEGIN) {
        return codex_hook_status();
    }
    if current
        .lines()
        .any(|line| line.trim_start().starts_with("notify"))
    {
        return Ok(HookStatus {
            installed: false,
            conflict: true,
            detail: "existing Codex notify found; no configuration was changed".into(),
        });
    }
    backup(&path)?;
    let mut next = current;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(MANAGED_BEGIN);
    next.push('\n');
    next.push_str(&format!(
        "notify = [\"{}\", \"notify\", \"--provider\", \"codex\"]\n",
        command.replace('\\', "\\\\").replace('"', "\\\"")
    ));
    next.push_str(MANAGED_END);
    next.push('\n');
    fs::create_dir_all(path.parent().context("Codex config has no parent")?)?;
    fs::write(&path, next)?;
    codex_hook_status()
}

pub fn install_claude_hook(executable: &Path) -> Result<HookStatus> {
    let path = claude_settings_path();
    let mut value: Value = if path.exists() {
        serde_json::from_str(&fs::read_to_string(&path)?)?
    } else {
        json!({})
    };
    let command = managed_command(Provider::Claude);
    if value.to_string().contains(&command) {
        return claude_hook_status();
    }
    backup(&path)?;
    let hooks = value
        .as_object_mut()
        .context("Claude settings root must be a JSON object")?
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let session_end = hooks
        .as_object_mut()
        .context("Claude hooks must be a JSON object")?
        .entry("SessionEnd")
        .or_insert_with(|| json!([]));
    let entries = session_end
        .as_array_mut()
        .context("Claude SessionEnd hooks must be an array")?;
    entries.push(json!({
        "hooks": [{
            "type": "command",
            "command": format!(
                "{} notify --provider claude {}",
                shell_quote(executable),
                HOOK_MARKER
            )
        }]
    }));
    fs::create_dir_all(path.parent().context("Claude settings has no parent")?)?;
    fs::write(&path, serde_json::to_vec_pretty(&value)?)?;
    claude_hook_status()
}

pub fn uninstall_codex_hook() -> Result<HookStatus> {
    let path = codex_config_path();
    if !path.exists() {
        return codex_hook_status();
    }
    let current = fs::read_to_string(&path)?;
    if !current.contains(MANAGED_BEGIN) {
        return codex_hook_status();
    }
    backup(&path)?;
    let next = remove_managed_block(&current);
    fs::write(path, next)?;
    codex_hook_status()
}

pub fn uninstall_claude_hook() -> Result<HookStatus> {
    let path = claude_settings_path();
    if !path.exists() {
        return claude_hook_status();
    }
    let mut value: Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
    let command = managed_command(Provider::Claude);
    let Some(session_end) = value
        .get_mut("hooks")
        .and_then(|value| value.get_mut("SessionEnd"))
        .and_then(Value::as_array_mut)
    else {
        return claude_hook_status();
    };
    let before = session_end.len();
    session_end.retain(|entry| !entry.to_string().contains(&command));
    if session_end.len() != before {
        backup(&path)?;
        fs::write(&path, serde_json::to_vec_pretty(&value)?)?;
    }
    claude_hook_status()
}

fn managed_command(provider: Provider) -> String {
    format!("--provider {} {}", provider, HOOK_MARKER)
}

fn shell_quote(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "'\\''")
}

fn backup(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let directory = data_dir().join("hooks").join("backups");
    fs::create_dir_all(&directory)?;
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "config".into());
    let backup = directory.join(format!("{name}.{}.bak", Utc::now().timestamp_millis()));
    fs::copy(path, backup)?;
    Ok(())
}

fn remove_managed_block(value: &str) -> String {
    let mut result = Vec::new();
    let mut in_block = false;
    for line in value.lines() {
        if line.trim() == MANAGED_BEGIN {
            in_block = true;
            continue;
        }
        if line.trim() == MANAGED_END {
            in_block = false;
            continue;
        }
        if !in_block {
            result.push(line);
        }
    }
    let mut output = result.join("\n");
    if value.ends_with('\n') {
        output.push('\n');
    }
    output
}
