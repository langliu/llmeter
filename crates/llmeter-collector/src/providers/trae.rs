use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use llmeter_core::{Provider, ProviderDetection, SourceFile};
use serde_json::Value;

use super::{ProviderAdapter, data_status, home_dir};

const SERVER_DATA_KEY: &str = "iCubeServerData://icube.cloudide";
const CN_AUTH_KEY: &str = "iCubeAuthInfo://icube.cloudide";

#[derive(Clone, Debug)]
pub struct TraeAdapter {
    root: PathBuf,
    cn_root: PathBuf,
}

impl Default for TraeAdapter {
    fn default() -> Self {
        let home = home_dir();
        Self {
            root: trae_root(&home, std::env::consts::OS),
            cn_root: trae_cn_root(&home, std::env::consts::OS),
        }
    }
}

pub(crate) fn trae_root(home: &Path, platform: &str) -> PathBuf {
    std::env::var_os("LLMETER_TRAE_HOME")
        .or_else(|| std::env::var_os("TRAE_HOME"))
        .or_else(|| std::env::var_os("TOKENTRACKER_TRAE_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| match platform {
            "macos" => home.join("Library/Application Support/TRAE SOLO"),
            "windows" => std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData/Roaming"))
                .join("TRAE SOLO"),
            _ => home.join(".trae-solo"),
        })
}

pub(crate) fn trae_cn_root(home: &Path, platform: &str) -> PathBuf {
    std::env::var_os("LLMETER_TRAE_CN_HOME")
        .or_else(|| std::env::var_os("TRAE_CN_HOME"))
        .or_else(|| std::env::var_os("TOKENTRACKER_TRAE_CN_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| match platform {
            "macos" => home.join("Library/Application Support/TRAE SOLO CN"),
            "windows" => std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData/Roaming"))
                .join("TRAE SOLO CN"),
            _ => home.join(".trae-solo-cn"),
        })
}

impl ProviderAdapter for TraeAdapter {
    fn provider(&self) -> Provider {
        Provider::Trae
    }

    fn detect(&self) -> Result<ProviderDetection> {
        let storage = self.root.join("User/globalStorage/storage.json");
        let cn_storage = self.cn_root.join("User/globalStorage/storage.json");
        let mut details = Vec::new();
        if let Some(detail) = read_entitlement(&storage).and_then(|value| {
            let identity = value.get("identityStr").and_then(Value::as_str)?;
            let fast = value
                .pointer("/detail/fastRequestPer")
                .and_then(Value::as_u64);
            Some(match fast {
                Some(fast) => format!("{identity} · {fast} fast requests/hour"),
                None => identity.to_string(),
            })
        }) {
            details.push(detail);
        }
        if cn_storage.is_file() {
            details.push(if has_trae_cn_auth(&cn_storage) {
                "TRAE SOLO CN · signed in".to_string()
            } else {
                "TRAE SOLO CN · installed".to_string()
            });
        }
        Ok(data_status(
            Provider::Trae,
            vec![self.root.clone(), storage, self.cn_root.clone(), cn_storage],
            false,
            (!details.is_empty())
                .then(|| details.join(" · "))
                .or_else(|| {
                    Some("TRAE exposes entitlement data, but no readable local token log.".into())
                }),
        ))
    }

    fn discover_sources(&self) -> Result<Vec<SourceFile>> {
        Ok(Vec::new())
    }

    fn parse_line(&self, _source: &SourceFile, _line: &[u8]) -> Result<Option<super::ParsedUsage>> {
        Ok(None)
    }
}

pub(crate) fn read_entitlement(storage: &std::path::Path) -> Option<Value> {
    let storage: Value = serde_json::from_slice(&fs::read(storage).ok()?).ok()?;
    let server_data = storage.get(SERVER_DATA_KEY)?;
    let server_data = match server_data {
        Value::String(value) => serde_json::from_str(value).ok()?,
        value => value.clone(),
    };
    server_data.get("entitlementInfo").cloned()
}

pub(crate) fn has_trae_cn_auth(storage: &Path) -> bool {
    let Ok(bytes) = fs::read(storage) else {
        return false;
    };
    let Ok(storage) = serde_json::from_slice::<Value>(&bytes) else {
        return false;
    };
    storage.get(CN_AUTH_KEY).is_some_and(|value| match value {
        Value::Null => false,
        Value::String(value) => !value.trim().is_empty(),
        _ => true,
    })
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn reads_string_encoded_trae_entitlement() {
        let directory = std::env::temp_dir().join(format!(
            "llmeter-trae-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("storage.json");
        let server = serde_json::json!({
            "entitlementInfo": {
                "identityStr": "Pro",
                "detail": { "fastRequestPer": 20 }
            }
        });
        let storage = serde_json::json!({ (SERVER_DATA_KEY): server.to_string() });
        fs::write(&path, serde_json::to_vec(&storage).unwrap()).unwrap();

        let entitlement = read_entitlement(&path).unwrap();

        assert_eq!(
            entitlement.get("identityStr"),
            Some(&Value::String("Pro".into()))
        );
        assert_eq!(
            entitlement
                .pointer("/detail/fastRequestPer")
                .and_then(Value::as_u64),
            Some(20)
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn detects_trae_solo_cn_login_state() {
        let directory = std::env::temp_dir().join(format!(
            "llmeter-trae-cn-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let international = directory.join("TRAE SOLO");
        let cn = directory.join("TRAE SOLO CN");
        let storage = cn.join("User/globalStorage/storage.json");
        fs::create_dir_all(storage.parent().unwrap()).unwrap();
        fs::write(
            &storage,
            serde_json::to_vec(&serde_json::json!({ (CN_AUTH_KEY): "encrypted-auth" })).unwrap(),
        )
        .unwrap();
        let adapter = TraeAdapter {
            root: international,
            cn_root: cn,
        };

        let detection = adapter.detect().unwrap();

        assert_eq!(detection.status, llmeter_core::ProviderStatus::Installed);
        assert!(
            detection
                .detail
                .unwrap()
                .contains("TRAE SOLO CN · signed in")
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
