//! Provider account balance / quota cards for `GET /api/provider-quotas`.
//!
//! One card per distinct provider account (endpoint URL and API key). The
//! config only names providers and their base URLs; a small host -> endpoint
//! table (below) decides which upstream usage API answers for it. Every field
//! is always serialized, and `kind` is one of `balance` / `quota` / `error`.
//!
//! Failure policy: a provider never aborts the response. Non-2xx responses,
//! timeouts, connection failures, and unparsable bodies all become that
//! provider's `kind: "error"` card with one of the fixed, short messages
//! below — never the upstream body and never a credential.

use std::sync::OnceLock;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use serde::Serialize;
use serde_json::Value;
use url::Url;

use crate::codex_auth::CodexAuth;
use crate::config::ProviderAccount;

/// Per-request timeout; every provider is queried concurrently.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Total per-provider budget, covering both credential acquisition and the
/// quota request. It is a ceiling on one card, not on the whole response:
/// providers still run concurrently, so one slow account cannot stretch the
/// endpoint beyond this budget.
const PROVIDER_BUDGET: Duration = Duration::from_secs(20);

const ERR_EXPIRED: &str = "API 密钥已过期，请重新生成";
const ERR_UNAUTHORIZED: &str = "认证失败（HTTP 401）";
const ERR_NOT_LOGGED_IN: &str = "未登录 ChatGPT（运行 e-agent login）";
const ERR_NO_KEY: &str = "未配置可用的 API 密钥";
const ERR_PAYMENT_REQUIRED: &str = "余额不足（HTTP 402）";
const ERR_NOT_FOUND: &str = "查询接口不可用（HTTP 404）";
const ERR_RATE_LIMITED: &str = "请求过于频繁（HTTP 429）";
const ERR_TIMEOUT: &str = "请求超时";
const ERR_QUERY_TIMEOUT: &str = "查询超时";
const ERR_CONNECT: &str = "连接失败";
const ERR_PARSE: &str = "响应格式无法解析";

const CHATGPT_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

/// The response body of `GET /api/provider-quotas`.
#[derive(Debug, Serialize)]
pub struct ProviderQuotas {
    pub providers: Vec<ProviderQuota>,
}

#[derive(Debug, Serialize)]
pub struct ProviderQuota {
    pub id: String,
    pub label: String,
    pub kind: QuotaKind,
    pub amount: Option<f64>,
    pub currency: Option<String>,
    pub plan: Option<String>,
    pub windows: Vec<QuotaWindow>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum QuotaKind {
    Balance,
    Quota,
    Error,
}

#[derive(Debug, Serialize, PartialEq, Clone)]
pub struct QuotaWindow {
    pub label: String,
    pub used_percent: f64,
    pub reset_at: Option<String>,
}

/// Which upstream usage API answers for a provider. `ChatGpt` is selected by
/// `auth = "chatgpt"` instead of by host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    DeepSeek,
    SiliconFlow,
    Zhipu,
    Kimi,
    OpenCode,
    ChatGpt,
}

impl Source {
    /// The card label shown by the UI.
    fn label(self) -> &'static str {
        match self {
            Self::DeepSeek => "DeepSeek",
            Self::SiliconFlow => "SiliconFlow",
            Self::Zhipu => "GLM Coding Plan",
            Self::Kimi => "Kimi Code",
            Self::OpenCode => "OpenCode Go",
            Self::ChatGpt => "ChatGPT",
        }
    }
}

/// Map a provider base URL host to its usage endpoint. Only the host
/// matters: any path prefix on `base_url` is ignored (the same provider is
/// reached through `https://api.z.ai/...`, `/api/coding`, or a bare host).
fn endpoint(host: &str) -> Option<(&'static str, Source)> {
    Some(match host {
        "api.deepseek.com" => ("https://api.deepseek.com/user/balance", Source::DeepSeek),
        "api.siliconflow.cn" => (
            "https://api.siliconflow.cn/v1/user/info",
            Source::SiliconFlow,
        ),
        "open.bigmodel.cn" => (
            "https://open.bigmodel.cn/api/monitor/usage/quota/limit",
            Source::Zhipu,
        ),
        "api.z.ai" => (
            "https://api.z.ai/api/monitor/usage/quota/limit",
            Source::Zhipu,
        ),
        "api.kimi.com" => ("https://api.kimi.com/coding/v1/usages", Source::Kimi),
        "api.kimi.ai" => ("https://api.kimi.ai/coding/v1/usages", Source::Kimi),
        "opencode.ai" | "api.opencode.ai" => {
            ("https://opencode.ai/zen/go/v1/usage", Source::OpenCode)
        }
        _ => return None,
    })
}

/// One provider endpoint to query. `id` is the provider name from the config
/// (the first sorted name wins when several providers share an endpoint).
#[derive(Debug)]
struct Target {
    id: String,
    url: &'static str,
    source: Source,
    api_key: String,
}

/// Resolve configured providers to endpoints, skipping unknown hosts and
/// duplicates. Deduplication is by endpoint URL *and* API key: aliases of one
/// provider (`opencode.ai` / `api.opencode.ai`) with the same key produce a
/// single card, while the same host with different keys stays two accounts.
/// Accounts arrive sorted by provider name, so the surviving `id` is stable.
fn targets(accounts: &[ProviderAccount]) -> Vec<Target> {
    let mut targets: Vec<Target> = Vec::new();
    for account in accounts {
        let resolved = if account.auth.as_deref() == Some("chatgpt") {
            Some((CHATGPT_USAGE_URL, Source::ChatGpt, String::new()))
        } else {
            account
                .base_url
                .as_deref()
                .and_then(|base_url| Url::parse(base_url).ok())
                .and_then(|url| url.host_str().and_then(endpoint))
                .map(|(url, source)| (url, source, account.api_key.clone()))
        };
        let Some((url, source, api_key)) = resolved else {
            continue;
        };
        if targets
            .iter()
            .any(|target| target.url == url && target.api_key == api_key)
        {
            continue;
        }
        targets.push(Target {
            id: account.name.clone(),
            url,
            source,
            api_key,
        });
    }
    targets
}

/// A provider that exceeded its budget still gets its own error card, using
/// the same fixed-message shape as every other failure.
fn timeout_card(target: &Target) -> ProviderQuota {
    ProviderQuota::error(
        &target.id,
        target.source.label(),
        ERR_QUERY_TIMEOUT.to_owned(),
    )
}

/// Query every configured provider concurrently. Never fails: an upstream
/// problem becomes that provider's error card. Each provider runs under its
/// own `PROVIDER_BUDGET`, so a slow credential refresh only times out that
/// provider's card while the others still answer.
pub async fn fetch(accounts: &[ProviderAccount], chatgpt: Option<CodexAuth>) -> ProviderQuotas {
    let client = client();
    let now = Utc::now();
    let cards = targets(accounts).into_iter().map(|target| {
        let client = client.clone();
        let chatgpt = chatgpt.clone();
        async move {
            let query = query(&client, &target, chatgpt.as_ref(), now);
            tokio::time::timeout(PROVIDER_BUDGET, query)
                .await
                .unwrap_or_else(|_| timeout_card(&target))
        }
    });
    ProviderQuotas {
        providers: futures_util::future::join_all(cards).await,
    }
}

impl ProviderQuota {
    fn balance(id: &str, label: &str, amount: f64, currency: Option<String>) -> Self {
        Self {
            id: id.to_owned(),
            label: label.to_owned(),
            kind: QuotaKind::Balance,
            amount: Some(amount),
            currency,
            plan: None,
            windows: Vec::new(),
            error: None,
        }
    }

    fn quota(id: &str, label: &str, plan: Option<String>, windows: Vec<QuotaWindow>) -> Self {
        Self {
            id: id.to_owned(),
            label: label.to_owned(),
            kind: QuotaKind::Quota,
            amount: None,
            currency: None,
            plan,
            windows,
            error: None,
        }
    }

    fn error(id: &str, label: &str, error: String) -> Self {
        Self {
            id: id.to_owned(),
            label: label.to_owned(),
            kind: QuotaKind::Error,
            amount: None,
            currency: None,
            plan: None,
            windows: Vec::new(),
            error: Some(error),
        }
    }
}

async fn query(
    client: &reqwest::Client,
    target: &Target,
    chatgpt: Option<&CodexAuth>,
    now: DateTime<Utc>,
) -> ProviderQuota {
    let label = target.source.label();
    let parsed = match target.source {
        Source::ChatGpt => match chatgpt {
            Some(auth) => chatgpt_card(client, auth, now, &target.id, label).await,
            // Configured for ChatGPT auth, but no login is available: name
            // the actual remedy instead of reporting a generic HTTP 401.
            None => Err(ERR_NOT_LOGGED_IN.to_owned()),
        },
        // A configured provider whose credential could not be resolved keeps
        // its card; sending an empty `Bearer` would report a misleading 401.
        _ if target.api_key.is_empty() => Err(ERR_NO_KEY.to_owned()),
        source => match client
            .get(target.url)
            .bearer_auth(&target.api_key)
            .send()
            .await
        {
            Ok(response) => json_body(response)
                .await
                .and_then(|value| parse(source, &value, now, &target.id, label)),
            Err(error) => Err(transport(error)),
        },
    };
    match parsed {
        Ok(card) => card,
        Err(error) => ProviderQuota::error(&target.id, label, error),
    }
}

fn parse(
    source: Source,
    value: &Value,
    now: DateTime<Utc>,
    id: &str,
    label: &str,
) -> Result<ProviderQuota, String> {
    match source {
        Source::DeepSeek => parse_deepseek(value, id, label),
        Source::SiliconFlow => parse_siliconflow(value, id, label),
        Source::Zhipu => parse_zhipu(value, id, label),
        Source::Kimi => parse_kimi(value, id, label),
        Source::OpenCode => parse_opencode(value, now, id, label),
        Source::ChatGpt => parse_chatgpt(value, now, id, label),
    }
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Upstream parsing (pure; no I/O)
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

/// `{"is_available":true,"balance_infos":[{"currency":"CNY","total_balance":"10.44",...}]}`
fn parse_deepseek(value: &Value, id: &str, label: &str) -> Result<ProviderQuota, String> {
    let infos = value
        .get("balance_infos")
        .and_then(Value::as_array)
        .ok_or_else(parse_error)?;
    let info = infos
        .iter()
        .find(|info| info.get("currency").and_then(Value::as_str) == Some("CNY"))
        .or_else(|| infos.first())
        .ok_or_else(parse_error)?;
    let amount = info
        .get("total_balance")
        .and_then(number)
        .ok_or_else(parse_error)?;
    Ok(ProviderQuota::balance(
        id,
        label,
        amount,
        info.get("currency")
            .and_then(Value::as_str)
            .map(str::to_owned),
    ))
}

/// `{"code":20000,...,"data":{"balance":"0.88","totalBalance":"88.88"}}`
fn parse_siliconflow(value: &Value, id: &str, label: &str) -> Result<ProviderQuota, String> {
    let amount = value
        .get("data")
        .and_then(|data| data.get("totalBalance"))
        .and_then(number)
        .ok_or_else(parse_error)?;
    Ok(ProviderQuota::balance(
        id,
        label,
        amount,
        Some("CNY".to_owned()),
    ))
}

/// `{"code":200,"data":{"limits":[{"unit":3,"percentage":0,...}],"level":"max"}}`
/// Units: 3 = 5-hour window, 6 = weekly, 5 = monthly. `percentage` is already
/// 0-100; `nextResetTime` is a millisecond timestamp.
fn parse_zhipu(value: &Value, id: &str, label: &str) -> Result<ProviderQuota, String> {
    if value.get("code").and_then(Value::as_i64) != Some(200) {
        return Err(ERR_PARSE.to_owned());
    }
    let data = value.get("data").ok_or_else(parse_error)?;
    let limits = data
        .get("limits")
        .and_then(Value::as_array)
        .ok_or_else(parse_error)?;
    let windows: Vec<QuotaWindow> = limits.iter().filter_map(zhipu_window).collect();
    if windows.is_empty() {
        return Err(parse_error());
    }
    Ok(ProviderQuota::quota(
        id,
        label,
        data.get("level").and_then(Value::as_str).map(str::to_owned),
        windows,
    ))
}

fn zhipu_window(limit: &Value) -> Option<QuotaWindow> {
    let label = match limit.get("unit").and_then(Value::as_i64)? {
        3 => "5 小时",
        6 => "每周",
        5 => "每月",
        _ => return None,
    };
    let used_percent = limit.get("percentage").and_then(number)?;
    let reset_at = limit
        .get("nextResetTime")
        .and_then(Value::as_i64)
        .and_then(rfc3339_millis);
    Some(QuotaWindow {
        label: label.to_owned(),
        used_percent,
        reset_at,
    })
}

/// Prefers the `usages` block; falls back to the legacy `usage` + `limits`
/// shape when it is absent.
fn parse_kimi(value: &Value, id: &str, label: &str) -> Result<ProviderQuota, String> {
    kimi_usages(value)
        .or_else(|| kimi_legacy(value))
        .map(|windows| ProviderQuota::quota(id, label, None, windows))
        .ok_or_else(parse_error)
}

/// `{"usages":{"limit_5h":{"used_ratio":0.0709,"reset_time":"..."},"limit_7d":{...}}}`
fn kimi_usages(value: &Value) -> Option<Vec<QuotaWindow>> {
    let usages = value.get("usages")?;
    let mut windows = Vec::new();
    for (key, label) in [("limit_5h", "5 小时"), ("limit_7d", "每周")] {
        let Some(entry) = usages.get(key) else {
            continue;
        };
        let Some(ratio) = entry.get("used_ratio").and_then(number) else {
            continue;
        };
        windows.push(QuotaWindow {
            label: label.to_owned(),
            used_percent: ratio * 100.0,
            reset_at: entry
                .get("reset_time")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    (!windows.is_empty()).then_some(windows)
}

/// Legacy shape: `usage` is the weekly window, `limits[0].detail` the 5-hour
/// one; both express usage as `used` / `limit`.
fn kimi_legacy(value: &Value) -> Option<Vec<QuotaWindow>> {
    let mut windows = Vec::new();
    let detail = value
        .get("limits")
        .and_then(Value::as_array)
        .and_then(|limits| limits.first())
        .and_then(|limit| limit.get("detail"));
    for (entry, label) in [(detail, "5 小时"), (value.get("usage"), "每周")] {
        let Some(entry) = entry else { continue };
        let Some(used_percent) = kimi_percent(entry) else {
            continue;
        };
        windows.push(QuotaWindow {
            label: label.to_owned(),
            used_percent,
            reset_at: entry
                .get("resetTime")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    (!windows.is_empty()).then_some(windows)
}

fn kimi_percent(entry: &Value) -> Option<f64> {
    let limit = entry.get("limit").and_then(number)?;
    let used = entry.get("used").and_then(number)?;
    (limit > 0.0).then(|| used / limit * 100.0)
}

/// `{"usage":{"rolling":{"percent":12.5,"resetInSec":3600},"weekly":{...}}}`
fn parse_opencode(
    value: &Value,
    now: DateTime<Utc>,
    id: &str,
    label: &str,
) -> Result<ProviderQuota, String> {
    let usage = value.get("usage").ok_or_else(parse_error)?;
    let mut windows = Vec::new();
    for (key, label) in [
        ("rolling", "5 小时"),
        ("weekly", "每周"),
        ("monthly", "每月"),
    ] {
        let Some(window) = usage.get(key) else {
            continue;
        };
        let Some(used_percent) = window.get("percent").and_then(number) else {
            continue;
        };
        windows.push(QuotaWindow {
            label: label.to_owned(),
            used_percent,
            reset_at: window
                .get("resetInSec")
                .and_then(number)
                .and_then(|seconds| rfc3339_after(now, seconds as i64)),
        });
    }
    if windows.is_empty() {
        return Err(ERR_PARSE.to_owned());
    }
    Ok(ProviderQuota::quota(id, label, None, windows))
}

/// `{"plan_type":"pro","rate_limit":{"primary_window":{...},"secondary_window":null}}`
fn parse_chatgpt(
    value: &Value,
    now: DateTime<Utc>,
    id: &str,
    label: &str,
) -> Result<ProviderQuota, String> {
    let rate_limit = value.get("rate_limit");
    let mut windows = Vec::new();
    if let Some(window) = rate_limit
        .and_then(|rate_limit| rate_limit.get("primary_window"))
        .filter(|window| !window.is_null())
        .and_then(|window| {
            let label = chatgpt_window_label(window.get("limit_window_seconds").and_then(number));
            chatgpt_window(window, label, now)
        })
    {
        windows.push(window);
    }
    if let Some(window) = rate_limit
        .and_then(|rate_limit| rate_limit.get("secondary_window"))
        .filter(|window| !window.is_null())
        .and_then(|window| chatgpt_window(window, "次级窗口".to_owned(), now))
    {
        windows.push(window);
    }
    if windows.is_empty() {
        return Err(ERR_PARSE.to_owned());
    }
    Ok(ProviderQuota::quota(
        id,
        label,
        value
            .get("plan_type")
            .and_then(Value::as_str)
            .map(str::to_owned),
        windows,
    ))
}

fn chatgpt_window_label(seconds: Option<f64>) -> String {
    match seconds.map(|seconds| seconds as i64) {
        Some(604_800) => "7 天".to_owned(),
        Some(18_000) => "5 小时".to_owned(),
        Some(86_400) => "1 天".to_owned(),
        Some(hours) => format!("{} 小时", hours / 3600),
        None => "窗口".to_owned(),
    }
}

fn chatgpt_window(window: &Value, label: String, now: DateTime<Utc>) -> Option<QuotaWindow> {
    let used_percent = window.get("used_percent").and_then(number)?;
    let reset_at = window
        .get("reset_at")
        .and_then(Value::as_i64)
        .and_then(rfc3339_seconds)
        .or_else(|| {
            window
                .get("reset_after_seconds")
                .and_then(Value::as_i64)
                .and_then(|seconds| rfc3339_after(now, seconds))
        });
    Some(QuotaWindow {
        label,
        used_percent,
        reset_at,
    })
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// HTTP plumbing and fixed error messages
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

fn client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("cannot build the quota HTTP client")
        })
        .clone()
}

async fn chatgpt_card(
    client: &reqwest::Client,
    auth: &CodexAuth,
    now: DateTime<Utc>,
    id: &str,
    label: &str,
) -> Result<ProviderQuota, String> {
    // Credential refresh must survive this card's timeout: the token endpoint
    // may have already rotated the refresh token, and `refresh_if_needed`
    // only persists it after the exchange returns. Dropping a JoinHandle
    // detaches the task, so a timed-out quota card cannot lose a rotated
    // credential. Only ChatGPT has this exposure; other providers carry a
    // static API key and are queried inline.
    let auth = auth.clone();
    let (token, account) = tokio::spawn(async move { auth.access_token_and_account().await })
        .await
        .map_err(|_| ERR_UNAUTHORIZED.to_owned())?
        .map_err(|_| ERR_UNAUTHORIZED.to_owned())?;
    let response = client
        .get(CHATGPT_USAGE_URL)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header("ChatGPT-Account-ID", account)
        .header("originator", "codex_cli_rs")
        .header(reqwest::header::USER_AGENT, "codex_cli_rs/0.0.0 (e-agent)")
        .send()
        .await
        .map_err(transport)?;
    json_body(response)
        .await
        .and_then(|value| parse_chatgpt(&value, now, id, label))
}

/// Status handling for every provider: non-2xx becomes a fixed message that
/// never contains the upstream body or a credential.
async fn json_body(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    // `text` never surfaces the token: the request's headers are not echoed.
    let body = response.text().await.map_err(|_| ERR_CONNECT.to_owned())?;
    if !status.is_success() {
        return Err(http_error(status.as_u16(), &body));
    }
    serde_json::from_str(&body).map_err(|_| ERR_PARSE.to_owned())
}

fn transport(error: reqwest::Error) -> String {
    if error.is_timeout() {
        ERR_TIMEOUT.to_owned()
    } else {
        ERR_CONNECT.to_owned()
    }
}

fn http_error(status: u16, body: &str) -> String {
    match status {
        401 if key_expired(body) => ERR_EXPIRED.to_owned(),
        401 => ERR_UNAUTHORIZED.to_owned(),
        402 => ERR_PAYMENT_REQUIRED.to_owned(),
        404 => ERR_NOT_FOUND.to_owned(),
        429 => ERR_RATE_LIMITED.to_owned(),
        other => format!("HTTP {other}"),
    }
}

/// Only ever inspects `error.code`; the body itself is discarded.
fn key_expired(body: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    value
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        == Some("api_key_expired")
}

// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
// Helpers
// ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~

fn parse_error() -> String {
    ERR_PARSE.to_owned()
}

/// Numbers and numeric strings are both accepted: these APIs mix them
/// (`"total_balance":"10.44"` vs `"percent":12.5`).
fn number(value: &Value) -> Option<f64> {
    let parsed = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    };
    parsed.filter(|number| number.is_finite())
}

fn rfc3339_seconds(seconds: i64) -> Option<String> {
    DateTime::from_timestamp(seconds, 0).map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn rfc3339_millis(millis: i64) -> Option<String> {
    DateTime::from_timestamp_millis(millis)
        .map(|at| at.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn rfc3339_after(now: DateTime<Utc>, seconds: i64) -> Option<String> {
    let delta = TimeDelta::try_seconds(seconds)?;
    now.checked_add_signed(delta)
        .map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).unwrap()
    }

    fn parse_value(source: Source, body: &str) -> Result<ProviderQuota, String> {
        let value: Value = serde_json::from_str(body).unwrap();
        parse(source, &value, at(1_760_000_000), "test", source.label())
    }

    fn labels(windows: &[QuotaWindow]) -> Vec<&str> {
        windows.iter().map(|w| w.label.as_str()).collect()
    }

    fn close(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-9
    }

    #[test]
    fn non_finite_percentages_are_rejected() {
        assert_eq!(number(&json!("NaN")), None);
        assert_eq!(number(&json!("inf")), None);
        assert_eq!(number(&json!("-inf")), None);
        assert_eq!(number(&json!("12.5")), Some(12.5));
        assert_eq!(number(&json!(12.5)), Some(12.5));
    }

    const DEEPSEEK: &str = r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":"1.50"},{"currency":"CNY","total_balance":"10.44","granted_balance":"0.00","topped_up_balance":"10.44"}]}"#;
    const SILICONFLOW: &str = r#"{"code":20000,"message":"OK","status":true,"data":{"balance":"0.88","chargeBalance":"88.00","totalBalance":"88.88"}}"#;
    const ZHIPU: &str = r#"{"code":200,"data":{"limits":[{"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":28000,"currentValue":0,"remaining":28000,"percentage":0},{"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":140000,"currentValue":0,"remaining":140000,"percentage":0,"nextResetTime":1792985375998}],"level":"max"},"success":true}"#;
    const KIMI: &str = r#"{"usage":{"limit":"100","used":"1","remaining":"99","resetTime":"2026-10-27T06:49:31.626557Z"},"limits":[{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},"detail":{"limit":"100","used":"7","remaining":"93","resetTime":"2026-10-20T11:49:31.626557Z"}}],"usages":{"limit_5h":{"used_ratio":0.0709,"reset_time":"2026-10-20T11:49:31Z"},"limit_7d":{"used_ratio":0.0143,"reset_time":"2026-10-27T06:49:31Z"}}}"#;
    const OPENCODE: &str = r#"{"usage":{"rolling":{"percent":12.5,"resetInSec":3600},"weekly":{"percent":40,"resetInSec":7200},"monthly":{"percent":100,"resetInSec":0}}}"#;
    const CHATGPT: &str = r#"{"plan_type":"pro","rate_limit":{"primary_window":{"used_percent":15,"limit_window_seconds":604800,"reset_after_seconds":596127,"reset_at":1793082793},"secondary_window":null},"credits":{"has_credits":false,"unlimited":false,"balance":"0"}}"#;

    #[test]
    fn deepseek_balance_prefers_cny() {
        let card = parse_value(Source::DeepSeek, DEEPSEEK).unwrap();
        assert_eq!(card.kind, QuotaKind::Balance);
        assert_eq!(card.amount, Some(10.44));
        assert_eq!(card.currency.as_deref(), Some("CNY"));
        assert!(card.windows.is_empty());
        assert_eq!(Source::DeepSeek.label(), "DeepSeek");
    }

    #[test]
    fn deepseek_without_cny_takes_the_first_balance() {
        let body =
            r#"{"is_available":true,"balance_infos":[{"currency":"USD","total_balance":"1.50"}]}"#;
        let card = parse_value(Source::DeepSeek, body).unwrap();
        assert_eq!(card.kind, QuotaKind::Balance);
        assert_eq!(card.amount, Some(1.5));
        assert_eq!(card.currency.as_deref(), Some("USD"));
    }

    #[test]
    fn siliconflow_balance_uses_total() {
        let card = parse_value(Source::SiliconFlow, SILICONFLOW).unwrap();
        assert_eq!(card.kind, QuotaKind::Balance);
        assert_eq!(card.amount, Some(88.88));
        assert_eq!(card.currency.as_deref(), Some("CNY"));
        assert_eq!(Source::SiliconFlow.label(), "SiliconFlow");
    }

    #[test]
    fn zhipu_windows_and_millisecond_reset() {
        let card = parse_value(Source::Zhipu, ZHIPU).unwrap();
        assert_eq!(card.kind, QuotaKind::Quota);
        assert_eq!(card.plan.as_deref(), Some("max"));
        assert_eq!(labels(&card.windows), ["5 小时", "每周"]);
        assert!(close(card.windows[0].used_percent, 0.0));
        assert_eq!(card.windows[0].reset_at, None);
        assert!(close(card.windows[1].used_percent, 0.0));
        // 1792985375998 ms since the epoch, rendered in UTC.
        assert_eq!(
            card.windows[1].reset_at.as_deref(),
            Some("2026-10-26T03:29:35.998Z")
        );
        assert_eq!(Source::Zhipu.label(), "GLM Coding Plan");
    }

    #[test]
    fn zhipu_non_200_code_is_an_error_card() {
        let body = r#"{"code":1001,"message":"bad key","success":false}"#;
        assert_eq!(
            parse_value(Source::Zhipu, body).unwrap_err(),
            "响应格式无法解析"
        );
    }

    #[test]
    fn zhipu_unknown_units_are_skipped() {
        let body = json!({"code":200,"data":{"limits":[
            {"unit":5,"percentage":25.5},
            {"unit":1,"percentage":1.0}
        ]}});
        let card = parse(
            Source::Zhipu,
            &body,
            at(1_760_000_000),
            "test",
            Source::Zhipu.label(),
        )
        .unwrap();
        assert_eq!(labels(&card.windows), ["每月"]);
        assert!(close(card.windows[0].used_percent, 25.5));
    }

    #[test]
    fn zhipu_all_unknown_units_is_an_error_card() {
        let body = r#"{"code":200,"data":{"limits":[{"unit":1,"percentage":1.0},{"unit":2,"percentage":2.0}]}}"#;
        assert_eq!(
            parse_value(Source::Zhipu, body).unwrap_err(),
            "响应格式无法解析"
        );
    }

    #[test]
    fn kimi_usages_win_over_legacy_shape() {
        let card = parse_value(Source::Kimi, KIMI).unwrap();
        assert_eq!(card.kind, QuotaKind::Quota);
        assert_eq!(labels(&card.windows), ["5 小时", "每周"]);
        assert!(close(card.windows[0].used_percent, 7.09));
        assert_eq!(
            card.windows[0].reset_at.as_deref(),
            Some("2026-10-20T11:49:31Z")
        );
        assert!(close(card.windows[1].used_percent, 1.43));
        assert_eq!(
            card.windows[1].reset_at.as_deref(),
            Some("2026-10-27T06:49:31Z")
        );
        assert_eq!(Source::Kimi.label(), "Kimi Code");
    }

    #[test]
    fn kimi_falls_back_to_legacy_shape() {
        let body = json!({
            "usage":{"limit":"100","used":"1","remaining":"99","resetTime":"2026-10-27T06:49:31.626557Z"},
            "limits":[{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},
                "detail":{"limit":"100","used":"7","remaining":"93","resetTime":"2026-10-20T11:49:31.626557Z"}}]
        });
        let card = parse(
            Source::Kimi,
            &body,
            at(1_760_000_000),
            "test",
            Source::Kimi.label(),
        )
        .unwrap();
        assert_eq!(labels(&card.windows), ["5 小时", "每周"]);
        assert!(close(card.windows[0].used_percent, 7.0));
        assert_eq!(
            card.windows[0].reset_at.as_deref(),
            Some("2026-10-20T11:49:31.626557Z")
        );
        assert!(close(card.windows[1].used_percent, 1.0));
        assert_eq!(
            card.windows[1].reset_at.as_deref(),
            Some("2026-10-27T06:49:31.626557Z")
        );
    }

    #[test]
    fn kimi_without_usage_blocks_is_an_error_card() {
        let card = parse_value(Source::Kimi, r#"{"ok":true}"#).unwrap_err();
        assert_eq!(card, "响应格式无法解析");
    }

    #[test]
    fn opencode_windows_from_percent_and_seconds() {
        let card = parse_value(Source::OpenCode, OPENCODE).unwrap();
        assert_eq!(labels(&card.windows), ["5 小时", "每周", "每月"]);
        assert!(close(card.windows[0].used_percent, 12.5));
        // now = 2025-10-09T08:53:20Z.
        assert_eq!(
            card.windows[0].reset_at.as_deref(),
            Some("2025-10-09T09:53:20Z")
        );
        assert_eq!(
            card.windows[1].reset_at.as_deref(),
            Some("2025-10-09T10:53:20Z")
        );
        assert_eq!(
            card.windows[2].reset_at.as_deref(),
            Some("2025-10-09T08:53:20Z")
        );
        assert_eq!(Source::OpenCode.label(), "OpenCode Go");
    }

    #[test]
    fn chatgpt_plan_and_weekly_window() {
        let card = parse_value(Source::ChatGpt, CHATGPT).unwrap();
        assert_eq!(card.plan.as_deref(), Some("pro"));
        assert_eq!(labels(&card.windows), ["7 天"]);
        assert!(close(card.windows[0].used_percent, 15.0));
        assert_eq!(
            card.windows[0].reset_at.as_deref(),
            Some("2026-10-27T06:33:13Z")
        );
        assert_eq!(Source::ChatGpt.label(), "ChatGPT");
    }

    #[test]
    fn chatgpt_secondary_window_and_relative_reset() {
        let body = json!({
            "plan_type":"plus",
            "rate_limit":{
                "primary_window":{"used_percent":5,"limit_window_seconds":18000},
                "secondary_window":{"used_percent":2.5,"reset_after_seconds":600}
            }
        });
        let card = parse(
            Source::ChatGpt,
            &body,
            at(1_760_000_000),
            "test",
            Source::ChatGpt.label(),
        )
        .unwrap();
        assert_eq!(labels(&card.windows), ["5 小时", "次级窗口"]);
        // Neither `reset_at` nor `reset_after_seconds` on the primary window.
        assert_eq!(card.windows[0].reset_at, None);
        // now + reset_after_seconds.
        assert_eq!(
            card.windows[1].reset_at.as_deref(),
            Some("2025-10-09T09:03:20Z")
        );
        assert_eq!(card.plan.as_deref(), Some("plus"));
    }

    #[test]
    fn chatgpt_other_window_lengths_render_in_hours() {
        let body = json!({
            "rate_limit":{
                "primary_window":{"used_percent":1,"limit_window_seconds":172800},
                "secondary_window":{"used_percent":1,"limit_window_seconds":3600}
            }
        });
        let card = parse(
            Source::ChatGpt,
            &body,
            at(1_760_000_000),
            "test",
            Source::ChatGpt.label(),
        )
        .unwrap();
        assert_eq!(labels(&card.windows), ["48 小时", "次级窗口"]);
    }

    #[test]
    fn chatgpt_without_windows_is_an_error_card() {
        assert_eq!(
            parse_value(Source::ChatGpt, "{}").unwrap_err(),
            "响应格式无法解析"
        );
    }

    #[test]
    fn unresolved_credential_becomes_an_explicit_error_card() {
        let target = Target {
            id: "deepseek".to_owned(),
            url: "https://api.deepseek.com/user/balance",
            source: Source::DeepSeek,
            api_key: String::new(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // No network: the empty-key guard answers before any request is sent.
        let card = runtime.block_on(query(&client(), &target, None, at(1_760_000_000)));
        assert_eq!(card.kind, QuotaKind::Error);
        assert_eq!(card.error.as_deref(), Some(ERR_NO_KEY));
        assert_eq!(card.label, "DeepSeek");
    }

    #[test]
    fn missing_chatgpt_login_names_the_remedy() {
        let target = Target {
            id: "chatgpt".to_owned(),
            url: CHATGPT_USAGE_URL,
            source: Source::ChatGpt,
            api_key: String::new(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let card = runtime.block_on(query(&client(), &target, None, at(1_760_000_000)));
        assert_eq!(card.kind, QuotaKind::Error);
        assert_eq!(card.error.as_deref(), Some(ERR_NOT_LOGGED_IN));
    }

    #[test]
    fn expired_key_401_has_its_own_message() {
        let body = r#"{"error":{"code":"api_key_expired","message":"API key expired"}}"#;
        assert_eq!(http_error(401, body), "API 密钥已过期，请重新生成");
        assert_eq!(
            http_error(401, r#"{"error":{"code":"bad_key"}}"#),
            "认证失败（HTTP 401）"
        );
        assert_eq!(http_error(401, "not json"), "认证失败（HTTP 401）");
        assert_eq!(http_error(402, ""), "余额不足（HTTP 402）");
        assert_eq!(http_error(404, ""), "查询接口不可用（HTTP 404）");
        assert_eq!(http_error(429, ""), "请求过于频繁（HTTP 429）");
        assert_eq!(http_error(500, "secret body"), "HTTP 500");
    }

    #[test]
    fn budget_timeout_is_a_fixed_error_card() {
        let target = Target {
            id: "zhipu".to_owned(),
            url: "https://open.bigmodel.cn/api/monitor/usage/quota/limit",
            source: Source::Zhipu,
            api_key: String::new(),
        };
        let card = timeout_card(&target);
        assert_eq!(card.id, "zhipu");
        assert_eq!(card.label, "GLM Coding Plan");
        assert_eq!(card.kind, QuotaKind::Error);
        assert_eq!(card.error.as_deref(), Some("查询超时"));
        assert!(card.windows.is_empty());
    }

    fn account(name: &str, base_url: &str) -> ProviderAccount {
        account_with_key(name, base_url, &format!("key-{name}"))
    }

    fn account_with_key(name: &str, base_url: &str, api_key: &str) -> ProviderAccount {
        ProviderAccount {
            name: name.to_owned(),
            base_url: Some(base_url.to_owned()),
            auth: None,
            api_key: api_key.to_owned(),
        }
    }

    #[test]
    fn unknown_hosts_are_skipped() {
        let accounts = [
            account("local", "http://127.0.0.1:8000/v1"),
            account("proxy", "https://example.com/v1"),
            account("deepseek", "https://api.deepseek.com/v1"),
        ];
        let targets = targets(&accounts);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].id, "deepseek");
        assert_eq!(targets[0].url, "https://api.deepseek.com/user/balance");
    }

    #[test]
    fn path_prefix_is_ignored_and_aliases_dedupe() {
        let accounts = [
            account_with_key(
                "zhipu",
                "https://open.bigmodel.cn/api/coding/paas/v4",
                "zhipu-key",
            ),
            account_with_key("zhipu-alt", "https://open.bigmodel.cn/v4", "zhipu-key"),
            account_with_key("opencode", "https://opencode.ai/zen/go/v1", "opencode-key"),
            account_with_key("opencode-alt", "https://api.opencode.ai/v1", "opencode-key"),
        ];
        let targets = targets(&accounts);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].id, "zhipu");
        assert_eq!(
            targets[0].url,
            "https://open.bigmodel.cn/api/monitor/usage/quota/limit"
        );
        assert_eq!(targets[1].id, "opencode");
        assert_eq!(targets[1].url, "https://opencode.ai/zen/go/v1/usage");
    }

    #[test]
    fn same_host_is_one_card_only_for_the_same_key() {
        let accounts = [
            account_with_key("zhipu-main", "https://open.bigmodel.cn/v4", "key-main"),
            account_with_key("zhipu-backup", "https://open.bigmodel.cn/v4", "key-backup"),
        ];
        let different_keys = targets(&accounts);
        assert_eq!(different_keys.len(), 2);
        assert_eq!(different_keys[0].id, "zhipu-main");
        assert_eq!(different_keys[1].id, "zhipu-backup");

        let same_key = [
            account_with_key("zhipu", "https://open.bigmodel.cn/v4", "one-key"),
            account_with_key(
                "zhipu-alt",
                "https://open.bigmodel.cn/api/coding/paas/v4",
                "one-key",
            ),
        ];
        let deduped = targets(&same_key);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].id, "zhipu");
    }

    #[test]
    fn kimi_hosts_map_to_their_own_endpoints() {
        let accounts = [
            account("kimi", "https://api.kimi.com/coding/v1"),
            account("kimi-ai", "https://api.kimi.ai/coding/v1"),
            account("zai", "https://api.z.ai/api/coding/paas/v4"),
        ];
        let targets = targets(&accounts);
        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0].url, "https://api.kimi.com/coding/v1/usages");
        assert_eq!(targets[1].url, "https://api.kimi.ai/coding/v1/usages");
        assert_eq!(
            targets[2].url,
            "https://api.z.ai/api/monitor/usage/quota/limit"
        );
    }

    #[test]
    fn chatgpt_auth_account_becomes_a_target_without_key() {
        let accounts = [ProviderAccount {
            name: "chatgpt".to_owned(),
            base_url: None,
            auth: Some("chatgpt".to_owned()),
            api_key: String::new(),
        }];
        let targets = targets(&accounts);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].source, Source::ChatGpt);
        assert_eq!(targets[0].url, CHATGPT_USAGE_URL);
        assert!(targets[0].api_key.is_empty());
    }

    /// Every field is serialized, including the `null` ones; `kind` is one of
    /// `balance` / `quota` / `error`.
    #[test]
    fn cards_serialize_every_field() {
        let quotas = ProviderQuotas {
            providers: vec![
                ProviderQuota {
                    id: "deepseek".to_owned(),
                    label: Source::DeepSeek.label().to_owned(),
                    kind: QuotaKind::Balance,
                    amount: Some(10.44),
                    currency: Some("CNY".to_owned()),
                    plan: None,
                    windows: Vec::new(),
                    error: None,
                },
                ProviderQuota {
                    id: "zhipu".to_owned(),
                    label: Source::Zhipu.label().to_owned(),
                    kind: QuotaKind::Quota,
                    amount: None,
                    currency: None,
                    plan: Some("max".to_owned()),
                    windows: vec![QuotaWindow {
                        label: "5 小时".to_owned(),
                        used_percent: 0.0,
                        reset_at: Some("2026-10-26T03:29:35.998Z".to_owned()),
                    }],
                    error: None,
                },
                ProviderQuota::error("opencode", Source::OpenCode.label(), ERR_EXPIRED.to_owned()),
            ],
        };
        assert_eq!(
            serde_json::to_value(&quotas).unwrap(),
            json!({"providers":[
                {"id":"deepseek","label":"DeepSeek","kind":"balance","amount":10.44,
                 "currency":"CNY","plan":null,"windows":[],"error":null},
                {"id":"zhipu","label":"GLM Coding Plan","kind":"quota","amount":null,
                 "currency":null,"plan":"max",
                 "windows":[{"label":"5 小时","used_percent":0.0,
                             "reset_at":"2026-10-26T03:29:35.998Z"}],"error":null},
                {"id":"opencode","label":"OpenCode Go","kind":"error","amount":null,
                 "currency":null,"plan":null,"windows":[],
                 "error":"API 密钥已过期，请重新生成"}
            ]})
        );
    }
}
