use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, NaiveDate, Utc};
use llmeter_core::{Provider, ProviderDetection, SourceFile, SourceFormat, TokenCounts};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use super::{
    ParsedSnapshot, ParsedUsage, ProviderAdapter, SnapshotPolicy, data_status, home_dir,
    snapshot_scope,
};

const CURSOR_PARSER_VERSION: u32 = 2;
const CURSOR_USAGE_CSV_URL: &str =
    "https://cursor.com/api/dashboard/export-usage-events-csv?strategy=tokens";
const MAX_CSV_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct CursorAdapter {
    home: PathBuf,
    root: PathBuf,
    platform: String,
}

impl Default for CursorAdapter {
    fn default() -> Self {
        let home = home_dir();
        let platform = std::env::consts::OS.to_string();
        let root = cursor_root(&home, &platform);
        Self {
            home,
            root,
            platform,
        }
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

pub(crate) fn cursor_session_cookie(home: &Path, platform: &str) -> Option<String> {
    let state = cursor_root(home, platform).join("User/globalStorage/state.vscdb");
    let (subject, token) = cursor_identity(home, &state)?;
    Some(format!("WorkosCursorSessionToken={subject}%3A%3A{token}"))
}

pub(crate) fn cursor_account_scope(home: &Path, platform: &str) -> Option<String> {
    let state = cursor_root(home, platform).join("User/globalStorage/state.vscdb");
    let (subject, _) = cursor_identity(home, &state)?;
    Some(snapshot_scope(Provider::Cursor, &subject))
}

fn cursor_identity(home: &Path, state: &Path) -> Option<(String, String)> {
    let token = read_cursor_access_token(state)?;
    let subject = read_json_file(&home.join(".cursor/cli-config.json"))
        .and_then(|value| {
            value
                .pointer("/authInfo/authId")
                .and_then(Value::as_str)
                .and_then(normalize_cursor_subject)
        })
        .or_else(|| {
            jwt_payload(&token).and_then(|value| {
                value
                    .get("sub")
                    .and_then(Value::as_str)
                    .and_then(normalize_cursor_subject)
            })
        })?;
    Some((subject, token))
}

fn read_cursor_access_token(path: &Path) -> Option<String> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    connection
        .query_row(
            "SELECT value FROM ItemTable WHERE key = 'cursorAuth/accessToken'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .filter(|value| value.len() >= 10)
}

fn read_json_file(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let padding = (4 - payload.len() % 4) % 4;
    let mut encoded = payload.to_string();
    encoded.extend(std::iter::repeat_n('=', padding));
    serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(payload)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(encoded.as_bytes()))
            .ok()?,
    )
    .ok()
}

fn normalize_cursor_subject(subject: &str) -> Option<String> {
    let subject = subject.trim();
    if let Some((_, native)) = subject.rsplit_once('|')
        && native.starts_with("user_")
        && native
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return Some(native.to_string());
    }
    ["google-oauth2|", "github|", "oidc|", "auth0|"]
        .iter()
        .any(|prefix| subject.starts_with(prefix) && subject.len() > prefix.len())
        .then(|| subject.to_string())
}

impl ProviderAdapter for CursorAdapter {
    fn provider(&self) -> Provider {
        Provider::Cursor
    }

    fn parser_version(&self) -> u32 {
        CURSOR_PARSER_VERSION
    }

    fn detect(&self) -> Result<ProviderDetection> {
        let state = self.root.join("User/globalStorage/state.vscdb");
        let signed_in = cursor_session_cookie(&self.home, &self.platform).is_some();
        Ok(data_status(
            Provider::Cursor,
            vec![self.root.clone(), state],
            signed_in,
            Some(if signed_in {
                "Account token usage is available from Cursor's official usage export.".into()
            } else {
                "Sign in to Cursor to collect account token usage.".into()
            }),
        ))
    }

    fn discover_sources(&self) -> Result<Vec<SourceFile>> {
        let state = self.root.join("User/globalStorage/state.vscdb");
        if !state.is_file() || cursor_session_cookie(&self.home, &self.platform).is_none() {
            return Ok(Vec::new());
        }
        Ok(vec![SourceFile {
            path: state,
            provider: Provider::Cursor,
            format: SourceFormat::Snapshot,
            session_id: None,
            project_path: None,
            project_name: Some("Cursor account usage".into()),
        }])
    }

    fn parse_line(&self, _source: &SourceFile, _line: &[u8]) -> Result<Option<ParsedUsage>> {
        Ok(None)
    }

    fn parse_snapshot(&self, _source: &SourceFile) -> Result<ParsedSnapshot> {
        let cookie = cursor_session_cookie(&self.home, &self.platform)
            .ok_or_else(|| anyhow!("Cursor is not signed in"))?;
        let scope = cursor_account_scope(&self.home, &self.platform)
            .ok_or_else(|| anyhow!("Cursor account identity is unavailable"))?;
        let usages = parse_cursor_csv(&fetch_cursor_csv(&cookie)?)?;
        let policy = usages
            .iter()
            .map(|usage| usage.timestamp)
            .min()
            .map(SnapshotPolicy::ReplaceSince)
            // An empty export has no provable coverage window. Preserve the
            // existing history instead of treating a transient empty body as
            // an authoritative full-account deletion.
            .unwrap_or(SnapshotPolicy::Upsert);
        Ok(ParsedSnapshot {
            usages,
            policy,
            scope: Some(scope),
        })
    }

    fn uses_remote_snapshot(&self) -> bool {
        true
    }
}

fn fetch_cursor_csv(cookie: &str) -> Result<String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .redirects(0)
        .build();
    let mut url = CURSOR_USAGE_CSV_URL.to_string();
    for redirect in 0..=1 {
        let response = agent
            .get(&url)
            .set("Accept", "text/csv,*/*")
            .set("Cookie", cookie)
            .set("Referer", "https://www.cursor.com/settings")
            .set("User-Agent", "Mozilla/5.0 LLMeter/0.1")
            .call();
        match response {
            Ok(response) => {
                let mut body = String::new();
                response
                    .into_reader()
                    .take(MAX_CSV_BYTES + 1)
                    .read_to_string(&mut body)
                    .context("read Cursor usage export")?;
                if body.len() as u64 > MAX_CSV_BYTES {
                    bail!("Cursor usage export is too large");
                }
                return Ok(body);
            }
            Err(ureq::Error::Status(301 | 302 | 303 | 307 | 308, response)) if redirect == 0 => {
                let location = response
                    .header("Location")
                    .ok_or_else(|| anyhow!("Cursor usage export redirect has no location"))?;
                url = trusted_cursor_redirect(location)?;
            }
            Err(ureq::Error::Status(401 | 403, _)) => {
                bail!("Cursor login expired; sign in again in Cursor")
            }
            Err(ureq::Error::Status(status, _)) => {
                bail!("Cursor usage export returned HTTP {status}")
            }
            Err(ureq::Error::Transport(error)) => {
                bail!("Cursor usage export request failed: {error}")
            }
        }
    }
    bail!("Cursor usage export redirected too many times")
}

fn trusted_cursor_redirect(location: &str) -> Result<String> {
    if location.starts_with('/') {
        return Ok(format!("https://cursor.com{location}"));
    }
    if location.starts_with("https://cursor.com/")
        || location.starts_with("https://www.cursor.com/")
    {
        return Ok(location.to_string());
    }
    bail!("Cursor usage export redirected to an untrusted origin")
}

fn parse_cursor_csv(csv: &str) -> Result<Vec<ParsedUsage>> {
    let rows = parse_csv(csv)?;
    let Some(header) = rows.first() else {
        bail!("Cursor usage export is empty");
    };
    let columns = header
        .iter()
        .enumerate()
        .map(|(index, name)| (name.trim().to_string(), index))
        .collect::<HashMap<_, _>>();
    let required = [
        "Date",
        "Model",
        "Input (w/ Cache Write)",
        "Input (w/o Cache Write)",
        "Cache Read",
        "Output Tokens",
        "Total Tokens",
    ];
    if required.iter().any(|name| !columns.contains_key(*name)) {
        bail!("Cursor usage export has an unsupported schema");
    }

    let mut occurrences = HashMap::<String, usize>::new();
    let mut parsed = Vec::new();
    let mut valid = 0usize;
    let mut invalid = 0usize;
    for fields in rows.into_iter().skip(1) {
        match parse_cursor_data_row(&columns, &fields, &mut occurrences) {
            Ok(None) => valid += 1,
            Ok(Some(row)) => {
                valid += 1;
                parsed.push(row);
            }
            Err(error) => {
                invalid += 1;
                tracing::warn!(error = %error, "skipping malformed Cursor usage row");
            }
        }
    }
    if invalid > valid {
        bail!("Cursor usage export has too many invalid rows");
    }
    Ok(parsed)
}

fn parse_cursor_data_row(
    columns: &HashMap<String, usize>,
    fields: &[String],
    occurrences: &mut HashMap<String, usize>,
) -> Result<Option<ParsedUsage>> {
    let value = |name: &str| {
        columns
            .get(name)
            .and_then(|index| fields.get(*index))
            .map(String::as_str)
            .unwrap_or_default()
    };
    let timestamp = parse_cursor_date(value("Date"))?;
    let input_with_cache = csv_u64(value("Input (w/ Cache Write)"), "cache-write input")?;
    let input_tokens = csv_u64(value("Input (w/o Cache Write)"), "ordinary input")?;
    let cache_creation_input_tokens = input_with_cache.saturating_sub(input_tokens);
    let cached_input_tokens = csv_u64(value("Cache Read"), "cache-read")?;
    let output_tokens = csv_u64(value("Output Tokens"), "output")?;
    let _reported_total = csv_u64(value("Total Tokens"), "total")?;
    let total_tokens = input_tokens
        .saturating_add(cache_creation_input_tokens)
        .saturating_add(cached_input_tokens)
        .saturating_add(output_tokens);
    if total_tokens == 0 {
        return Ok(None);
    }
    let fingerprint = blake3::hash(fields.join("\u{1f}").as_bytes())
        .to_hex()
        .to_string();
    let occurrence = occurrences.entry(fingerprint.clone()).or_default();
    let source_event_id = format!("cursor:{fingerprint}:{}", *occurrence);
    *occurrence += 1;
    let session_id = [value("Cloud Agent ID"), value("Automation ID")]
        .into_iter()
        .find(|value| !value.trim().is_empty())
        .map(str::to_string);
    let model = value("Model").trim();
    Ok(Some(ParsedUsage {
        counts: TokenCounts {
            input_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
            output_tokens,
            reasoning_tokens: 0,
            total_tokens,
        },
        cumulative_snapshot: None,
        timestamp,
        model: (!model.is_empty()).then(|| model.to_string()),
        session_id,
        project_path: None,
        project_name: Some("Cursor account usage".into()),
        source_event_id: Some(source_event_id),
        reported_cost_usd: csv_f64(value("Cost")),
    }))
}

fn parse_csv(input: &str) -> Result<Vec<Vec<String>>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => row.push(std::mem::take(&mut field)),
            '\n' if !quoted => {
                row.push(std::mem::take(&mut field));
                if row.iter().any(|value| !value.trim().is_empty()) {
                    rows.push(std::mem::take(&mut row));
                } else {
                    row.clear();
                }
            }
            '\r' if !quoted => {}
            other => field.push(other),
        }
    }
    if quoted {
        bail!("Cursor usage export contains an unterminated quoted field");
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        if row.iter().any(|value| !value.trim().is_empty()) {
            rows.push(row);
        }
    }
    Ok(rows)
}

fn parse_cursor_date(value: &str) -> Result<DateTime<Utc>> {
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value.trim()) {
        return Ok(timestamp.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d") {
        return Ok(date.and_hms_opt(0, 0, 0).unwrap().and_utc());
    }
    bail!("Cursor usage export contains an invalid date")
}

fn csv_u64(value: &str, label: &str) -> Result<u64> {
    value
        .trim()
        .replace(',', "")
        .parse::<u64>()
        .map_err(|_| anyhow!("Cursor usage export contains an invalid {label} token count"))
}

fn csv_f64(value: &str) -> Option<f64> {
    let value = value.trim().replace(['$', ','], "");
    value
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cursor_exports_by_header_name() {
        let csv = "Date,Cloud Agent ID,Automation ID,Kind,Model,Max Mode,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens,Cost\n\
            \"2026-04-16T03:32:33.284Z\",\"agent-1\",\"\",\"On-Demand\",\"composer-2-fast\",\"No\",\"4000\",\"3189\",\"194368\",\"1815\",\"200183\",\"0.11\"\n";

        let rows = parse_cursor_csv(csv).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id.as_deref(), Some("agent-1"));
        assert_eq!(rows[0].model.as_deref(), Some("composer-2-fast"));
        assert_eq!(rows[0].counts.input_tokens, 3189);
        assert_eq!(rows[0].counts.cache_creation_input_tokens, 811);
        assert_eq!(rows[0].counts.cached_input_tokens, 194_368);
        assert_eq!(rows[0].counts.output_tokens, 1815);
        assert_eq!(rows[0].counts.total_tokens, 200_183);
        assert_eq!(rows[0].reported_cost_usd, Some(0.11));
    }

    #[test]
    fn preserves_duplicate_cursor_rows_with_stable_occurrences() {
        let csv = "Date,Model,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens,Cost\n\
            2025-02-01,gpt-4o,1000,500,200,300,2000,$0.10\n\
            2025-02-01,gpt-4o,1000,500,200,300,2000,$0.10\n";

        let rows = parse_cursor_csv(csv).unwrap();

        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].source_event_id, rows[1].source_event_id);
    }

    #[test]
    fn rejects_untrusted_cursor_redirects() {
        assert!(trusted_cursor_redirect("https://cursor.com.evil.test/usage").is_err());
        assert!(trusted_cursor_redirect("https://www.cursor.com/usage").is_ok());
    }

    #[test]
    fn rejects_blank_cursor_exports_but_accepts_header_only_exports() {
        assert!(parse_cursor_csv("").is_err());
        let header = "Date,Model,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens\n";
        assert!(parse_cursor_csv(header).unwrap().is_empty());
    }

    #[test]
    fn skips_malformed_cursor_rows_when_most_rows_are_valid() {
        let csv = "Date,Model,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens\n\
            2025-02-01,gpt-4o,1000,500,200,300,2000\n\
            not-a-date,gpt-4o,1000,500,200,300,2000\n\
            2025-02-02,gpt-4o,bad,500,200,300,2000\n\
            2025-02-03,gpt-4o,800,400,100,50,550\n";

        let rows = parse_cursor_csv(csv).unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].model.as_deref(), Some("gpt-4o"));
        assert_eq!(rows[0].counts.input_tokens, 500);
        assert_eq!(rows[1].counts.input_tokens, 400);
    }

    #[test]
    fn rejects_cursor_exports_when_most_data_rows_are_invalid() {
        let csv = "Date,Model,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens\n\
            not-a-date,gpt-4o,1000,500,200,300,2000\n\
            2025-02-01,gpt-4o,bad,500,200,300,2000\n\
            2025-02-02,gpt-4o,800,400,100,50,550\n";

        assert!(
            parse_cursor_csv(csv)
                .unwrap_err()
                .to_string()
                .contains("too many invalid rows")
        );
    }

    #[test]
    fn cursor_subject_supports_native_and_workos_logins() {
        assert_eq!(
            normalize_cursor_subject("auth0|user_abc"),
            Some("user_abc".into())
        );
        assert_eq!(
            normalize_cursor_subject("google-oauth2|123"),
            Some("google-oauth2|123".into())
        );
        assert_eq!(normalize_cursor_subject("invalid"), None);
    }
}
