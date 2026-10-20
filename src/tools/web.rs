use super::*;

use async_trait::async_trait;
use reqwest::header::HeaderValue;
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

/// The resolved web backend: each variant owns the credential and URL(s) it
/// needs. Exactly three concrete shapes, so this is an enum rather than a
/// trait.
pub(super) enum WebBackend {
    /// Exa (the default provider): POST `{endpoint}` with the API key.
    Exa { api_key: String, endpoint: String },
    /// SearXNG: GET `{base_url}/search`; no credential.
    Searxng { base_url: String },
    /// DeepSeek: POST `{base}/v1/tools/search` and `{base}/v1/tools/fetch`,
    /// both with the same API key.
    Deepseek { api_key: String, base: String },
}

pub(super) struct WebSearch {
    backend: WebBackend,
    client: reqwest::Client,
    timeout: Duration,
}

/// `web_fetch` exists only for the DeepSeek backend (Exa/SearXNG register no
/// fetch tool), so its credential and fixed base live here directly.
pub(super) struct WebFetch {
    api_key: String,
    base: String,
    client: reqwest::Client,
    timeout: Duration,
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
    /// DeepSeek fetch with the resolved API key; the endpoint is fixed.
    pub(super) fn deepseek(api_key: String) -> Self {
        Self {
            api_key,
            base: DEEPSEEK_API_BASE.into(),
            client: web_search_client(),
            timeout: WEB_SEARCH_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(super) fn for_test(api_key: String, base: String, timeout: Duration) -> Self {
        Self {
            api_key,
            base,
            client: web_search_client(),
            timeout,
        }
    }
}

pub(super) fn web_search_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .expect("web search client configuration is valid")
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
        ToolSpec {
            name: "web_fetch".into(),
            description: "Fetch a public web page as Markdown by its URL. Never include secrets, private source code, or internal URLs.".into(),
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
        let authorization = bearer_header(&self.api_key, "web fetch")?;
        let mut response = self
            .client
            .post(deepseek_endpoint(&self.base, DEEPSEEK_FETCH_PATH))
            .timeout(self.timeout)
            .header("authorization", authorization)
            .json(&json!({"url": url}))
            .send()
            .await
            .map_err(|_| "web fetch request failed".to_string())?;
        if !response.status().is_success() {
            let error = status_error(&mut response, "web fetch").await?;
            return Err(redact_api_key(error, &self.api_key));
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
            redact_api_key(output, &self.api_key),
            OUTPUT_LIMIT,
        )))
    }
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
            let error = status_error(&mut response, "web search").await?;
            return Err(redact_api_key(error, api_key));
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
            return Err(status_error(&mut response, "web search").await?);
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
            let error = status_error(&mut response, "web search").await?;
            return Err(redact_api_key(error, api_key));
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
/// response, bounded like the response-body read. Callers add credential
/// redaction where the backend has a credential.
async fn status_error(response: &mut reqwest::Response, tool: &str) -> Result<String, String> {
    let status = response.status();
    let (body, truncated) = read_response_prefix(response, WEB_SEARCH_ERROR_PREVIEW_LIMIT)
        .await
        .map_err(|_| format!("{tool} failed with status {status}"))?;
    let mut context = truncate_utf8(
        String::from_utf8_lossy(&body).into_owned(),
        WEB_SEARCH_ERROR_PREVIEW_LIMIT,
    );
    if truncated {
        context = truncate_utf8(
            format!("{context}\n...[truncated]"),
            WEB_SEARCH_ERROR_PREVIEW_LIMIT,
        );
    }
    Ok(if context.is_empty() {
        format!("{tool} failed with status {status}")
    } else {
        format!("{tool} failed with status {status}: {context}")
    })
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
