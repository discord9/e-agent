use super::*;

use async_trait::async_trait;
use dom_smoothie::{Article, Config as ReadabilityConfig, Readability, TextMode};
use encoding_rs::Encoding;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;

pub(super) const WEB_SEARCH_ENDPOINT: &str = "https://api.exa.ai/context";
pub(super) const WEB_SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const WEB_SEARCH_QUERY_LIMIT: usize = 2000;
pub(super) const WEB_SEARCH_TOKENS: u16 = 5000;
pub(super) const WEB_SEARCH_ERROR_PREVIEW_LIMIT: usize = 8 * 1024;
pub(super) const WEB_SEARCH_RESPONSE_LIMIT: usize = 128 * 1024;

/// DeepSeek Search/Fetch: one fixed API base, and the two paths below are
/// fixed too (`[web_search] base_url` is a config error for this provider,
/// so `{base}` is only overridden by tests).
pub(super) const DEEPSEEK_API_BASE: &str = "https://api.deepseek.com";
pub(super) const DEEPSEEK_SEARCH_PATH: &str = "/v1/tools/search";
pub(super) const DEEPSEEK_FETCH_PATH: &str = "/v1/tools/fetch";
/// Fixed search width and `need_content = false`: a single summary is ~2.5KB
/// and a page body ~10KB, so neither is negotiable by the model.
pub(super) const DEEPSEEK_SEARCH_COUNT: u8 = 5;
pub(super) const WEB_FETCH_URL_LIMIT: usize = 8192;
/// The local fetcher's timeout: the same 30s its DeepSeek predecessor used.
pub(super) const WEB_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard cap on a fetched body: a page past it is an error, not a truncated
/// read, so the model never sees a silently clipped document.
pub(super) const WEB_FETCH_BODY_LIMIT: usize = 2 * 1024 * 1024;
/// Redirect hops `web_fetch` follows before failing.
pub(super) const WEB_FETCH_REDIRECT_LIMIT: usize = 5;
/// The local fetcher's `User-Agent`. `reqwest` sends none by default, and a
/// UA-less client is answered with 403 by Wikipedia and much of the web, so
/// this is a fixed, ordinary desktop-browser string — honest about being a
/// generic browser, and carrying no version, contact or credential.
pub(super) const WEB_FETCH_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
/// What the local fetcher says it accepts: the shape a browser sends, so a
/// server that negotiates on `Accept` returns the HTML this tool parses
/// instead of JSON or an image.
pub(super) const WEB_FETCH_ACCEPT: &str =
    "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.8";
/// How much of a failed request's reason reaches the model. `reqwest` repeats
/// the URL in its `Display`, and the URL alone may be `WEB_FETCH_URL_LIMIT`
/// bytes.
pub(super) const WEB_FETCH_ERROR_REASON_LIMIT: usize = 512;

/// The resolved web backend: each variant owns the credential and URL(s) it
/// needs. Exactly three concrete shapes, so this is an enum rather than a
/// trait.
pub(super) enum WebBackend {
    /// Exa (the default provider): POST `{endpoint}` with the API key.
    Exa { api_key: String, endpoint: String },
    /// SearXNG: GET `{base_url}/search`; no credential.
    Searxng { base_url: String },
    /// DeepSeek: POST `{base}/v1/tools/search` and `{base}/v1/tools/fetch`,
    /// both with the same API key; the fetch endpoint is only used by
    /// `WebFetch::Deepseek` (see `[web_search] fetch_provider`).
    Deepseek { api_key: String, base: String },
}

pub(super) struct WebSearch {
    backend: WebBackend,
    client: reqwest::Client,
    timeout: Duration,
}

/// `web_fetch`: one URL, fetched either by THIS process (the default) or by
/// DeepSeek's remote fetch service, chosen by `[web_search] fetch_provider`.
///
/// The local variant rides the process's own egress, so it reaches whatever
/// the process reaches (the same network as `bash`) and is registered for
/// every search backend. Neither variant follows links found in the page, so
/// this is not a crawler.
pub(super) enum WebFetch {
    /// GET the URL here; no credential, no fixed endpoint.
    Local {
        client: reqwest::Client,
        timeout: Duration,
    },
    /// POST the URL to `{base}/v1/tools/fetch` with the API key.
    Deepseek {
        api_key: String,
        base: String,
        client: reqwest::Client,
        timeout: Duration,
    },
}

#[derive(Deserialize)]
pub(super) struct ExaContextResponse {
    response: String,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct SearxngResponse {
    /// Always present in a SearXNG JSON response (empty when nothing
    /// matched).
    #[serde(default)]
    results: Vec<SearxngResult>,
    /// `[[engine, reason], ...]` for engines that failed during this query.
    /// Only surfaced when `results` is empty: a partial engine failure
    /// alongside a non-empty result set is normal and is not special-cased.
    #[serde(default)]
    unresponsive_engines: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct SearxngResult {
    title: Option<String>,
    url: Option<String>,
    content: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct DeepseekSearchResponse {
    #[serde(default)]
    results: Vec<DeepseekSearchResult>,
}

/// One DeepSeek hit. `content` (only sent with `need_content = true`, which
/// this tool never sends), `site_name`, `authority` and `rerank_score` are
/// deliberately not deserialized.
#[derive(Deserialize)]
struct DeepseekSearchResult {
    title: Option<String>,
    url: Option<String>,
    summary: Option<String>,
    date: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct DeepseekFetchResponse {
    title: String,
    /// Markdown.
    content: String,
}

impl WebSearch {
    /// Exa (the default provider) with the resolved API key.
    pub(super) fn new(api_key: String) -> Self {
        Self::with_backend(WebBackend::Exa {
            api_key,
            endpoint: WEB_SEARCH_ENDPOINT.into(),
        })
    }

    /// SearXNG with the configured base URL; requests go to
    /// `{base_url}/search` and carry no credential.
    pub(super) fn searxng(base_url: String) -> Self {
        Self::with_backend(WebBackend::Searxng { base_url })
    }

    /// DeepSeek with the resolved API key; the endpoints are fixed.
    pub(super) fn deepseek(api_key: String) -> Self {
        Self::with_backend(WebBackend::Deepseek {
            api_key,
            base: DEEPSEEK_API_BASE.into(),
        })
    }

    fn with_backend(backend: WebBackend) -> Self {
        Self {
            backend,
            client: web_search_client(),
            timeout: WEB_SEARCH_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(super) fn for_test(api_key: String, endpoint: String, timeout: Duration) -> Self {
        Self {
            backend: WebBackend::Exa { api_key, endpoint },
            client: web_search_client(),
            timeout,
        }
    }

    #[cfg(test)]
    pub(super) fn for_test_searxng(base_url: String, timeout: Duration) -> Self {
        Self {
            backend: WebBackend::Searxng { base_url },
            client: web_search_client(),
            timeout,
        }
    }

    #[cfg(test)]
    pub(super) fn for_test_deepseek(api_key: String, base: String, timeout: Duration) -> Self {
        Self {
            backend: WebBackend::Deepseek { api_key, base },
            client: web_search_client(),
            timeout,
        }
    }
}

impl WebFetch {
    /// The local fetcher: no credential, no fixed endpoint (the URL is the
    /// argument), and a redirect-following client of its own.
    pub(super) fn local() -> Self {
        Self::Local {
            client: web_fetch_client(),
            timeout: WEB_FETCH_TIMEOUT,
        }
    }

    /// DeepSeek fetch with the resolved API key; the endpoint is fixed.
    pub(super) fn deepseek(api_key: String) -> Self {
        Self::Deepseek {
            api_key,
            base: DEEPSEEK_API_BASE.into(),
            client: web_search_client(),
            timeout: WEB_SEARCH_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(super) fn for_test(api_key: String, base: String, timeout: Duration) -> Self {
        Self::Deepseek {
            api_key,
            base,
            client: web_search_client(),
            timeout,
        }
    }
}

/// The `web_search` client: no redirects, no retries — every backend gets a
/// single request whose response is either parsed or reported.
pub(super) fn web_search_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .expect("web search client configuration is valid")
}

/// The `web_fetch` client: pages redirect (canonical URLs, short links), so
/// this one follows up to 5 hops and still never retries. A 6-hop chain is
/// an error rather than an unbounded walk.
///
/// It is also the only client that sends `User-Agent` and `Accept`: the
/// header-less requests of `web_search_client` are API calls whose servers do
/// not care, while a fetched page's server usually does (see
/// `WEB_FETCH_USER_AGENT`). Both are client defaults, so every redirect hop
/// carries them, and no credential is ever added here.
pub(super) fn web_fetch_client() -> reqwest::Client {
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::ACCEPT,
        HeaderValue::from_static(WEB_FETCH_ACCEPT),
    );
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(WEB_FETCH_REDIRECT_LIMIT))
        .retry(reqwest::retry::never())
        .user_agent(WEB_FETCH_USER_AGENT)
        .default_headers(headers)
        .build()
        .expect("web fetch client configuration is valid")
}

/// `{prefix}: {reason}` for a failed request or body read. `reqwest`'s own
/// `Display` is only the error kind plus the URL (`error sending request for
/// url (...)`), so the cause chain is appended too — a refused connection, a
/// DNS failure, a TLS rejection and a timeout are otherwise one message, and
/// the reason is what tells them apart. Bounded and flattened to one line so
/// the model gets a single readable line.
fn transport_error(prefix: &str, error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    let text = truncate_utf8(text, WEB_FETCH_ERROR_REASON_LIMIT);
    format!(
        "{prefix}: {}",
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    )
}

/// `{base}{path}` with any trailing slash on the base removed, so a test base
/// of `http://127.0.0.1:1234` and the constant `https://api.deepseek.com`
/// compose identically.
fn deepseek_endpoint(base: &str, path: &str) -> String {
    format!("{}{path}", base.trim_end_matches('/'))
}

/// `Authorization: Bearer ...` for the DeepSeek tools endpoints, rejected
/// (and never echoed) when the key cannot be a header value. `tool` names the
/// failing tool in the model-facing error.
fn bearer_header(api_key: &str, tool: &str) -> Result<HeaderValue, String> {
    let mut header: HeaderValue = format!("Bearer {api_key}")
        .parse()
        .map_err(|_| format!("{tool} API key is invalid"))?;
    header.set_sensitive(true);
    Ok(header)
}

#[async_trait]
impl Tool for WebSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".into(),
            description: "Search public web documentation and code examples. Never include secrets, private source code, internal URLs, or personal data in the query.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "A specific public-web research query."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, String> {
        let query = required_string(&arguments, "query")?.trim();
        if query.is_empty() {
            return Err("`query` must not be empty".into());
        }
        if query.chars().count() > WEB_SEARCH_QUERY_LIMIT {
            return Err(format!(
                "`query` must be at most {WEB_SEARCH_QUERY_LIMIT} characters"
            ));
        }
        match &self.backend {
            WebBackend::Exa { api_key, endpoint } => {
                self.execute_exa(api_key, endpoint, query).await
            }
            WebBackend::Searxng { base_url } => self.execute_searxng(base_url, query).await,
            WebBackend::Deepseek { api_key, base } => {
                self.execute_deepseek(api_key, base, query).await
            }
        }
    }
}

#[async_trait]
impl Tool for WebFetch {
    fn spec(&self) -> ToolSpec {
        // The local fetcher is the one whose reach needs stating: it uses
        // this process's egress. The DeepSeek variant fetches server-side.
        let description = match self {
            Self::Local { .. } => {
                "Fetch one URL as text. This process performs the request over its own network, so it reaches whatever the process can reach (the same reach as `bash`), not only public hosts. HTML is reduced to its readable article as Markdown, and a page with no readable article is an error. It returns that single page and never follows links found in it. Never include secrets, private source code, or internal URLs in the URL."
            }
            Self::Deepseek { .. } => {
                "Fetch a public web page as Markdown by its URL. Never include secrets, private source code, or internal URLs."
            }
        };
        ToolSpec {
            name: "web_fetch".into(),
            description: description.into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "An absolute http or https URL."
                    }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, String> {
        let url = required_string(&arguments, "url")?.trim();
        if url.is_empty() {
            return Err("`url` must not be empty".into());
        }
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("`url` must start with http:// or https://".into());
        }
        if url.chars().count() > WEB_FETCH_URL_LIMIT {
            return Err(format!(
                "`url` must be at most {WEB_FETCH_URL_LIMIT} characters"
            ));
        }
        match self {
            Self::Local { client, timeout } => execute_local_fetch(client, *timeout, url).await,
            Self::Deepseek {
                api_key,
                base,
                client,
                timeout,
            } => execute_deepseek_fetch(client, *timeout, api_key, base, url).await,
        }
    }
}

/// The local fetch: GET the URL with this process's egress, then turn the
/// body into text (`text/plain` and `text/markdown` verbatim, HTML through
/// the readability extractor).
async fn execute_local_fetch(
    client: &reqwest::Client,
    timeout: Duration,
    url: &str,
) -> Result<ToolOutput, String> {
    let mut response = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .map_err(|error| transport_error("web fetch request failed", &error))?;
    if !response.status().is_success() {
        // No credential: this fetch carries none, so nothing to redact.
        return Err(status_error(&mut response, "web fetch", "").await?);
    }

    // The media type decides how the body is read, so it is checked before
    // the read: an unsupported type fails fast and a binary body never
    // reaches the model.
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let html = match media_type.as_str() {
        "" => return Err("web fetch response has no content type".into()),
        "text/html" | "application/xhtml+xml" => true,
        "text/plain" | "text/markdown" => false,
        other => {
            return Err(format!(
                "web fetch does not support content type `{other}` (accepted: text/html, application/xhtml+xml, text/plain, text/markdown)"
            ));
        }
    };

    let (body, truncated) = read_response_prefix(&mut response, WEB_FETCH_BODY_LIMIT)
        .await
        .map_err(|error| transport_error("web fetch response body failed", &error))?;
    if truncated {
        return Err(format!(
            "web fetch response body exceeds {WEB_FETCH_BODY_LIMIT} bytes"
        ));
    }
    let text = decode_body(&body, &content_type);
    let (title, text) = if html {
        let article = extract_article(&text, url)?;
        (Some(article.title), article.text_content.to_string())
    } else {
        (None, text.trim().to_owned())
    };
    let output = [title.as_deref().unwrap_or_default(), text.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let output = if output.is_empty() {
        "no content".to_owned()
    } else {
        output
    };
    Ok(ToolOutput::text(truncate_utf8(output, OUTPUT_LIMIT)))
}

/// The DeepSeek fetch: POST the URL to `{base}/v1/tools/fetch` with the API
/// key and return the server's title + Markdown body. Unchanged from the
/// tool's original single-backend implementation.
async fn execute_deepseek_fetch(
    client: &reqwest::Client,
    timeout: Duration,
    api_key: &str,
    base: &str,
    url: &str,
) -> Result<ToolOutput, String> {
    let authorization = bearer_header(api_key, "web fetch")?;
    let mut response = client
        .post(deepseek_endpoint(base, DEEPSEEK_FETCH_PATH))
        .timeout(timeout)
        .header("authorization", authorization)
        .json(&json!({"url": url}))
        .send()
        .await
        .map_err(|_| "web fetch request failed".to_string())?;
    if !response.status().is_success() {
        return Err(status_error(&mut response, "web fetch", api_key).await?);
    }

    let (body, truncated) = read_response_prefix(&mut response, WEB_SEARCH_RESPONSE_LIMIT)
        .await
        .map_err(|_| "web fetch response body failed".to_string())?;
    if truncated {
        return Err(format!(
            "web fetch response body exceeds {WEB_SEARCH_RESPONSE_LIMIT} bytes"
        ));
    }
    let fetched: DeepseekFetchResponse = serde_json::from_slice(&body)
        .map_err(|_| "web fetch returned malformed JSON or no response".to_string())?;
    let output = [fetched.title.trim(), fetched.content.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let output = if output.is_empty() {
        "no content".to_owned()
    } else {
        output
    };
    Ok(ToolOutput::text(truncate_utf8(
        redact_api_key(output, api_key),
        OUTPUT_LIMIT,
    )))
}

impl WebSearch {
    /// Exa: POST the query to the context endpoint with the API key.
    async fn execute_exa(
        &self,
        api_key: &str,
        endpoint: &str,
        query: &str,
    ) -> Result<ToolOutput, String> {
        let mut api_key_header: HeaderValue = api_key
            .parse()
            .map_err(|_| "web search API key is invalid".to_string())?;
        api_key_header.set_sensitive(true);

        let mut response = self
            .client
            .post(endpoint)
            .timeout(self.timeout)
            .header("x-api-key", api_key_header)
            .json(&json!({"query": query, "tokensNum": WEB_SEARCH_TOKENS}))
            .send()
            .await
            .map_err(|_| "web search request failed".to_string())?;
        if !response.status().is_success() {
            return Err(status_error(&mut response, "web search", api_key).await?);
        }

        let (body, truncated) = read_response_prefix(&mut response, WEB_SEARCH_RESPONSE_LIMIT)
            .await
            .map_err(|_| "web search response body failed".to_string())?;
        if truncated {
            return Err(format!(
                "web search response body exceeds {WEB_SEARCH_RESPONSE_LIMIT} bytes"
            ));
        }
        let context: ExaContextResponse = serde_json::from_slice(&body)
            .map_err(|_| "web search returned malformed JSON or no response".to_string())?;
        let _ = context.request_id;
        Ok(ToolOutput::text(truncate_utf8(
            redact_api_key(context.response, api_key),
            OUTPUT_LIMIT,
        )))
    }

    /// SearXNG: GET `{base_url}/search?q=&format=json`; no credential, and
    /// the `results` array (title/url/content) is flattened to plain text.
    async fn execute_searxng(&self, base_url: &str, query: &str) -> Result<ToolOutput, String> {
        let endpoint = format!("{}/search", base_url.trim_end_matches('/'));
        let mut response = self
            .client
            .get(&endpoint)
            .timeout(self.timeout)
            .query(&[("q", query), ("format", "json")])
            .send()
            .await
            .map_err(|_| "web search request failed".to_string())?;
        if !response.status().is_success() {
            return Err(status_error(&mut response, "web search", "").await?);
        }

        let (body, truncated) = read_response_prefix(&mut response, WEB_SEARCH_RESPONSE_LIMIT)
            .await
            .map_err(|_| "web search response body failed".to_string())?;
        if truncated {
            return Err(format!(
                "web search response body exceeds {WEB_SEARCH_RESPONSE_LIMIT} bytes"
            ));
        }
        let response: SearxngResponse = serde_json::from_slice(&body)
            .map_err(|_| "web search returned malformed JSON or no response".to_string())?;
        let mut entries = Vec::new();
        for result in response.results {
            let lines: Vec<String> = [result.title, result.url, result.content]
                .into_iter()
                .flatten()
                .map(|field| field.trim().to_owned())
                .filter(|field| !field.is_empty())
                .collect();
            if !lines.is_empty() {
                entries.push(lines.join("\n"));
            }
        }
        let output = if entries.is_empty() {
            if response.unresponsive_engines.is_empty() {
                "no results".to_owned()
            } else {
                let failures = response
                    .unresponsive_engines
                    .iter()
                    .map(|(engine, reason)| format!("{engine} ({reason})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "no results; {} engine(s) failed: {failures} — the search backend is degraded, so this is not evidence the topic does not exist",
                    response.unresponsive_engines.len()
                )
            }
        } else {
            entries.join("\n\n")
        };
        Ok(ToolOutput::text(truncate_utf8(output, OUTPUT_LIMIT)))
    }

    /// DeepSeek: POST the query to `{base}/v1/tools/search` with
    /// `count = 5` and `need_content = false`, then flatten title/url/summary
    /// (/date) per hit to plain text. No JSON, no raw response, no content.
    async fn execute_deepseek(
        &self,
        api_key: &str,
        base: &str,
        query: &str,
    ) -> Result<ToolOutput, String> {
        let authorization = bearer_header(api_key, "web search")?;
        let mut response = self
            .client
            .post(deepseek_endpoint(base, DEEPSEEK_SEARCH_PATH))
            .timeout(self.timeout)
            .header("authorization", authorization)
            .json(&json!({
                "query": query,
                "count": DEEPSEEK_SEARCH_COUNT,
                "need_content": false,
            }))
            .send()
            .await
            .map_err(|_| "web search request failed".to_string())?;
        if !response.status().is_success() {
            return Err(status_error(&mut response, "web search", api_key).await?);
        }

        let (body, truncated) = read_response_prefix(&mut response, WEB_SEARCH_RESPONSE_LIMIT)
            .await
            .map_err(|_| "web search response body failed".to_string())?;
        if truncated {
            return Err(format!(
                "web search response body exceeds {WEB_SEARCH_RESPONSE_LIMIT} bytes"
            ));
        }
        let response: DeepseekSearchResponse = serde_json::from_slice(&body)
            .map_err(|_| "web search returned malformed JSON or no response".to_string())?;
        let output = project_deepseek_results(response.results);
        Ok(ToolOutput::text(truncate_utf8(
            redact_api_key(output, api_key),
            OUTPUT_LIMIT,
        )))
    }
}

/// One text block per hit (title, URL, summary, optional date), separated by
/// a blank line — the same shape as the SearXNG projection.
fn project_deepseek_results(results: Vec<DeepseekSearchResult>) -> String {
    let mut entries = Vec::new();
    for result in results {
        let lines: Vec<String> = [result.title, result.url, result.summary, result.date]
            .into_iter()
            .flatten()
            .map(|field| field.trim().to_owned())
            .filter(|field| !field.is_empty())
            .collect();
        if !lines.is_empty() {
            entries.push(lines.join("\n"));
        }
    }
    if entries.is_empty() {
        "no results".to_owned()
    } else {
        entries.join("\n\n")
    }
}

/// The shared `{tool} failed with status ...` error for a non-success
/// response, bounded like the response-body read. The backend's credential is
/// redacted here, before truncation, so a key that straddles the preview limit
/// cannot leave a prefix behind. `api_key` is `""` for a backend with no
/// credential (redaction is then a no-op).
async fn status_error(
    response: &mut reqwest::Response,
    tool: &str,
    api_key: &str,
) -> Result<String, String> {
    let status = response.status();
    // Read one key's length past the preview limit: a credential cut by that
    // limit must still be complete in the text redaction sees.
    let (body, truncated) =
        read_response_prefix(response, WEB_SEARCH_ERROR_PREVIEW_LIMIT + api_key.len())
            .await
            .map_err(|_| format!("{tool} failed with status {status}"))?;
    Ok(bounded_error_context(
        tool, status, &body, truncated, api_key,
    ))
}

/// Redact first, then truncate: truncation can cut a credential in half, and a
/// leftover prefix is just as much a leak as the whole key.
fn bounded_error_context(
    tool: &str,
    status: reqwest::StatusCode,
    body: &[u8],
    truncated: bool,
    api_key: &str,
) -> String {
    let mut context = truncate_utf8(
        redact_api_key(String::from_utf8_lossy(body).into_owned(), api_key),
        WEB_SEARCH_ERROR_PREVIEW_LIMIT,
    );
    if truncated {
        context = truncate_utf8(
            format!("{context}\n...[truncated]"),
            WEB_SEARCH_ERROR_PREVIEW_LIMIT,
        );
    }
    if context.is_empty() {
        format!("{tool} failed with status {status}")
    } else {
        format!("{tool} failed with status {status}: {context}")
    }
}

pub(super) async fn read_response_prefix(
    response: &mut reqwest::Response,
    limit: usize,
) -> Result<(Vec<u8>, bool), reqwest::Error> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let room = limit.saturating_sub(bytes.len());
        if chunk.len() > room {
            bytes.extend_from_slice(&chunk[..room]);
            return Ok((bytes, true));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, false))
}

pub(super) fn truncate_utf8(mut text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let marker = "\n...[truncated]";
    let mut end = limit.saturating_sub(marker.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    if limit >= marker.len() {
        text.push_str(marker);
    }
    text
}

pub(super) fn redact_api_key(text: String, api_key: &str) -> String {
    if api_key.is_empty() {
        return text;
    }
    text.replace(api_key, "[redacted]")
}

/// Decode a fetched body: the `charset` parameter of `content_type` wins,
/// else a `<meta>` charset sniffed from the first KiB, else UTF-8. Decoding
/// is always lossy — an invalid sequence becomes U+FFFD, never an error.
pub(super) fn decode_body(body: &[u8], content_type: &str) -> String {
    content_type_charset(content_type)
        .or_else(|| sniff_charset(body))
        .and_then(|label| Encoding::for_label(label.as_bytes()))
        .map(|encoding| encoding.decode(body).0.into_owned())
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned())
}

/// The `charset` parameter of a `Content-Type` value, quotes removed.
fn content_type_charset(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches(['"', '\'']).trim().to_owned())
            .filter(|charset| !charset.is_empty())
    })
}

/// The charset of `<meta charset=...>` or `<meta http-equiv="content-type">`
/// in the first KiB, which is all a browser looks at for the same job.
fn sniff_charset(body: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(&body[..body.len().min(1024)]);
    let mut search = 0;
    while let Some(at) = find_ci(&head, "charset", search) {
        let after = head[at + "charset".len()..].trim_start();
        if let Some(after) = after.strip_prefix('=') {
            let charset: String = after
                .trim_start()
                .trim_start_matches(['"', '\''])
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
                .collect();
            if !charset.is_empty() {
                return Some(charset);
            }
        }
        search = at + "charset".len();
    }
    None
}

/// The readable article of a decoded HTML document (Mozilla Readability via
/// `dom_smoothie`): its title and Markdown text. `url` is the document base,
/// so relative links resolve in the Markdown output. A document without a
/// readable article is an error naming the failure — this tool does not fall
/// back to dumping tags and navigation text into the model's context.
pub(super) fn extract_article(html: &str, url: &str) -> Result<Article, String> {
    let config = ReadabilityConfig {
        text_mode: TextMode::Markdown,
        ..ReadabilityConfig::default()
    };
    let mut readability = Readability::new(html, Some(url), Some(config))
        .map_err(|error| format!("could not extract readable content: {error}"))?;
    readability
        .parse()
        .map_err(|error| format!("could not extract readable content: {error}"))
}

/// Case-insensitive ASCII substring search from byte offset `from`. Every
/// match starts on an ASCII byte, so the returned offset is a char boundary.
fn find_ci(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let (haystack, needle) = (haystack.as_bytes(), needle.as_bytes());
    if needle.is_empty() || from >= haystack.len() || needle.len() > haystack.len() - from {
        return None;
    }
    (from..=haystack.len() - needle.len())
        .find(|&start| haystack[start..start + needle.len()].eq_ignore_ascii_case(needle))
}
