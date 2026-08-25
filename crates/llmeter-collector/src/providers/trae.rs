use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use aes::Aes128;
use anyhow::{Result, anyhow, bail};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD},
};
use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use chrono::{Duration as ChronoDuration, Timelike, Utc};
use llmeter_core::{Provider, ProviderDetection, SourceFile, SourceFormat, TokenCounts};
use llmeter_storage::Database;
use serde_json::Value;
use sha2::{Digest, Sha512};

use super::{
    ParsedSnapshot, ParsedUsage, ProviderAdapter, SnapshotPolicy, data_status, home_dir,
    snapshot_scope,
};

pub const TRAE_CN_USAGE_SETTING: &str = "trae_cn_usage_enabled";

const TRAE_PARSER_VERSION: u32 = 2;
const SERVER_DATA_KEY: &str = "iCubeServerData://icube.cloudide";
const CN_AUTH_KEY: &str = "iCubeAuthInfo://icube.cloudide";
const TRAE_CN_USAGE_URL: &str =
    "https://api.trae.cn/trae/api/v1/pay/query_user_usage_group_by_session";
const TRAE_CN_PAGE_SIZE: usize = 20;
const TRAE_CN_MAX_PAGES: usize = 100;
const TRAE_CN_MAX_SPLIT_DEPTH: usize = 8;
const TRAE_CN_RESPONSE_LIMIT: u64 = 4 * 1024 * 1024;
const TRAE_CN_MAGIC: &[u8] = &[0x74, 0x63, 0x05, 0x10, 0x00, 0x00];
const TRAE_CN_JG: [u8; 64] = [
    82, 9, 106, 213, 48, 54, 165, 56, 191, 64, 163, 158, 129, 243, 215, 251, 124, 227, 57, 130,
    155, 47, 255, 135, 52, 142, 67, 68, 196, 222, 233, 203, 84, 123, 148, 50, 166, 194, 35, 61,
    238, 76, 149, 11, 66, 250, 195, 78, 8, 46, 161, 102, 40, 217, 36, 178, 118, 91, 162, 73, 109,
    139, 209, 37,
];
const TRAE_CN_KG: [u8; 64] = [
    31, 221, 168, 51, 136, 7, 199, 49, 177, 18, 16, 89, 39, 128, 236, 95, 96, 81, 127, 169, 25,
    181, 74, 13, 45, 229, 122, 159, 147, 201, 156, 239, 160, 224, 59, 77, 174, 42, 245, 176, 200,
    235, 187, 60, 131, 83, 153, 97, 23, 43, 4, 126, 186, 119, 214, 38, 225, 105, 20, 99, 85, 33,
    12, 125,
];

type Aes128CbcDecryptor = cbc::Decryptor<Aes128>;

#[derive(Clone)]
pub struct TraeAdapter {
    root: PathBuf,
    cn_root: PathBuf,
    database: Option<Database>,
}

impl Default for TraeAdapter {
    fn default() -> Self {
        let home = home_dir();
        Self {
            root: trae_root(&home, std::env::consts::OS),
            cn_root: trae_cn_root(&home, std::env::consts::OS),
            database: None,
        }
    }
}

impl TraeAdapter {
    pub fn with_database(database: Database) -> Self {
        Self {
            database: Some(database),
            ..Self::default()
        }
    }

    fn usage_enabled(&self) -> bool {
        self.database
            .as_ref()
            .and_then(|database| database.get_setting(TRAE_CN_USAGE_SETTING).ok().flatten())
            .is_some_and(|value| matches!(value.trim(), "1" | "true"))
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

    fn parser_version(&self) -> u32 {
        TRAE_PARSER_VERSION
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
        let cn_token = read_trae_cn_token(&cn_storage).ok().flatten();
        if cn_storage.is_file() {
            details.push(match (cn_token.is_some(), self.usage_enabled()) {
                (true, true) => "TRAE SOLO CN · token usage enabled".to_string(),
                (true, false) => "TRAE SOLO CN · signed in · token usage disabled".to_string(),
                (false, _) => "TRAE SOLO CN · installed".to_string(),
            });
        }
        Ok(data_status(
            Provider::Trae,
            vec![self.root.clone(), storage, self.cn_root.clone(), cn_storage],
            cn_token.is_some() && self.usage_enabled(),
            (!details.is_empty())
                .then(|| details.join(" · "))
                .or_else(|| Some("TRAE account data is not available locally.".into())),
        ))
    }

    fn discover_sources(&self) -> Result<Vec<SourceFile>> {
        if !self.usage_enabled() {
            return Ok(Vec::new());
        }
        let storage = self.cn_root.join("User/globalStorage/storage.json");
        if !storage.is_file() || read_trae_cn_token(&storage)?.is_none() {
            return Ok(Vec::new());
        }
        Ok(vec![SourceFile {
            path: storage,
            provider: Provider::Trae,
            format: SourceFormat::Snapshot,
            session_id: None,
            project_path: None,
            project_name: Some("TRAE CN account usage".into()),
        }])
    }

    fn parse_line(&self, _source: &SourceFile, _line: &[u8]) -> Result<Option<ParsedUsage>> {
        Ok(None)
    }

    fn parse_snapshot(&self, source: &SourceFile) -> Result<ParsedSnapshot> {
        let (token, scope) = read_trae_cn_identity(&source.path)?
            .ok_or_else(|| anyhow!("TRAE CN is not signed in"))?;
        let end = Utc::now();
        let start = (end - ChronoDuration::days(30))
            .with_minute(if end.minute() < 30 { 0 } else { 30 })
            .and_then(|value| value.with_second(0))
            .and_then(|value| value.with_nanosecond(0))
            .unwrap_or(end - ChronoDuration::days(30));
        let rows = fetch_trae_window(&token, start.timestamp(), end.timestamp(), 0)
            .map_err(|error| anyhow!(error.to_string()))?;
        let usages = normalize_trae_sessions(rows)?;
        Ok(ParsedSnapshot {
            policy: if usages.is_empty() {
                SnapshotPolicy::Upsert
            } else {
                SnapshotPolicy::ReplaceSince(start)
            },
            usages,
            scope: Some(scope),
        })
    }

    fn uses_remote_snapshot(&self) -> bool {
        true
    }
}

pub(crate) fn read_entitlement(storage: &Path) -> Option<Value> {
    let storage: Value = serde_json::from_slice(&fs::read(storage).ok()?).ok()?;
    let server_data = storage.get(SERVER_DATA_KEY)?;
    let server_data = match server_data {
        Value::String(value) => serde_json::from_str(value).ok()?,
        value => value.clone(),
    };
    server_data.get("entitlementInfo").cloned()
}

pub(crate) fn has_trae_cn_auth(storage: &Path) -> bool {
    read_trae_cn_token(storage).ok().flatten().is_some()
}

fn read_trae_cn_token(storage: &Path) -> Result<Option<String>> {
    Ok(read_trae_cn_identity(storage)?.map(|(token, _)| token))
}

fn read_trae_cn_identity(storage: &Path) -> Result<Option<(String, String)>> {
    let bytes = match fs::read(storage) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("TRAE CN storage could not be read"),
    };
    let storage: Value =
        serde_json::from_slice(&bytes).map_err(|_| anyhow!("TRAE CN storage is malformed"))?;
    let Some(value) = storage.get(CN_AUTH_KEY) else {
        return Ok(None);
    };
    let auth = parse_trae_cn_auth(value)?;
    let token = auth
        .get("token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("TRAE CN sign-in data has no token"))?;
    let identity = trae_cn_account_identity(&auth, token);
    Ok(Some((
        token.to_string(),
        snapshot_scope(Provider::Trae, &identity),
    )))
}

fn trae_cn_account_identity(auth: &Value, token: &str) -> String {
    const CLAIMS: [&str; 6] = ["sub", "user_id", "userId", "uid", "account_id", "accountId"];
    if let Some(value) = jwt_claim(token, &CLAIMS) {
        return value;
    }
    if let Value::Object(fields) = auth {
        for key in CLAIMS {
            if let Some(value) = fields
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                return value.to_string();
            }
        }
    }
    // A token without a readable subject is still scoped, but the raw token
    // never leaves memory or gets persisted; only this one-way identity hash
    // is used in the database.
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

fn jwt_claim(token: &str, names: &[&str]) -> Option<String> {
    token.split('.').skip(1).find_map(|segment| {
        let padding = (4 - segment.len() % 4) % 4;
        let mut padded = segment.to_string();
        padded.extend(std::iter::repeat_n('=', padding));
        let decoded = URL_SAFE_NO_PAD
            .decode(segment)
            .or_else(|_| URL_SAFE.decode(padded.as_bytes()))
            .ok()?;
        let payload: Value = serde_json::from_slice(&decoded).ok()?;
        names
            .iter()
            .find_map(|name| payload.get(*name).and_then(Value::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn parse_trae_cn_auth(value: &Value) -> Result<Value> {
    match value {
        Value::Object(_) => Ok(value.clone()),
        Value::String(value) if value.trim_start().starts_with('{') => {
            serde_json::from_str(value).map_err(|_| anyhow!("TRAE CN sign-in data is malformed"))
        }
        Value::String(value) => decrypt_trae_cn_auth(value),
        _ => bail!("TRAE CN sign-in data is malformed"),
    }
}

fn decrypt_trae_cn_auth(value: &str) -> Result<Value> {
    let blob = STANDARD
        .decode(value.trim())
        .map_err(|_| anyhow!("TRAE CN sign-in data is malformed"))?;
    let minimum = TRAE_CN_MAGIC.len() + 32 + 16;
    if blob.len() < minimum || !blob.starts_with(TRAE_CN_MAGIC) {
        bail!("TRAE CN sign-in data uses an unsupported format");
    }
    let salt_start = TRAE_CN_MAGIC.len();
    let salt_end = salt_start + 32;
    let ciphertext = &blob[salt_end..];
    if ciphertext.len() % 16 != 0 {
        bail!("TRAE CN sign-in data is malformed");
    }
    let salt_hash = Sha512::digest(&blob[salt_start..salt_end]);
    let secret = TRAE_CN_JG
        .iter()
        .zip(TRAE_CN_KG)
        .map(|(left, right)| left ^ right)
        .collect::<Vec<_>>();
    let mut kdf_input = Vec::with_capacity(128);
    kdf_input.extend_from_slice(&salt_hash);
    kdf_input.extend_from_slice(&secret);
    let key_iv = Sha512::digest(kdf_input);
    let plaintext = Aes128CbcDecryptor::new_from_slices(&key_iv[..16], &key_iv[16..32])
        .map_err(|_| anyhow!("TRAE CN sign-in data is malformed"))?
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .map_err(|_| anyhow!("TRAE CN sign-in data could not be decrypted"))?;
    if plaintext.len() < 64 {
        bail!("TRAE CN sign-in data is malformed");
    }
    let (expected, json) = plaintext.split_at(64);
    if expected != Sha512::digest(json).as_slice() {
        bail!("TRAE CN sign-in data failed its integrity check");
    }
    serde_json::from_slice(json).map_err(|_| anyhow!("TRAE CN sign-in data is malformed"))
}

#[derive(Debug, thiserror::Error)]
enum TraeUsageError {
    #[error("TRAE CN usage snapshot exceeds the supported capacity")]
    Capacity,
    #[error("TRAE CN usage snapshot is incomplete")]
    Incomplete,
    #[error("{0}")]
    Failed(String),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum TraePageDecision {
    Continue,
    Complete,
    Incomplete,
    Capacity,
}

fn trae_page_decision(
    page: usize,
    page_row_count: usize,
    collected: u64,
    total: Option<u64>,
) -> TraePageDecision {
    if page == 1
        && total.is_some_and(|total| total > (TRAE_CN_PAGE_SIZE * TRAE_CN_MAX_PAGES) as u64)
    {
        return TraePageDecision::Capacity;
    }
    if page_row_count == 0 {
        if total.is_some_and(|total| collected < total) {
            return TraePageDecision::Incomplete;
        }
        return TraePageDecision::Complete;
    }
    let collected = collected.saturating_add(page_row_count as u64);
    if total.is_some_and(|total| collected >= total) {
        return TraePageDecision::Complete;
    }
    if page == TRAE_CN_MAX_PAGES {
        return TraePageDecision::Capacity;
    }
    TraePageDecision::Continue
}

fn fetch_trae_window(
    token: &str,
    start: i64,
    end: i64,
    depth: usize,
) -> std::result::Result<Vec<Value>, TraeUsageError> {
    match fetch_trae_pages(token, start, end) {
        Ok(rows) => Ok(rows),
        Err(TraeUsageError::Capacity) if depth < TRAE_CN_MAX_SPLIT_DEPTH && start < end => {
            let middle = start + (end - start) / 2;
            let mut left = fetch_trae_window(token, start, middle, depth + 1)?;
            left.extend(fetch_trae_window(token, middle + 1, end, depth + 1)?);
            Ok(left)
        }
        Err(error) => Err(error),
    }
}

fn fetch_trae_pages(
    token: &str,
    start: i64,
    end: i64,
) -> std::result::Result<Vec<Value>, TraeUsageError> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .redirects(0)
        .build();
    let mut rows = Vec::new();
    for page in 1..=TRAE_CN_MAX_PAGES {
        let response = agent
            .post(TRAE_CN_USAGE_URL)
            .set("Content-Type", "application/json")
            .set("Authorization", &format!("Cloud-IDE-JWT {token}"))
            .send_json(serde_json::json!({
                "usage_type": [7],
                "start_time": start,
                "end_time": end,
                "page_num": page,
                "page_size": TRAE_CN_PAGE_SIZE,
            }));
        let response = match response {
            Ok(response) => response,
            Err(ureq::Error::Status(401 | 403, _)) => {
                return Err(TraeUsageError::Failed(
                    "TRAE CN login expired; sign in again in TRAE".into(),
                ));
            }
            Err(ureq::Error::Status(status, _)) => {
                return Err(TraeUsageError::Failed(format!(
                    "TRAE CN usage API returned HTTP {status}"
                )));
            }
            Err(ureq::Error::Transport(_)) => {
                return Err(TraeUsageError::Failed(
                    "TRAE CN usage API request failed".into(),
                ));
            }
        };
        let mut body = Vec::new();
        response
            .into_reader()
            .take(TRAE_CN_RESPONSE_LIMIT + 1)
            .read_to_end(&mut body)
            .map_err(|_| {
                TraeUsageError::Failed("TRAE CN usage response could not be read".into())
            })?;
        if body.len() as u64 > TRAE_CN_RESPONSE_LIMIT {
            return Err(TraeUsageError::Failed(
                "TRAE CN usage response is too large".into(),
            ));
        }
        let body: Value = serde_json::from_slice(&body).map_err(|_| {
            TraeUsageError::Failed("TRAE CN usage API returned invalid JSON".into())
        })?;
        let api_code = body
            .pointer("/data/code")
            .or_else(|| body.get("code"))
            .and_then(Value::as_i64);
        if api_code.is_some_and(|code| code != 0) {
            return Err(TraeUsageError::Failed(format!(
                "TRAE CN usage API returned error code {}",
                api_code.unwrap()
            )));
        }
        let data = body
            .get("data")
            .filter(|value| value.is_object())
            .unwrap_or(&body);
        let page_rows = data
            .get("user_usage_group_by_sessions")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                TraeUsageError::Failed("TRAE CN usage response has an unsupported schema".into())
            })?;
        let total = match data.get("total") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_u64().ok_or_else(|| {
                TraeUsageError::Failed("TRAE CN usage response has an invalid total".into())
            })?),
        };
        match trae_page_decision(page, page_rows.len(), rows.len() as u64, total) {
            TraePageDecision::Incomplete => return Err(TraeUsageError::Incomplete),
            TraePageDecision::Capacity => return Err(TraeUsageError::Capacity),
            TraePageDecision::Complete => {
                rows.extend(page_rows.iter().cloned());
                return Ok(rows);
            }
            TraePageDecision::Continue => rows.extend(page_rows.iter().cloned()),
        }
    }
    Err(TraeUsageError::Capacity)
}

fn normalize_trae_sessions(rows: Vec<Value>) -> Result<Vec<ParsedUsage>> {
    let mut sessions = HashMap::<String, ParsedUsage>::new();
    for row in rows {
        let session_id = row
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("TRAE CN usage row has no session ID"))?
            .to_string();
        let usage_time = row
            .get("usage_time")
            .and_then(Value::as_i64)
            .filter(|value| *value > 0)
            .ok_or_else(|| anyhow!("TRAE CN usage row has an invalid timestamp"))?;
        let timestamp = chrono::DateTime::from_timestamp(usage_time, 0)
            .ok_or_else(|| anyhow!("TRAE CN usage row has an invalid timestamp"))?;
        let decoded_extra = row
            .get("extra_info")
            .and_then(Value::as_str)
            .and_then(|value| serde_json::from_str::<Value>(value).ok());
        let extra = row
            .get("extra_info")
            .filter(|value| value.is_object())
            .or(decoded_extra.as_ref().filter(|value| value.is_object()));
        let token = |name: &str, required: bool| -> Result<u64> {
            let value = extra
                .and_then(|extra| extra.get(name))
                .or_else(|| row.get(name));
            match value {
                Some(value) => value
                    .as_u64()
                    .ok_or_else(|| anyhow!("TRAE CN usage row has an invalid {name}")),
                None if !required => Ok(0),
                None => Err(anyhow!("TRAE CN usage row has no {name}")),
            }
        };
        let raw_input = token("input_token", true)?;
        let output_tokens = token("output_token", true)?;
        let cached_input_tokens = token("cache_read_token", false)?.min(raw_input);
        let cache_creation_input_tokens =
            token("cache_write_token", false)?.min(raw_input.saturating_sub(cached_input_tokens));
        let input_tokens = raw_input
            .saturating_sub(cached_input_tokens)
            .saturating_sub(cache_creation_input_tokens);
        let model = row
            .get("model_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("trae-cn-unknown")
            .to_string();
        let parsed = ParsedUsage {
            counts: TokenCounts {
                input_tokens,
                cached_input_tokens,
                cache_creation_input_tokens,
                output_tokens,
                reasoning_tokens: 0,
                total_tokens: raw_input.saturating_add(output_tokens),
            },
            cumulative_snapshot: None,
            timestamp,
            model: Some(model),
            session_id: Some(session_id.clone()),
            project_path: None,
            project_name: Some("TRAE CN account usage".into()),
            source_event_id: Some(format!("trae-cn:{session_id}")),
            reported_cost_usd: None,
        };
        if let Some(previous) = sessions.get(&session_id)
            && (previous.counts != parsed.counts
                || previous.timestamp != parsed.timestamp
                || previous.model != parsed.model)
        {
            bail!("TRAE CN usage contains conflicting session rows");
        }
        sessions.insert(session_id, parsed);
    }
    let mut sessions = sessions.into_values().collect::<Vec<_>>();
    sessions.sort_by_key(|usage| usage.timestamp);
    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    const AUTH_FIXTURE: &str = "dGMFEAAA3q2+796tvu/erb7v3q2+796tvu/erb7v3q2+796tvu/43q0BL3Fo+gI3mEsCEdP2c66sO1HLQB7bKIlHUL7xJg0PVJHLSzn/gDVQmH7Ksh0wflKSFHtHgz7rwynP4aOOx3CPgDR5lQ8neR7CWouH94Dh99HlsWwqo29etbVBVbTU18KIS74UPyA5ASAWnYHqKPQSfv8uCn6GOMfOyJPNPMI6Yz666PBNVFTastLi70ThZ6Lb/Gs+bjyTDrQnSfjDnDJPinjJejWWlboznbnME/rNkmc2LLMftj8jGqorIq4=";

    fn temporary_directory(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "llmeter-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn decrypts_trae_cn_auth_fixture() {
        let auth = decrypt_trae_cn_auth(AUTH_FIXTURE).unwrap();
        assert_eq!(
            auth.get("token").and_then(Value::as_str),
            Some("fake.jwt.eyJzdWIiOiJ0ZXN0IiwiZXhwIjo5OTk5OTk5OTk5fQ.sig")
        );
        assert_eq!(
            trae_cn_account_identity(&auth, auth.get("token").and_then(Value::as_str).unwrap()),
            "test"
        );
    }

    #[test]
    fn normalizes_trae_cn_sessions_and_cache_subsets() {
        let rows = vec![serde_json::json!({
            "session_id": "session-1",
            "model_name": "doubao-pro",
            "usage_time": 1_700_000_000,
            "input_token": 500,
            "output_token": 7,
            "cache_read_token": 200,
            "cache_write_token": 50
        })];

        let parsed = normalize_trae_sessions(rows).unwrap();

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].counts.input_tokens, 250);
        assert_eq!(parsed[0].counts.cached_input_tokens, 200);
        assert_eq!(parsed[0].counts.cache_creation_input_tokens, 50);
        assert_eq!(parsed[0].counts.output_tokens, 7);
        assert_eq!(parsed[0].counts.total_tokens, 507);
        assert_eq!(parsed[0].session_id.as_deref(), Some("session-1"));
    }

    #[test]
    fn reads_string_encoded_trae_entitlement() {
        let directory = temporary_directory("trae");
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
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn detects_trae_solo_cn_login_state_without_enabling_network_usage() {
        let directory = temporary_directory("trae-cn");
        let international = directory.join("TRAE SOLO");
        let cn = directory.join("TRAE SOLO CN");
        let storage = cn.join("User/globalStorage/storage.json");
        fs::create_dir_all(storage.parent().unwrap()).unwrap();
        fs::write(
            &storage,
            serde_json::to_vec(&serde_json::json!({
                (CN_AUTH_KEY): { "token": "synthetic.jwt.value" }
            }))
            .unwrap(),
        )
        .unwrap();
        let adapter = TraeAdapter {
            root: international,
            cn_root: cn,
            database: None,
        };

        let detection = adapter.detect().unwrap();

        assert_eq!(detection.status, llmeter_core::ProviderStatus::Installed);
        assert!(detection.detail.unwrap().contains("token usage disabled"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn empty_page_is_complete_only_when_no_further_rows_are_expected() {
        assert_eq!(
            trae_page_decision(1, 0, 0, None),
            TraePageDecision::Complete
        );
        assert_eq!(
            trae_page_decision(1, 0, 0, Some(0)),
            TraePageDecision::Complete
        );
        assert_eq!(
            trae_page_decision(2, 0, 20, None),
            TraePageDecision::Complete
        );
        assert_eq!(
            trae_page_decision(2, 0, 20, Some(20)),
            TraePageDecision::Complete
        );
    }

    #[test]
    fn truncated_trae_page_is_incomplete_when_total_says_more_remain() {
        assert_eq!(
            trae_page_decision(1, 0, 0, Some(20)),
            TraePageDecision::Incomplete
        );
        assert_eq!(
            trae_page_decision(2, 0, 20, Some(40)),
            TraePageDecision::Incomplete
        );
    }

    #[test]
    fn filled_trae_page_continues_until_total_or_capacity() {
        assert_eq!(
            trae_page_decision(1, 20, 0, Some(40)),
            TraePageDecision::Continue
        );
        assert_eq!(
            trae_page_decision(2, 20, 20, Some(40)),
            TraePageDecision::Complete
        );
        assert_eq!(
            trae_page_decision(TRAE_CN_MAX_PAGES, 20, 0, None),
            TraePageDecision::Capacity
        );
        assert_eq!(
            trae_page_decision(
                1,
                0,
                0,
                Some((TRAE_CN_PAGE_SIZE * TRAE_CN_MAX_PAGES + 1) as u64)
            ),
            TraePageDecision::Capacity
        );
    }
}
