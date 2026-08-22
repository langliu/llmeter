use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

use base64::{
    Engine,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use chrono::{DateTime, Datelike, TimeZone, Utc};
use llmeter_core::{LimitSource, LimitWindow, LimitsSnapshot, Provider, ProviderLimits};
use llmeter_storage::{Database, LimitRepository};
use serde_json::Value;

use crate::providers::{
    cursor_root, cursor_session_cookie, has_trae_cn_auth, qoder_root, read_entitlement,
    trae_cn_root, trae_root,
};

const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const GROK_BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing";
const CURSOR_USAGE_URL: &str = "https://cursor.com/api/usage-summary";
const QODER_USAGE_URL: &str = "https://qoder.com/api/v2/me/usages/big_model_credits";
const QODER_CN_USAGE_URL: &str = "https://qoder.com.cn/api/v2/me/usages/big_model_credits";
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct LimitCollector {
    repository: LimitRepository,
    http: Arc<dyn LimitHttpClient>,
    home: PathBuf,
    platform: String,
}

impl LimitCollector {
    pub fn new(database: Database) -> Self {
        Self {
            repository: LimitRepository::new(database),
            http: Arc::new(UreqLimitHttpClient::new()),
            home: dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
            platform: std::env::consts::OS.to_string(),
        }
    }

    pub fn cached_snapshot(&self) -> LimitsSnapshot {
        self.repository.load_snapshot().unwrap_or_default()
    }

    pub fn refresh(&self) -> LimitsSnapshot {
        let now = Utc::now();
        let (claude, codex, grok, cursor, qoder, trae) = std::thread::scope(|scope| {
            let claude =
                scope.spawn(|| fetch_claude(&self.home, &self.platform, self.http.as_ref()));
            let codex = scope.spawn(|| fetch_codex(&self.home, self.http.as_ref()));
            let grok = scope.spawn(|| fetch_grok(&self.home, self.http.as_ref()));
            let cursor =
                scope.spawn(|| fetch_cursor(&self.home, &self.platform, self.http.as_ref()));
            let qoder = scope.spawn(|| fetch_qoder(&self.home, &self.platform, self.http.as_ref()));
            let trae = scope.spawn(|| fetch_trae(&self.home, &self.platform));
            (
                claude.join().unwrap_or_else(|_| {
                    ProviderFetch::failed("Claude limit worker stopped unexpectedly")
                }),
                codex.join().unwrap_or_else(|_| {
                    ProviderFetch::failed("Codex limit worker stopped unexpectedly")
                }),
                grok.join().unwrap_or_else(|_| {
                    ProviderFetch::failed("Grok limit worker stopped unexpectedly")
                }),
                cursor.join().unwrap_or_else(|_| {
                    ProviderFetch::failed("Cursor limit worker stopped unexpectedly")
                }),
                qoder.join().unwrap_or_else(|_| {
                    ProviderFetch::failed("Qoder limit worker stopped unexpectedly")
                }),
                trae.join().unwrap_or_else(|_| {
                    ProviderFetch::failed("TRAE limit worker stopped unexpectedly")
                }),
            )
        });

        let providers = [
            (Provider::Claude, claude),
            (Provider::Codex, codex),
            (Provider::Grok, grok),
            (Provider::Cursor, cursor),
            (Provider::Qoder, qoder),
            (Provider::Trae, trae),
        ]
        .into_iter()
        .map(|(provider, result)| self.finish(provider, result, now))
        .collect();

        LimitsSnapshot {
            fetched_at: Some(now),
            providers,
        }
    }

    fn finish(
        &self,
        provider: Provider,
        result: ProviderFetch,
        now: DateTime<Utc>,
    ) -> ProviderLimits {
        match result {
            ProviderFetch::NotConfigured => ProviderLimits::not_configured(provider, now),
            ProviderFetch::Ready(mut limits) => {
                if provider == Provider::Qoder
                    && limits.last_error.is_some()
                    && let Ok(Some(cached)) = self.repository.load(provider)
                {
                    limits = merge_partial_qoder_cache(limits, cached, now);
                } else if provider == Provider::Qoder {
                    limits.last_error = None;
                }
                let _ = self.repository.save(&limits);
                limits
            }
            ProviderFetch::Failed(error) => {
                if error.transient
                    && let Ok(Some(cached)) = self.repository.load(provider)
                    && let Some(stale) = cached.as_stale(error.message.clone(), now)
                {
                    return stale;
                }
                ProviderLimits::failed(provider, now, error.message)
            }
        }
    }
}

enum ProviderFetch {
    NotConfigured,
    Ready(ProviderLimits),
    Failed(LimitFetchError),
}

impl ProviderFetch {
    fn failed(message: impl Into<String>) -> Self {
        Self::Failed(LimitFetchError::transient(message))
    }
}

#[derive(Debug)]
struct LimitFetchError {
    message: String,
    transient: bool,
}

impl LimitFetchError {
    fn auth(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            transient: false,
        }
    }

    fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            transient: true,
        }
    }
}

trait LimitHttpClient: Send + Sync {
    fn get_json(&self, url: &str, headers: &[(&str, &str)]) -> Result<Value, HttpFailure>;
}

#[derive(Debug)]
struct HttpFailure {
    status: Option<u16>,
    message: String,
}

struct UreqLimitHttpClient {
    agent: ureq::Agent,
}

impl UreqLimitHttpClient {
    fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout(PROVIDER_TIMEOUT)
                .redirects(0)
                .build(),
        }
    }
}

impl LimitHttpClient for UreqLimitHttpClient {
    fn get_json(&self, url: &str, headers: &[(&str, &str)]) -> Result<Value, HttpFailure> {
        let mut request = self.agent.get(url);
        for (name, value) in headers {
            request = request.set(name, value);
        }
        match request.call() {
            Ok(response) => response.into_json().map_err(|error| HttpFailure {
                status: None,
                message: format!("invalid JSON response: {error}"),
            }),
            Err(ureq::Error::Status(status, _)) => Err(HttpFailure {
                status: Some(status),
                message: format!("HTTP {status}"),
            }),
            Err(ureq::Error::Transport(error)) => Err(HttpFailure {
                status: None,
                message: error.to_string(),
            }),
        }
    }
}

fn fetch_claude(home: &Path, platform: &str, http: &dyn LimitHttpClient) -> ProviderFetch {
    let Some(credentials) = read_claude_credentials(home, platform) else {
        return ProviderFetch::NotConfigured;
    };
    let Some(token) = credentials
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return ProviderFetch::Failed(LimitFetchError::auth(
            "Claude login expired. Run `claude` to sign in again.",
        ));
    };
    let authorization = format!("Bearer {token}");
    let body = match http.get_json(
        CLAUDE_USAGE_URL,
        &[
            ("Authorization", authorization.as_str()),
            ("anthropic-beta", "oauth-2025-04-20"),
            ("Accept", "application/json"),
        ],
    ) {
        Ok(body) => body,
        Err(error) if matches!(error.status, Some(401 | 403)) => {
            return ProviderFetch::Failed(LimitFetchError::auth(
                "Claude login expired. Run `claude` to sign in again.",
            ));
        }
        Err(error) => {
            return ProviderFetch::Failed(LimitFetchError::transient(format!(
                "Claude usage request failed: {}",
                error.message
            )));
        }
    };

    let plan = credentials
        .pointer("/claudeAiOauth/subscriptionType")
        .and_then(Value::as_str)
        .map(title_case);
    match normalize_claude(&body, plan, Utc::now()) {
        Ok(limits) => ProviderFetch::Ready(limits),
        Err(error) => ProviderFetch::Failed(error),
    }
}

fn read_claude_credentials(home: &Path, platform: &str) -> Option<Value> {
    if platform == "macos" {
        let output = Command::new("/usr/bin/security")
            .args([
                "find-generic-password",
                "-s",
                "Claude Code-credentials",
                "-w",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        return serde_json::from_slice(&output.stdout).ok();
    }
    read_json_file(&home.join(".claude/.credentials.json"))
}

fn normalize_claude(
    body: &Value,
    plan: Option<String>,
    now: DateTime<Utc>,
) -> Result<ProviderLimits, LimitFetchError> {
    let mut windows = Vec::new();
    push_claude_window(&mut windows, "five_hour", body.get("five_hour"));
    push_claude_window(&mut windows, "seven_day", body.get("seven_day"));
    push_claude_window(&mut windows, "opus", body.get("seven_day_opus"));
    let has_legacy_opus = body
        .get("seven_day_opus")
        .is_some_and(|value| !value.is_null());
    if let Some(scoped) = body.get("limits").and_then(Value::as_array) {
        for entry in scoped {
            if entry.get("kind").and_then(Value::as_str) != Some("weekly_scoped") {
                continue;
            }
            let label = entry
                .pointer("/scope/model/display_name")
                .or_else(|| entry.pointer("/scope/model/id"))
                .and_then(Value::as_str);
            let Some(label) = label else { continue };
            if has_legacy_opus && label.eq_ignore_ascii_case("opus") {
                continue;
            }
            let Some(percent) = number(entry.get("percent")) else {
                continue;
            };
            let mut window = LimitWindow::new(format!("model:{label}"), percent);
            window.reset_at = parse_reset(entry.get("resets_at"));
            window.window_seconds = Some(7 * 24 * 60 * 60);
            windows.push(window);
        }
    }
    if windows.is_empty() {
        return Err(LimitFetchError::transient(
            "Claude usage response contained no quota windows",
        ));
    }
    Ok(ready_limits(Provider::Claude, plan, windows, now))
}

fn push_claude_window(windows: &mut Vec<LimitWindow>, key: &str, value: Option<&Value>) {
    let Some(value) = value else { return };
    let Some(percent) = number(value.get("utilization")) else {
        return;
    };
    let mut window = LimitWindow::new(key, percent);
    window.reset_at = parse_reset(value.get("resets_at"));
    window.window_seconds = match key {
        "five_hour" => Some(5 * 60 * 60),
        "seven_day" | "opus" => Some(7 * 24 * 60 * 60),
        _ => None,
    };
    windows.push(window);
}

fn fetch_codex(home: &Path, http: &dyn LimitHttpClient) -> ProviderFetch {
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    let Some(auth) = read_json_file(&codex_home.join("auth.json")) else {
        return ProviderFetch::NotConfigured;
    };
    let Some(token) = auth
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return ProviderFetch::Failed(LimitFetchError::auth(
            "Codex login expired. Run `codex login` to sign in again.",
        ));
    };
    let access_payload = jwt_payload(token);
    let id_payload = auth
        .pointer("/tokens/id_token")
        .and_then(Value::as_str)
        .and_then(jwt_payload);
    let account_id = auth
        .pointer("/tokens/account_id")
        .and_then(Value::as_str)
        .or_else(|| {
            access_payload
                .as_ref()
                .and_then(|value| {
                    value.pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
                })
                .and_then(Value::as_str)
        })
        .or_else(|| {
            id_payload
                .as_ref()
                .and_then(|value| {
                    value.pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
                })
                .and_then(Value::as_str)
        });
    let plan = access_payload
        .as_ref()
        .and_then(|value| value.pointer("/https:~1~1api.openai.com~1auth/chatgpt_plan_type"))
        .and_then(Value::as_str)
        .or_else(|| {
            id_payload
                .as_ref()
                .and_then(|value| {
                    value.pointer("/https:~1~1api.openai.com~1auth/chatgpt_plan_type")
                })
                .and_then(Value::as_str)
        })
        .map(title_case);
    let authorization = format!("Bearer {token}");
    let mut headers = vec![
        ("Authorization", authorization.as_str()),
        ("Accept", "application/json"),
    ];
    if let Some(account_id) = account_id {
        headers.push(("ChatGPT-Account-Id", account_id));
    }
    let body = match http.get_json(CODEX_USAGE_URL, &headers) {
        Ok(body) => body,
        Err(error) if matches!(error.status, Some(401 | 403)) => {
            return ProviderFetch::Failed(LimitFetchError::auth(
                "Codex login expired. Run `codex login` to sign in again.",
            ));
        }
        Err(error) => {
            return ProviderFetch::Failed(LimitFetchError::transient(format!(
                "Codex usage request failed: {}",
                error.message
            )));
        }
    };
    match normalize_codex(&body, plan, Utc::now()) {
        Ok(limits) => ProviderFetch::Ready(limits),
        Err(error) => ProviderFetch::Failed(error),
    }
}

fn normalize_codex(
    body: &Value,
    plan: Option<String>,
    now: DateTime<Utc>,
) -> Result<ProviderLimits, LimitFetchError> {
    let mut windows = normalize_codex_rate_limit(body.get("rate_limit"), "", true);
    if let Some(limit) = body.pointer("/spend_control/individual_limit") {
        let limit_amount = number(limit.get("limit"));
        let used_amount = number(limit.get("used"));
        let used_percent = used_amount
            .zip(limit_amount)
            .filter(|(_, total)| *total > 0.0)
            .map(|(used, total)| used / total * 100.0)
            .or_else(|| number(limit.get("used_percent")));
        if let Some(percent) = used_percent {
            let mut window = LimitWindow::new("credits", percent);
            window.reset_at = parse_reset(limit.get("reset_at"));
            window.used_amount = used_amount;
            window.limit_amount = limit_amount;
            window.unit = Some("credits".into());
            windows.push(window);
        }
    }
    if let Some(additional) = body.get("additional_rate_limits").and_then(Value::as_array) {
        for entry in additional {
            let name = format!(
                "{} {}",
                entry
                    .get("limit_name")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                entry
                    .get("metered_feature")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            );
            if name.to_ascii_lowercase().contains("spark") {
                windows.extend(normalize_codex_rate_limit(
                    entry.get("rate_limit"),
                    "spark_",
                    true,
                ));
            }
        }
    }
    if windows.is_empty() {
        return Err(LimitFetchError::transient(
            "Codex usage response contained no quota windows",
        ));
    }
    Ok(ready_limits(Provider::Codex, plan, windows, now))
}

fn normalize_codex_rate_limit(
    rate_limit: Option<&Value>,
    prefix: &str,
    positional_fallback: bool,
) -> Vec<LimitWindow> {
    let Some(rate_limit) = rate_limit else {
        return Vec::new();
    };
    let values = [
        rate_limit.get("primary_window"),
        rate_limit.get("secondary_window"),
    ];
    let mut classified = Vec::new();
    let mut unknown = Vec::new();
    for (index, value) in values.into_iter().enumerate() {
        let Some(value) = value else { continue };
        let Some(percent) = number(value.get("used_percent")) else {
            continue;
        };
        let seconds = number(value.get("limit_window_seconds")).map(|value| value as u64);
        let key = match seconds {
            Some(18_000) => Some(format!("{prefix}five_hour")),
            Some(604_800) => Some(format!("{prefix}seven_day")),
            _ => None,
        };
        let mut window = LimitWindow::new(key.clone().unwrap_or_default(), percent.round());
        window.reset_at = parse_reset(value.get("reset_at"));
        window.window_seconds = seconds;
        if key.is_some() {
            classified.push(window);
        } else {
            unknown.push((index, window));
        }
    }
    if positional_fallback {
        let mut has_session = classified
            .iter()
            .any(|window| window.key == format!("{prefix}five_hour"));
        let mut has_weekly = classified
            .iter()
            .any(|window| window.key == format!("{prefix}seven_day"));
        for (index, mut window) in unknown {
            let inferred = if !has_session && (index == 0 || has_weekly) {
                has_session = true;
                Some("five_hour")
            } else if !has_weekly {
                has_weekly = true;
                Some("seven_day")
            } else {
                None
            };
            if let Some(inferred) = inferred {
                window.key = format!("{prefix}{inferred}");
                classified.push(window);
            }
        }
    }
    classified.sort_by_key(|window| window.key.contains("seven_day"));
    classified
}

fn fetch_cursor(home: &Path, platform: &str, http: &dyn LimitHttpClient) -> ProviderFetch {
    let root = cursor_root(home, platform);
    if !root.exists() {
        return ProviderFetch::NotConfigured;
    }
    let Some(cookie) = cursor_session_cookie(home, platform) else {
        return ProviderFetch::NotConfigured;
    };
    let body = match http.get_json(
        CURSOR_USAGE_URL,
        &[
            ("Cookie", cookie.as_str()),
            ("Accept", "application/json"),
            ("Referer", "https://www.cursor.com/settings"),
            ("User-Agent", "Mozilla/5.0 LLMeter/0.1"),
        ],
    ) {
        Ok(body) => body,
        Err(error) if matches!(error.status, Some(401 | 403)) => {
            return ProviderFetch::Failed(LimitFetchError::auth(
                "Cursor login expired. Sign in again in Cursor.",
            ));
        }
        Err(error) => {
            return ProviderFetch::Failed(LimitFetchError::transient(format!(
                "Cursor usage request failed: {}",
                error.message
            )));
        }
    };
    match normalize_cursor(&body, Utc::now()) {
        Ok(limits) => ProviderFetch::Ready(limits),
        Err(error) => ProviderFetch::Failed(error),
    }
}

fn normalize_cursor(body: &Value, now: DateTime<Utc>) -> Result<ProviderLimits, LimitFetchError> {
    let plan = body.pointer("/individualUsage/plan");
    let individual = body.pointer("/individualUsage/onDemand");
    let team = body.pointer("/teamUsage/onDemand");
    let reset = parse_reset(body.get("billingCycleEnd"));
    let cycle_seconds = body
        .get("billingCycleStart")
        .and_then(|start| parse_reset(Some(start)))
        .zip(reset)
        .and_then(|(start, end)| u64::try_from((end - start).num_seconds()).ok());
    let auto = plan.and_then(|value| number(value.get("autoPercentUsed")));
    let api = plan.and_then(|value| number(value.get("apiPercentUsed")));
    let amount_percent = |value: Option<&Value>| {
        value.and_then(|value| {
            number(value.get("used"))
                .zip(number(value.get("limit")))
                .filter(|(_, limit)| *limit > 0.0)
                .map(|(used, limit)| used / limit * 100.0)
        })
    };
    let mut headline = plan.and_then(|value| number(value.get("totalPercentUsed")));
    if headline.is_none() {
        headline = auto
            .zip(api)
            .map(|(auto, api)| (auto + api) / 2.0)
            .or(api)
            .or(auto)
            .or_else(|| amount_percent(plan))
            .or_else(|| amount_percent(individual))
            .or_else(|| amount_percent(team));
    }
    if headline == Some(0.0) {
        headline = [amount_percent(individual), amount_percent(team)]
            .into_iter()
            .flatten()
            .find(|value| *value > 0.0)
            .or(headline);
    }
    let mut windows = Vec::new();
    for (key, percent) in [
        ("cursor_plan", headline),
        ("cursor_auto", auto),
        ("cursor_api", api),
    ] {
        if let Some(percent) = percent {
            let mut window = LimitWindow::new(key, percent);
            window.reset_at = reset;
            window.window_seconds = cycle_seconds;
            windows.push(window);
        }
    }
    if windows.is_empty() {
        return Err(LimitFetchError::transient(
            "Cursor usage response contained no quota windows",
        ));
    }
    let plan = body
        .get("membershipType")
        .and_then(Value::as_str)
        .map(title_case);
    Ok(ready_limits(Provider::Cursor, plan, windows, now))
}

fn fetch_qoder(home: &Path, platform: &str, http: &dyn LimitHttpClient) -> ProviderFetch {
    let mut ready = Vec::new();
    let mut errors = Vec::new();
    for edition in qoder_editions(home, platform) {
        match fetch_qoder_edition(&edition, http) {
            ProviderFetch::Ready(limits) => ready.push(limits),
            ProviderFetch::Failed(error) => errors.push(error),
            ProviderFetch::NotConfigured => {}
        }
    }
    if !ready.is_empty() {
        let mut merged = merge_qoder_limits(ready, Utc::now());
        merged.last_error = errors.first().map(|error| error.message.clone());
        return ProviderFetch::Ready(merged);
    }
    errors
        .into_iter()
        .next()
        .map(ProviderFetch::Failed)
        .unwrap_or(ProviderFetch::NotConfigured)
}

fn fetch_qoder_edition(edition: &QoderEdition, http: &dyn LimitHttpClient) -> ProviderFetch {
    let installed = edition.root.exists();
    let mut last_error = None;
    match qoder_rpc(&edition.root, "credit/usage") {
        Ok(Some(body)) => {
            match normalize_qoder_rpc(&body, edition.label, edition.window_key, Utc::now()) {
                Ok(limits) => return ProviderFetch::Ready(limits),
                Err(error) => last_error = Some(error),
            }
        }
        Ok(None) => {}
        Err(error) => last_error = Some(LimitFetchError::transient(error)),
    }
    let cookie = std::env::var(&edition.cookie_env)
        .ok()
        .filter(|value| !value.trim().is_empty());
    if let Some(cookie) = cookie {
        let body = match http.get_json(
            edition.usage_url,
            &[
                ("Cookie", cookie.as_str()),
                ("Accept", "application/json, text/plain, */*"),
                ("Accept-Language", "en-US,en;q=0.9"),
                ("Origin", edition.origin),
                ("Referer", edition.referer),
                ("X-Requested-With", "XMLHttpRequest"),
                ("Bx-V", "2.5.35"),
                ("User-Agent", "Mozilla/5.0 LLMeter/0.1"),
            ],
        ) {
            Ok(body) => body,
            Err(error) if matches!(error.status, Some(401 | 403)) => {
                return ProviderFetch::Failed(LimitFetchError::auth(format!(
                    "{} login expired. Sign in again in {}.",
                    edition.label, edition.label
                )));
            }
            Err(error) => {
                return ProviderFetch::Failed(LimitFetchError::transient(format!(
                    "{} usage request failed: {}",
                    edition.label, error.message
                )));
            }
        };
        match normalize_qoder_api(&body, edition.label, edition.window_key, Utc::now()) {
            Ok(limits) => return ProviderFetch::Ready(limits),
            Err(error) => last_error = Some(error),
        }
    }
    if let Some(error) = last_error {
        ProviderFetch::Failed(error)
    } else if installed {
        ProviderFetch::Failed(LimitFetchError::transient(format!(
            "{} is installed, but its local service is not running. Open {} and refresh.",
            edition.label, edition.label
        )))
    } else {
        ProviderFetch::NotConfigured
    }
}

struct QoderEdition {
    root: PathBuf,
    label: &'static str,
    cookie_env: String,
    usage_url: &'static str,
    origin: &'static str,
    referer: &'static str,
    window_key: &'static str,
}

fn qoder_editions(home: &Path, platform: &str) -> [QoderEdition; 2] {
    [
        qoder_edition(home, platform, false),
        qoder_edition(home, platform, true),
    ]
}

fn qoder_edition(home: &Path, platform: &str, china: bool) -> QoderEdition {
    let (prefix, label, usage_url, origin, referer, window_key) = if china {
        (
            "QODER_CN",
            "Qoder CN",
            QODER_CN_USAGE_URL,
            "https://qoder.com.cn",
            "https://qoder.com.cn/account/usage",
            "qoder_cn_credits",
        )
    } else {
        (
            "QODER",
            "Qoder",
            QODER_USAGE_URL,
            "https://qoder.com",
            "https://qoder.com/account/usage",
            "qoder_credits",
        )
    };
    QoderEdition {
        root: qoder_root(home, platform, china),
        label,
        cookie_env: format!("{prefix}_COOKIE"),
        usage_url,
        origin,
        referer,
        window_key,
    }
}

#[cfg(unix)]
fn qoder_rpc(root: &Path, method: &str) -> Result<Option<Value>, String> {
    use std::os::unix::net::UnixStream;

    let Some(socket_path) = qoder_ipc_path(root)? else {
        return Ok(None);
    };
    let stream = UnixStream::connect(socket_path)
        .map_err(|_| "Qoder local service is not running.".to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_millis(1500)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_millis(1500)))
        .ok();
    qoder_rpc_stream(stream, method)
}

#[cfg(windows)]
fn qoder_rpc(root: &Path, method: &str) -> Result<Option<Value>, String> {
    let Some(pipe_path) = qoder_ipc_path(root)? else {
        return Ok(None);
    };
    let stream = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pipe_path)
        .map_err(|_| "Qoder local service is not running.".to_string())?;
    qoder_rpc_stream(stream, method)
}

#[cfg(not(any(unix, windows)))]
fn qoder_rpc(_root: &Path, _method: &str) -> Result<Option<Value>, String> {
    Ok(None)
}

fn qoder_ipc_path(root: &Path) -> Result<Option<String>, String> {
    let Some(info) = read_json_file(&root.join("SharedClientCache/.info.json")) else {
        return Ok(None);
    };
    info.get("ipcServerPath")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .map(Some)
        .ok_or_else(|| "Qoder local service endpoint is unavailable.".to_string())
}

fn qoder_rpc_stream(mut stream: impl Read + Write, method: &str) -> Result<Option<Value>, String> {
    let body = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": method, "params": {}
    }))
    .map_err(|_| "Qoder local request could not be encoded.".to_string())?;
    write!(stream, "Content-Length: {}\r\n\r\n", body.len())
        .and_then(|_| stream.write_all(&body))
        .map_err(|_| "Qoder local service request failed.".to_string())?;
    let mut response = Vec::new();
    let marker = b"\r\n\r\n";
    loop {
        let mut chunk = [0_u8; 8192];
        let read = stream
            .read(&mut chunk)
            .map_err(|_| "Qoder local service response could not be read.".to_string())?;
        if read == 0 {
            break;
        }
        response.extend_from_slice(&chunk[..read]);
        if response.len() > 4 * 1024 * 1024 {
            return Err("Qoder local service response is too large.".into());
        }
        if let Some(header_end) = response
            .windows(marker.len())
            .position(|window| window == marker)
        {
            let headers = String::from_utf8_lossy(&response[..header_end]);
            if let Some(content_length) = headers.lines().find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            }) && response.len() >= header_end + marker.len() + content_length
            {
                break;
            }
        }
    }
    let header_end = response
        .windows(marker.len())
        .position(|window| window == marker)
        .ok_or_else(|| "Qoder local service returned an invalid response.".to_string())?;
    let headers = String::from_utf8_lossy(&response[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        })
        .ok_or_else(|| "Qoder local service returned an invalid response.".to_string())?;
    let start = header_end + marker.len();
    let end = start
        .checked_add(content_length)
        .filter(|end| *end <= response.len())
        .ok_or_else(|| "Qoder local service returned an incomplete response.".to_string())?;
    let message: Value = serde_json::from_slice(&response[start..end])
        .map_err(|_| "Qoder local service returned invalid JSON.".to_string())?;
    if message.get("error").is_some_and(|error| !error.is_null()) {
        return Err("Qoder local service request failed.".into());
    }
    Ok(message.get("result").cloned())
}

fn normalize_qoder_rpc(
    body: &Value,
    edition: &str,
    window_key: &str,
    now: DateTime<Utc>,
) -> Result<ProviderLimits, LimitFetchError> {
    let quota = body
        .get("userQuota")
        .ok_or_else(|| LimitFetchError::transient("Qoder local response is missing userQuota"))?;
    let used = number(quota.get("used")).ok_or_else(|| {
        LimitFetchError::transient("Qoder local response is missing used credits")
    })?;
    let limit = number(quota.get("total")).ok_or_else(|| {
        LimitFetchError::transient("Qoder local response is missing total credits")
    })?;
    let percent = number(body.get("totalUsagePercentage"))
        .or_else(|| number(quota.get("percentage")))
        .unwrap_or_else(|| {
            if limit > 0.0 {
                used / limit * 100.0
            } else {
                0.0
            }
        });
    let quota_exceeded = body.get("isQuotaExceeded").and_then(Value::as_bool) == Some(true);
    let mut window = LimitWindow::new(
        window_key,
        if limit <= 0.0 {
            0.0
        } else if quota_exceeded {
            100.0
        } else {
            percent
        },
    );
    window.quota_exceeded = quota_exceeded;
    window.reset_at = parse_reset(body.get("expiresAt")).filter(|value| value.year() < 2100);
    window.used_amount = Some(used);
    window.limit_amount = Some(limit);
    window.unit = quota
        .get("unit")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| Some("credits".into()));
    let plan = body
        .get("userType")
        .and_then(Value::as_str)
        .map(|plan| format!("{edition} · {}", title_case(plan)))
        .or_else(|| Some(edition.into()));
    let mut limits = ready_limits(Provider::Qoder, plan, vec![window], now);
    limits.source = LimitSource::LocalApp;
    Ok(limits)
}

fn normalize_qoder_api(
    body: &Value,
    edition: &str,
    window_key: &str,
    now: DateTime<Utc>,
) -> Result<ProviderLimits, LimitFetchError> {
    let summary = body
        .pointer("/totalQuota/quotaSummary")
        .or_else(|| body.pointer("/total_quota/quota_summary"))
        .ok_or_else(|| {
            LimitFetchError::transient("Qoder usage response is missing quota summary")
        })?;
    let mut used = number(
        summary
            .get("usedValue")
            .or_else(|| summary.get("used_value")),
    )
    .unwrap_or_default();
    let mut limit = number(
        summary
            .get("limitValue")
            .or_else(|| summary.get("limit_value")),
    )
    .unwrap_or_default();
    if let Some(shared) = body
        .pointer("/sharedQuota/quotaSummary")
        .or_else(|| body.pointer("/shared_quota/quota_summary"))
    {
        used += number(shared.get("usedValue").or_else(|| shared.get("used_value")))
            .unwrap_or_default();
        limit += number(
            shared
                .get("limitValue")
                .or_else(|| shared.get("limit_value")),
        )
        .unwrap_or_default();
    }
    let percent = if limit > 0.0 {
        used / limit * 100.0
    } else {
        0.0
    };
    let mut window = LimitWindow::new(window_key, percent);
    window.reset_at = parse_reset(
        body.get("nextResetAt")
            .or_else(|| body.get("next_reset_at")),
    );
    window.used_amount = Some(used);
    window.limit_amount = Some(limit);
    window.unit = summary
        .get("unit")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| Some("credits".into()));
    Ok(ready_limits(
        Provider::Qoder,
        Some(edition.into()),
        vec![window],
        now,
    ))
}

fn merge_qoder_limits(limits: Vec<ProviderLimits>, now: DateTime<Utc>) -> ProviderLimits {
    let source = if limits
        .iter()
        .all(|limits| limits.source == LimitSource::LocalApp)
    {
        LimitSource::LocalApp
    } else {
        LimitSource::ProviderApi
    };
    let plans = limits
        .iter()
        .filter_map(|limits| limits.plan.as_deref())
        .fold(Vec::<&str>::new(), |mut plans, plan| {
            if !plans.contains(&plan) {
                plans.push(plan);
            }
            plans
        });
    let mut merged = ready_limits(
        Provider::Qoder,
        (!plans.is_empty()).then(|| plans.join(" / ")),
        limits
            .into_iter()
            .flat_map(|limits| limits.windows)
            .collect(),
        now,
    );
    merged.source = source;
    merged
}

fn merge_partial_qoder_cache(
    mut live: ProviderLimits,
    mut cached: ProviderLimits,
    now: DateTime<Utc>,
) -> ProviderLimits {
    if cached.cache_is_too_old(now) {
        live.last_error = None;
        return live;
    }
    cached.retain_unexpired_windows(now);
    let mut appended = false;
    for window in cached.windows {
        if !live
            .windows
            .iter()
            .any(|candidate| candidate.key == window.key)
        {
            live.windows.push(window);
            appended = true;
        }
    }
    if appended {
        live.plan = cached.plan.or(live.plan);
        live.stale = true;
    } else {
        live.last_error = None;
    }
    live
}

fn fetch_trae(home: &Path, platform: &str) -> ProviderFetch {
    let root = trae_root(home, platform);
    let storage = root.join("User/globalStorage/storage.json");
    let cn_storage = trae_cn_root(home, platform).join("User/globalStorage/storage.json");
    let entitlement = read_entitlement(&storage);
    if entitlement.is_none() && has_trae_cn_auth(&cn_storage) {
        let mut limits = ready_limits(
            Provider::Trae,
            Some("TRAE SOLO CN".into()),
            Vec::new(),
            Utc::now(),
        );
        limits.source = LimitSource::LocalApp;
        return ProviderFetch::Ready(limits);
    }
    let Some(entitlement) = entitlement else {
        if !storage.is_file() {
            return ProviderFetch::NotConfigured;
        }
        return ProviderFetch::Failed(LimitFetchError::transient(
            "TRAE entitlement information could not be read.",
        ));
    };
    let plan = entitlement
        .get("identityStr")
        .and_then(Value::as_str)
        .map(str::to_string);
    let fast_requests = entitlement
        .pointer("/detail/fastRequestPer")
        .and_then(|value| number(Some(value)));
    let mut windows = Vec::new();
    if let Some(limit) = fast_requests {
        let mut window = LimitWindow::new("trae_fast_requests", 0.0);
        window.limit_amount = Some(limit);
        window.unit = Some("requests/hour".into());
        window.usage_known = false;
        windows.push(window);
    }
    if windows.is_empty() && plan.is_none() {
        return ProviderFetch::Failed(LimitFetchError::transient(
            "TRAE entitlement response contained no plan information.",
        ));
    }
    let mut limits = ready_limits(Provider::Trae, plan, windows, Utc::now());
    limits.source = LimitSource::LocalApp;
    ProviderFetch::Ready(limits)
}

fn fetch_grok(home: &Path, http: &dyn LimitHttpClient) -> ProviderFetch {
    let grok_home = std::env::var_os("TOKENTRACKER_GROK_HOME")
        .or_else(|| std::env::var_os("GROK_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".grok"));
    let Some(auth) = read_json_file(&grok_home.join("auth.json")) else {
        return ProviderFetch::NotConfigured;
    };
    let token = auth.as_object().and_then(|entries| {
        entries.values().find_map(|entry| {
            entry
                .get("key")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        })
    });
    let Some(token) = token else {
        return ProviderFetch::Failed(LimitFetchError::auth(
            "Grok login expired. Run `grok login` to sign in again.",
        ));
    };
    let authorization = format!("Bearer {token}");
    let headers = [
        ("Authorization", authorization.as_str()),
        ("Accept", "application/json"),
    ];
    let credits_url = format!("{GROK_BILLING_URL}?format=credits");
    let body = match http.get_json(&credits_url, &headers) {
        Ok(body) => Ok(body),
        Err(error) if matches!(error.status, Some(401 | 403)) => Err(LimitFetchError::auth(
            "Grok login expired. Run `grok login` to sign in again.",
        )),
        Err(first) => http.get_json(GROK_BILLING_URL, &headers).map_err(|second| {
            if matches!(second.status, Some(401 | 403)) {
                LimitFetchError::auth("Grok login expired. Run `grok login` to sign in again.")
            } else {
                LimitFetchError::transient(format!(
                    "Grok billing request failed: {}; fallback: {}",
                    first.message, second.message
                ))
            }
        }),
    };
    match body.and_then(|body| normalize_grok(&body, Utc::now())) {
        Ok(limits) => ProviderFetch::Ready(limits),
        Err(error) => ProviderFetch::Failed(error),
    }
}

fn normalize_grok(body: &Value, now: DateTime<Utc>) -> Result<ProviderLimits, LimitFetchError> {
    let Some(config) = body.get("config") else {
        return Err(LimitFetchError::transient(
            "Grok billing response is missing config",
        ));
    };
    let current = config
        .get("currentPeriod")
        .filter(|value| value.is_object());
    let reset_at = parse_reset(
        current
            .and_then(|value| value.get("end"))
            .or_else(|| config.get("billingPeriodEnd")),
    );
    let key = grok_period_key(current, config);
    let mut used_percent = number(config.get("creditUsagePercent"));
    if used_percent.is_none() {
        used_percent = config
            .get("productUsage")
            .and_then(Value::as_array)
            .and_then(|items| {
                let values = items
                    .iter()
                    .filter_map(|entry| number(entry.get("usagePercent")))
                    .collect::<Vec<_>>();
                (!values.is_empty()).then(|| values.into_iter().sum())
            });
    }
    let limit_amount = nested_number(config.get("monthlyLimit"));
    let used_amount = nested_number(config.get("used"));
    if used_percent.is_none() {
        used_percent = used_amount
            .zip(limit_amount)
            .filter(|(_, total)| *total > 0.0)
            .map(|(used, total)| used / total * 100.0);
    }
    if used_percent.is_none()
        && current.is_some()
        && config.get("creditUsagePercent").is_none()
        && config.get("productUsage").is_none()
    {
        used_percent = Some(0.0);
    }

    let mut windows = Vec::new();
    if let Some(percent) = used_percent {
        let mut window = LimitWindow::new(key, percent);
        window.reset_at = reset_at;
        window.used_amount = used_amount;
        window.limit_amount = limit_amount;
        window.unit = limit_amount.map(|_| "credits".into());
        windows.push(window);
    }
    let on_demand_limit = nested_number(config.get("onDemandCap"));
    let on_demand_used = nested_number(config.get("onDemandUsed"));
    if let Some((used, limit)) = on_demand_used
        .zip(on_demand_limit)
        .filter(|(_, limit)| *limit > 0.0)
    {
        let mut window = LimitWindow::new("on_demand", used / limit * 100.0);
        window.reset_at = reset_at;
        window.used_amount = Some(used);
        window.limit_amount = Some(limit);
        window.unit = Some("credits".into());
        windows.push(window);
    }
    if windows.is_empty() {
        return Err(LimitFetchError::transient(
            "Grok billing response contained no quota windows",
        ));
    }
    Ok(ready_limits(Provider::Grok, None, windows, now))
}

fn grok_period_key(current: Option<&Value>, config: &Value) -> &'static str {
    if let Some(period) = current
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str)
    {
        let upper = period.to_ascii_uppercase();
        if upper.contains("WEEK") {
            return "weekly";
        }
        if upper.contains("MONTH") {
            return "monthly";
        }
        if upper.contains("DAILY") || upper.ends_with("_DAY") || upper == "DAY" {
            return "daily";
        }
    }
    let start = current
        .and_then(|value| value.get("start"))
        .or_else(|| config.get("billingPeriodStart"))
        .and_then(|value| parse_reset(Some(value)));
    let end = current
        .and_then(|value| value.get("end"))
        .or_else(|| config.get("billingPeriodEnd"))
        .and_then(|value| parse_reset(Some(value)));
    if let Some((start, end)) = start.zip(end) {
        let days = (end - start).num_hours() as f64 / 24.0;
        if (0.5..=1.5).contains(&days) {
            return "daily";
        }
        if (1.5..=8.0).contains(&days) {
            return "weekly";
        }
    }
    "monthly"
}

fn ready_limits(
    provider: Provider,
    plan: Option<String>,
    windows: Vec<LimitWindow>,
    captured_at: DateTime<Utc>,
) -> ProviderLimits {
    ProviderLimits {
        provider,
        configured: true,
        plan,
        windows,
        captured_at,
        source: LimitSource::ProviderApi,
        stale: false,
        error: None,
        last_error: None,
    }
}

fn read_json_file(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
    })
}

fn nested_number(value: Option<&Value>) -> Option<f64> {
    value.and_then(|value| number(value.get("val")).or_else(|| number(Some(value))))
}

fn parse_reset(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let value = value?;
    if let Some(raw) = value.as_i64() {
        let seconds = if raw > 10_000_000_000 {
            raw / 1000
        } else {
            raw
        };
        return Utc.timestamp_opt(seconds, 0).single();
    }
    if let Some(raw) = value.as_f64() {
        let seconds = if raw > 10_000_000_000.0 {
            (raw / 1000.0) as i64
        } else {
            raw as i64
        };
        return Utc.timestamp_opt(seconds, 0).single();
    }
    DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|date| date.with_timezone(&Utc))
}

fn jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use chrono::Datelike;
    use serde_json::json;

    use super::*;

    #[test]
    fn normalizes_claude_windows_and_scoped_models() {
        let result = normalize_claude(
            &json!({
                "five_hour": { "utilization": 12.5, "resets_at": "2026-08-21T05:00:00Z" },
                "seven_day": { "utilization": 45, "resets_at": "2026-08-27T00:00:00Z" },
                "limits": [{
                    "kind": "weekly_scoped",
                    "percent": 9,
                    "resets_at": "2026-08-27T00:00:00Z",
                    "scope": { "model": { "display_name": "Fable" } }
                }]
            }),
            Some("Max".into()),
            Utc::now(),
        )
        .unwrap();

        assert_eq!(result.windows.len(), 3);
        assert_eq!(result.windows[0].key, "five_hour");
        assert_eq!(result.windows[2].key, "model:Fable");
    }

    #[test]
    fn claude_legacy_opus_window_is_not_duplicated_by_scoped_limits() {
        let result = normalize_claude(
            &json!({
                "seven_day_opus": { "utilization": 20 },
                "limits": [{
                    "kind": "weekly_scoped",
                    "percent": 20,
                    "scope": { "model": { "display_name": "Opus" } }
                }]
            }),
            None,
            Utc::now(),
        )
        .unwrap();

        assert_eq!(result.windows.len(), 1);
        assert_eq!(result.windows[0].key, "opus");
    }

    #[test]
    fn classifies_reversed_codex_windows_by_duration() {
        let result = normalize_codex(
            &json!({
                "rate_limit": {
                    "primary_window": { "used_percent": 30, "limit_window_seconds": 604800, "reset_at": 1785542400 },
                    "secondary_window": { "used_percent": 12, "limit_window_seconds": 18000, "reset_at": 1785530000 }
                }
            }),
            Some("Plus".into()),
            Utc::now(),
        )
        .unwrap();

        assert_eq!(result.windows[0].key, "five_hour");
        assert_eq!(result.windows[0].used_percent, 12.0);
        assert_eq!(result.windows[1].key, "seven_day");
    }

    #[test]
    fn preserves_unknown_codex_window_next_to_classified_weekly_window() {
        let result = normalize_codex(
            &json!({
                "rate_limit": {
                    "primary_window": { "used_percent": 12 },
                    "secondary_window": { "used_percent": 30, "limit_window_seconds": 604800 }
                }
            }),
            None,
            Utc::now(),
        )
        .unwrap();

        assert_eq!(result.windows.len(), 2);
        assert_eq!(result.windows[0].key, "five_hour");
        assert_eq!(result.windows[1].key, "seven_day");
    }

    #[test]
    fn normalizes_codex_credit_window_from_amounts() {
        let result = normalize_codex(
            &json!({
                "spend_control": { "individual_limit": {
                    "limit": "37500", "used": "51.0", "remaining": "37449", "used_percent": 0,
                    "reset_at": 1785542400
                }}
            }),
            None,
            Utc::now(),
        )
        .unwrap();
        let credits = &result.windows[0];

        assert_eq!(credits.key, "credits");
        assert!((credits.used_percent - 0.136).abs() < 0.001);
        assert_eq!(credits.limit_amount, Some(37_500.0));
    }

    #[test]
    fn normalizes_grok_shared_weekly_pool_and_on_demand() {
        let result = normalize_grok(
            &json!({ "config": {
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-08-17T00:00:00Z",
                    "end": "2026-08-24T00:00:00Z"
                },
                "productUsage": [
                    { "usagePercent": 17 }, { "usagePercent": 1 }
                ],
                "onDemandCap": { "val": 50 },
                "onDemandUsed": { "val": 25 }
            }}),
            Utc::now(),
        )
        .unwrap();

        assert_eq!(result.windows[0].key, "weekly");
        assert_eq!(result.windows[0].used_percent, 18.0);
        assert_eq!(result.windows[1].used_percent, 50.0);
        assert_eq!(result.windows[0].reset_at.unwrap().year(), 2026);
    }

    #[test]
    fn normalizes_cursor_plan_auto_and_api_lanes() {
        let result = normalize_cursor(
            &json!({
                "membershipType": "pro",
                "billingCycleStart": "2026-08-01T00:00:00Z",
                "billingCycleEnd": "2026-09-01T00:00:00Z",
                "individualUsage": { "plan": {
                    "totalPercentUsed": 42.5,
                    "autoPercentUsed": 30,
                    "apiPercentUsed": 55
                }}
            }),
            Utc::now(),
        )
        .unwrap();

        assert_eq!(result.provider, Provider::Cursor);
        assert_eq!(result.plan.as_deref(), Some("Pro"));
        assert_eq!(result.windows.len(), 3);
        assert_eq!(result.windows[0].key, "cursor_plan");
        assert_eq!(result.windows[0].used_percent, 42.5);
        assert_eq!(result.windows[0].window_seconds, Some(31 * 24 * 60 * 60));
    }

    #[test]
    fn cursor_uses_team_pool_when_individual_plan_is_empty() {
        let result = normalize_cursor(
            &json!({
                "membershipType": "enterprise",
                "individualUsage": { "plan": { "totalPercentUsed": 0 }},
                "teamUsage": { "onDemand": { "used": 2500, "limit": 10000 }}
            }),
            Utc::now(),
        )
        .unwrap();

        assert_eq!(result.windows[0].used_percent, 25.0);
    }

    #[test]
    fn normalizes_qoder_rpc_credits_and_no_expiry_sentinel() {
        let result = normalize_qoder_rpc(
            &json!({
                "userType": "personal_standard",
                "totalUsagePercentage": 25,
                "expiresAt": "9999-12-31T00:00:00Z",
                "userQuota": { "used": 25, "total": 100, "remaining": 75, "unit": "credits" }
            }),
            "Qoder",
            "qoder_credits",
            Utc::now(),
        )
        .unwrap();

        assert_eq!(result.provider, Provider::Qoder);
        assert_eq!(result.source, LimitSource::LocalApp);
        assert_eq!(result.windows[0].limit_amount, Some(100.0));
        assert_eq!(result.windows[0].reset_at, None);
    }

    #[test]
    fn qoder_zero_quota_does_not_invent_full_usage() {
        let result = normalize_qoder_rpc(
            &json!({
                "userType": "personal_standard",
                "totalUsagePercentage": 0,
                "isQuotaExceeded": true,
                "userQuota": { "used": 0, "total": 0, "remaining": 0, "unit": "credits" }
            }),
            "Qoder",
            "qoder_credits",
            Utc::now(),
        )
        .unwrap();

        assert_eq!(result.windows[0].used_percent, 0.0);
        assert!(result.windows[0].quota_exceeded);
    }

    #[test]
    fn qoder_api_combines_base_and_shared_credits() {
        let result = normalize_qoder_api(
            &json!({
                "totalQuota": { "quotaSummary": {
                    "usedValue": 20, "limitValue": 100, "unit": "credits"
                }},
                "sharedQuota": { "quotaSummary": {
                    "usedValue": 10, "limitValue": 50, "unit": "credits"
                }},
                "nextResetAt": "2026-09-01T00:00:00Z"
            }),
            "Qoder",
            "qoder_credits",
            Utc::now(),
        )
        .unwrap();

        assert_eq!(result.windows[0].used_amount, Some(30.0));
        assert_eq!(result.windows[0].limit_amount, Some(150.0));
        assert_eq!(result.windows[0].used_percent, 20.0);
    }

    #[test]
    fn qoder_editions_keep_independent_limit_windows() {
        let international = normalize_qoder_rpc(
            &json!({
                "userQuota": { "used": 20, "total": 100, "remaining": 80 }
            }),
            "Qoder",
            "qoder_credits",
            Utc::now(),
        )
        .unwrap();
        let china = normalize_qoder_rpc(
            &json!({
                "userQuota": { "used": 30, "total": 200, "remaining": 170 }
            }),
            "Qoder CN",
            "qoder_cn_credits",
            Utc::now(),
        )
        .unwrap();

        let merged = merge_qoder_limits(vec![international, china], Utc::now());

        assert_eq!(merged.windows.len(), 2);
        assert_eq!(merged.windows[0].key, "qoder_credits");
        assert_eq!(merged.windows[1].key, "qoder_cn_credits");
        assert!(merged.plan.as_deref().unwrap().contains("Qoder CN"));
    }

    #[test]
    fn qoder_partial_refresh_preserves_other_edition_from_cache() {
        let now = Utc::now();
        let mut live = ready_limits(
            Provider::Qoder,
            Some("Qoder".into()),
            vec![LimitWindow::new("qoder_credits", 20.0)],
            now,
        );
        live.last_error = Some("Qoder CN local service is unavailable".into());
        let cached = ready_limits(
            Provider::Qoder,
            Some("Qoder / Qoder CN".into()),
            vec![LimitWindow::new("qoder_cn_credits", 30.0)],
            now - chrono::Duration::minutes(5),
        );

        let merged = merge_partial_qoder_cache(live, cached, now);

        assert_eq!(merged.windows.len(), 2);
        assert!(merged.stale);
        assert!(merged.last_error.is_some());
    }

    #[test]
    fn grok_empty_product_usage_is_not_reported_as_zero_percent() {
        let result = normalize_grok(&json!({ "config": { "productUsage": [] } }), Utc::now());

        assert!(result.is_err());
    }

    #[test]
    fn decodes_padded_jwt_payloads() {
        let payload =
            URL_SAFE.encode(br#"{"https://api.openai.com/auth":{"chatgpt_plan_type":"plus"}}"#);
        let decoded = jwt_payload(&format!("header.{payload}.signature")).unwrap();

        assert_eq!(
            decoded.pointer("/https:~1~1api.openai.com~1auth/chatgpt_plan_type"),
            Some(&json!("plus"))
        );
    }
}
