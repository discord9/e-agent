//! Small, deliberately concrete ChatGPT OAuth implementation for the Codex
//! Responses endpoint. JWTs are decoded only for expiry/account metadata; no
//! signature claim is made or required here.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use base64::Engine;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use url::Url;

pub const ISSUER: &str = "https://auth.openai.com";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const ACCOUNT_CLAIM: &str = "https://api.openai.com/auth.chatgpt_account_id";
const ACCOUNT_CLAIM_NAMESPACE: &str = "https://api.openai.com/auth";
const ACCOUNT_CLAIM_FIELD: &str = "chatgpt_account_id";
const REFRESH_WINDOW: ChronoDuration = ChronoDuration::minutes(5);
const UNKNOWN_EXPIRY_REFRESH: ChronoDuration = ChronoDuration::days(8);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Tokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AuthFile {
    tokens: Tokens,
    last_refresh: DateTime<Utc>,
}

#[derive(Clone)]
pub struct CodexAuth {
    inner: Arc<Mutex<AuthFile>>,
    path: Arc<PathBuf>,
    client: reqwest::Client,
    token_endpoint: Arc<String>,
}

impl CodexAuth {
    pub fn load() -> anyhow::Result<Self> {
        let path = auth_path()?;
        let source = std::fs::read(&path)
            .with_context(|| format!("cannot read ChatGPT auth file {}", path.display()))?;
        let data: AuthFile = serde_json::from_slice(&source)
            .with_context(|| format!("cannot decode ChatGPT auth file {}", path.display()))?;
        Self::from_file(path, data)
    }

    fn from_file(path: PathBuf, data: AuthFile) -> anyhow::Result<Self> {
        Ok(Self {
            inner: Arc::new(Mutex::new(data)),
            path: Arc::new(path),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .context("cannot create HTTP client")?,
            token_endpoint: Arc::new(format!("{ISSUER}/oauth/token")),
        })
    }

    pub async fn access_token_and_account(&self) -> anyhow::Result<(String, String)> {
        self.refresh_if_needed(false, None).await?;
        self.current_access_token_and_account().await
    }

    /// Returns the already-loaded credentials without a proactive refresh.
    /// Used only for the one permitted retry immediately after a forced 401
    /// refresh, so that retry cannot itself trigger a second refresh.
    pub async fn current_access_token_and_account(&self) -> anyhow::Result<(String, String)> {
        let state = self.inner.lock().await;
        let account = usable_account(&state.tokens)?;
        Ok((state.tokens.access_token.clone(), account))
    }

    pub async fn refresh_after_unauthorized(
        &self,
        rejected_access_token: &str,
    ) -> anyhow::Result<()> {
        self.refresh_if_needed(true, Some(rejected_access_token))
            .await
    }

    async fn refresh_if_needed(
        &self,
        force: bool,
        rejected_access_token: Option<&str>,
    ) -> anyhow::Result<()> {
        // The mutex is intentionally held over the exchange: cloned models,
        // including models on subagent runtimes, must serialize token rotation.
        let mut state = self.inner.lock().await;
        if let Some(rejected) = rejected_access_token
            && state.tokens.access_token != rejected
        {
            return Ok(());
        }
        if !force && !needs_refresh(&state) {
            return Ok(());
        }
        // Refresh boundary: re-read the auth file before submitting a refresh
        // token. `e-agent login` (or another e-agent process that rotated
        // credentials) may have replaced it on disk since we loaded it, and
        // submitting our stale refresh token would be rejected as reused.
        // Adopt newer credentials for the same account; a different account
        // is never adopted silently.
        if let Some(disk) = read_auth_file(&self.path)
            && tokens_differ(&disk.tokens, &state.tokens)
            && same_account(&disk.tokens, &state.tokens)
        {
            *state = disk;
            // Re-evaluate after adoption: the disk copy may already carry a
            // fresh access token (e.g. a completed `e-agent login`), so no
            // exchange is needed; a forced refresh whose rejected token no
            // longer matches was already recovered by another process.
            if let Some(rejected) = rejected_access_token
                && state.tokens.access_token != rejected
            {
                return Ok(());
            }
            if !force && !needs_refresh(&state) {
                return Ok(());
            }
        }
        // Exchange, with one guarded retry: when the provider reports the
        // submitted refresh token as expired/reused/revoked (or a 401),
        // another process may have rotated credentials between the reload
        // above and this exchange. Re-read the file and adopt the disk
        // version; only bail out when the disk carries the same token that
        // just failed.
        let mut retry = true;
        loop {
            let refresh_token = state.tokens.refresh_token.clone();
            let response = self
                .client
                .post(self.token_endpoint.as_str())
                .json(&serde_json::json!({
                    "client_id": CLIENT_ID,
                    "grant_type": "refresh_token",
                    "refresh_token": refresh_token,
                }))
                .send()
                .await
                .context("ChatGPT token refresh request failed")?;
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if !status.is_success() {
                if status == reqwest::StatusCode::UNAUTHORIZED || permanent_refresh_error(&body) {
                    if retry
                        && let Some(disk) = read_auth_file(&self.path)
                        && tokens_differ(&disk.tokens, &state.tokens)
                        && same_account(&disk.tokens, &state.tokens)
                    {
                        *state = disk;
                        if !needs_refresh(&state) {
                            // The disk copy already carries a fresh access
                            // token; the caller retries the request with it.
                            return Ok(());
                        }
                        retry = false;
                        continue;
                    }
                    return Err(login_expired_error(&body));
                }
                bail!("ChatGPT token refresh failed with HTTP {status}");
            }
            let returned: TokenResponse = serde_json::from_str(&body)
                .context("cannot decode ChatGPT token refresh response")?;
            if let Some(value) = returned.id_token.filter(|value| !value.is_empty()) {
                state.tokens.id_token = value;
            }
            if let Some(value) = returned.access_token.filter(|value| !value.is_empty()) {
                state.tokens.access_token = value;
            }
            if let Some(value) = returned.refresh_token.filter(|value| !value.is_empty()) {
                state.tokens.refresh_token = value;
            }
            state.last_refresh = Utc::now();
            save_file(&self.path, &state)?;
            return Ok(());
        }
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

pub fn auth_path() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("e-agent/auth.json"));
    }
    if let Some(home) = crate::home_dir() {
        return Ok(home.join(".config/e-agent/auth.json"));
    }
    bail!("cannot locate ChatGPT auth storage: set XDG_CONFIG_HOME or HOME")
}

pub fn logout() -> anyhow::Result<()> {
    logout_at(&auth_path()?)
}

fn logout_at(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("cannot remove {}", path.display())),
    }
}

/// Login is intentionally the only place that opens a listener/browser.
pub async fn login() -> anyhow::Result<CodexAuth> {
    let pkce = Pkce::new();
    let listener = bind_callback().await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{port}/auth/callback");
    let authorize = authorize_url(&redirect_uri, &pkce)?;
    if webbrowser::open(authorize.as_str()).is_err() {
        eprintln!("Open this URL in a browser to continue:\n{authorize}");
    }
    let mut callback = receive_callback(listener, &pkce.state).await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()?;
    let result = async {
        let response = client
            .post(format!("{ISSUER}/oauth/token"))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", &callback.code),
                ("redirect_uri", &redirect_uri),
                ("client_id", CLIENT_ID),
                ("code_verifier", &pkce.verifier),
            ])
            .send()
            .await
            .context("ChatGPT authorization-code exchange failed")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!(
                "ChatGPT authorization-code exchange failed with HTTP {}: {}",
                status,
                body.chars().take(500).collect::<String>()
            );
        }
        let returned: TokenResponse = response
            .json()
            .await
            .context("cannot decode ChatGPT token response")?;
        let mut tokens = Tokens {
            id_token: required_token(returned.id_token, "id_token")?,
            access_token: required_token(returned.access_token, "access_token")?,
            refresh_token: required_token(returned.refresh_token, "refresh_token")?,
            account_id: None,
        };
        tokens.account_id = usable_account(&tokens).ok();
        if tokens.account_id.is_none() {
            bail!("ChatGPT login has no usable account id; run `e-agent login` again");
        }
        let path = auth_path()?;
        let data = AuthFile {
            tokens,
            last_refresh: Utc::now(),
        };
        save_file(&path, &data)?;
        CodexAuth::from_file(path, data)
    }
    .await;
    match result {
        Ok(auth) => {
            send_callback_response(
                &mut callback.stream,
                "200 OK",
                "Login complete. You may close this window.",
            )
            .await;
            Ok(auth)
        }
        Err(error) => {
            send_callback_response(
                &mut callback.stream,
                "500 Internal Server Error",
                "Login failed. Return to e-agent for details.",
            )
            .await;
            Err(error)
        }
    }
}

fn required_token(value: Option<String>, name: &str) -> anyhow::Result<String> {
    value
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("ChatGPT token response is missing {name}"))
}

#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

impl Pkce {
    pub fn new() -> Self {
        let mut verifier = [0u8; 64];
        let mut state = [0u8; 32];
        rand::rng().fill_bytes(&mut verifier);
        rand::rng().fill_bytes(&mut state);
        let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(verifier);
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        let state = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(state);
        Self {
            verifier,
            challenge,
            state,
        }
    }
}

impl Default for Pkce {
    fn default() -> Self {
        Self::new()
    }
}

pub fn authorize_url(redirect_uri: &str, pkce: &Pkce) -> anyhow::Result<Url> {
    let mut url = Url::parse(&format!("{ISSUER}/oauth/authorize"))?;
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri),
        (
            "scope",
            "openid profile email offline_access api.connectors.read api.connectors.invoke",
        ),
        ("code_challenge", &pkce.challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", &pkce.state),
        ("originator", "codex_cli_rs"),
    ]);
    Ok(url)
}

async fn bind_callback() -> anyhow::Result<tokio::net::TcpListener> {
    for port in [1455, 1457] {
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            return Ok(listener);
        }
    }
    bail!("cannot bind ChatGPT login callback on 127.0.0.1:1455 or :1457")
}

async fn receive_callback(
    listener: tokio::net::TcpListener,
    expected_state: &str,
) -> anyhow::Result<PendingCallback> {
    use tokio::io::AsyncReadExt;
    let (mut stream, _) = listener.accept().await?;
    let result = async {
        let mut bytes = Vec::with_capacity(1024);
        loop {
            if bytes.len() >= 16 * 1024 {
                bail!("OAuth callback headers exceeded 16 KiB");
            }
            let mut chunk = [0u8; 1024];
            let count = stream.read(&mut chunk).await?;
            if count == 0 {
                bail!("OAuth callback closed before complete request headers");
            }
            bytes.extend_from_slice(&chunk[..count]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        if bytes.len() > 16 * 1024 {
            bail!("OAuth callback headers exceeded 16 KiB");
        }
        let request = std::str::from_utf8(&bytes)
            .map_err(|_| anyhow!("OAuth callback request is not valid UTF-8"))?;
        let mut parts = request.lines().next().unwrap_or("").split_whitespace();
        if parts.next() != Some("GET") {
            bail!("OAuth callback requires HTTP GET");
        }
        let target = parts
            .next()
            .ok_or_else(|| anyhow!("OAuth callback is missing request target"))?;
        callback_result(target, expected_state)
    }
    .await;
    match result {
        Ok(code) => Ok(PendingCallback { stream, code }),
        Err(error) => {
            send_callback_response(
                &mut stream,
                "400 Bad Request",
                "Login failed. Return to e-agent for details.",
            )
            .await;
            Err(error)
        }
    }
}

#[derive(Debug)]
struct PendingCallback {
    stream: tokio::net::TcpStream,
    code: String,
}

async fn send_callback_response(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    use tokio::io::AsyncWriteExt;
    let html = format!("<html><body>{body}</body></html>");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

pub fn callback_result(target: &str, expected_state: &str) -> anyhow::Result<String> {
    let url = Url::parse(&format!("http://localhost{target}"))
        .map_err(|_| anyhow!("invalid OAuth callback"))?;
    if url.path() != "/auth/callback" {
        bail!("unexpected OAuth callback path");
    }
    let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    if query.get("state").map(String::as_str) != Some(expected_state) {
        bail!("OAuth callback state did not match");
    }
    if query.contains_key("error") {
        bail!("ChatGPT login was rejected");
    }
    query
        .get("code")
        .filter(|code| !code.is_empty())
        .cloned()
        .ok_or_else(|| anyhow!("OAuth callback is missing authorization code"))
}

pub fn jwt_metadata(token: &str) -> Option<JwtMetadata> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Some(JwtMetadata {
        account_id: value
            .get(ACCOUNT_CLAIM)
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                value
                    .get(ACCOUNT_CLAIM_NAMESPACE)?
                    .get(ACCOUNT_CLAIM_FIELD)?
                    .as_str()
            })
            .filter(|v| !v.is_empty())
            .map(str::to_owned),
        exp: value
            .get("exp")
            .and_then(serde_json::Value::as_i64)
            .and_then(|v| DateTime::from_timestamp(v, 0)),
    })
}

#[derive(Debug, PartialEq)]
pub struct JwtMetadata {
    pub account_id: Option<String>,
    pub exp: Option<DateTime<Utc>>,
}

fn usable_account(tokens: &Tokens) -> anyhow::Result<String> {
    tokens
        .account_id
        .clone()
        .filter(|v| !v.is_empty())
        .or_else(|| jwt_metadata(&tokens.id_token).and_then(|m| m.account_id))
        .or_else(|| jwt_metadata(&tokens.access_token).and_then(|m| m.account_id))
        .ok_or_else(|| anyhow!("ChatGPT login has no usable account id; run `e-agent login` again"))
}

fn needs_refresh(data: &AuthFile) -> bool {
    jwt_metadata(&data.tokens.access_token)
        .and_then(|m| m.exp)
        .map(|exp| exp <= Utc::now() + REFRESH_WINDOW)
        .unwrap_or_else(|| data.last_refresh <= Utc::now() - UNKNOWN_EXPIRY_REFRESH)
}

/// Reads the auth file from disk; `None` when it is missing or cannot be
/// decoded, so callers keep their in-memory snapshot and fall back to the
/// pre-existing logic.
fn read_auth_file(path: &Path) -> Option<AuthFile> {
    let source = std::fs::read(path).ok()?;
    serde_json::from_slice(&source).ok()
}

/// True when the disk copy carries different credentials than memory (either
/// token changed), i.e. another process or a login rewrote the file.
fn tokens_differ(disk: &Tokens, memory: &Tokens) -> bool {
    disk.access_token != memory.access_token || disk.refresh_token != memory.refresh_token
}

/// True when both copies resolve to the same ChatGPT account (stored or
/// JWT-derived account id). Different or undeterminable accounts are never
/// adopted from disk, so a running process cannot silently switch accounts.
fn same_account(disk: &Tokens, memory: &Tokens) -> bool {
    match (usable_account(disk), usable_account(memory)) {
        (Ok(disk_account), Ok(memory_account)) => disk_account == memory_account,
        _ => false,
    }
}

fn permanent_refresh_kind(body: &str) -> Option<&'static str> {
    [
        "refresh_token_expired",
        "refresh_token_reused",
        "refresh_token_invalidated",
    ]
    .iter()
    .find(|needle| body.contains(**needle))
    .copied()
}

fn permanent_refresh_error(body: &str) -> bool {
    permanent_refresh_kind(body).is_some()
}

/// Classified, token-safe message for a permanent refresh failure. The raw
/// provider body is never echoed (it may contain the submitted token); only
/// the error kind is surfaced, so users can tell a true expiry or revocation
/// from a cross-process refresh-token reuse race.
fn login_expired_error(body: &str) -> anyhow::Error {
    let hint = match permanent_refresh_kind(body) {
        Some("refresh_token_expired") => "ChatGPT login has expired (refresh_token_expired)",
        Some("refresh_token_reused") => {
            "ChatGPT refresh token was reused by another process (refresh_token_reused)"
        }
        Some("refresh_token_invalidated") => {
            "ChatGPT login was revoked (refresh_token_invalidated)"
        }
        _ => "ChatGPT login has expired or was revoked",
    };
    anyhow!("{hint}; run `e-agent login`")
}

fn save_file(path: &Path, data: &AuthFile) -> anyhow::Result<()> {
    let dir = path.parent().ok_or_else(|| anyhow!("invalid auth path"))?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("cannot create auth directory {}", dir.display()))?;
    let (temporary, mut file) = create_private_temp(dir)?;
    let write_result = (|| -> anyhow::Result<()> {
        file.write_all(&serde_json::to_vec(data)?)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    let result = write_result.and_then(|()| {
        std::fs::rename(&temporary, path)
            .with_context(|| format!("cannot replace {}", path.display()))?;
        #[cfg(unix)]
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        // fsync the directory so the rename is durable — Unix semantics only:
        // on Windows CreateFile cannot open a directory handle (os error 5,
        // "access denied") and the OS journals metadata itself, so skip it.
        #[cfg(unix)]
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn create_private_temp(dir: &Path) -> anyhow::Result<(PathBuf, std::fs::File)> {
    for _ in 0..16 {
        let mut suffix = [0u8; 16];
        rand::rng().fill_bytes(&mut suffix);
        let path = dir.join(format!(
            ".auth.{}.tmp",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(suffix)
        ));
        match open_private_temp(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("cannot create private auth temporary in {}", dir.display())
                });
            }
        }
    }
    bail!("cannot create unique private auth temporary")
}

fn open_private_temp(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(test)]
impl CodexAuth {
    pub(crate) fn with_token_endpoint(mut self, endpoint: String) -> Self {
        self.token_endpoint = Arc::new(endpoint);
        self
    }

    pub(crate) fn test_auth(path: PathBuf) -> Self {
        Self::from_file(
            path,
            AuthFile {
                tokens: Tokens {
                    id_token: "id".into(),
                    access_token: "access".into(),
                    refresh_token: "refresh".into(),
                    account_id: Some("account".into()),
                },
                last_refresh: Utc::now(),
            },
        )
        .unwrap()
    }
}

#[cfg(test)]
mod tests;
