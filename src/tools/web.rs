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

pub(super) struct WebSearch {
    /// `Some` selects the default Exa backend with this API key; `None`
    /// selects the config-driven SearXNG backend at `endpoint`.
    api_key: Option<String>,
    /// Exa context endpoint, or the SearXNG base URL.
    endpoint: String,
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
    /// matched). Unknown extra fields (`unresponsive_engines`, ...) are
    /// ignored — engine behavior differences are not special-cased.
    #[serde(default)]
    results: Vec<SearxngResult>,
}

#[derive(Deserialize)]
struct SearxngResult {
    title: Option<String>,
    url: Option<String>,
    content: Option<String>,
}

impl WebSearch {
    /// Exa (the default provider) with the resolved API key.
    pub(super) fn new(api_key: String) -> Self {
        Self {
            api_key: Some(api_key),
            endpoint: WEB_SEARCH_ENDPOINT.into(),
            client: web_search_client(),
            timeout: WEB_SEARCH_TIMEOUT,
        }
    }

    /// SearXNG with the configured base URL; requests go to
    /// `{base_url}/search` and carry no credential.
    pub(super) fn searxng(base_url: String) -> Self {
        Self {
            api_key: None,
            endpoint: base_url,
            client: web_search_client(),
            timeout: WEB_SEARCH_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(super) fn for_test(api_key: String, endpoint: String, timeout: Duration) -> Self {
        Self {
            api_key: Some(api_key),
            endpoint,
            client: web_search_client(),
            timeout,
        }
    }

    #[cfg(test)]
    pub(super) fn for_test_searxng(base_url: String, timeout: Duration) -> Self {
        Self {
            api_key: None,
            endpoint: base_url,
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
        match &self.api_key {
            Some(api_key) => self.execute_exa(api_key, query).await,
            None => self.execute_searxng(query).await,
        }
    }
}

impl WebSearch {
    /// Exa: POST the query to the context endpoint with the API key.
    async fn execute_exa(&self, api_key: &str, query: &str) -> Result<ToolOutput, String> {
        let mut api_key_header: HeaderValue = api_key
            .parse()
            .map_err(|_| "web search API key is invalid".to_string())?;
        api_key_header.set_sensitive(true);

        let mut response = self
            .client
            .post(&self.endpoint)
            .timeout(self.timeout)
            .header("x-api-key", api_key_header)
            .json(&json!({"query": query, "tokensNum": WEB_SEARCH_TOKENS}))
            .send()
            .await
            .map_err(|_| "web search request failed".to_string())?;
        if !response.status().is_success() {
            let error = status_error(&mut response).await?;
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
    async fn execute_searxng(&self, query: &str) -> Result<ToolOutput, String> {
        let endpoint = format!("{}/search", self.endpoint.trim_end_matches('/'));
        let mut response = self
            .client
            .get(&endpoint)
            .timeout(self.timeout)
            .query(&[("q", query), ("format", "json")])
            .send()
            .await
            .map_err(|_| "web search request failed".to_string())?;
        if !response.status().is_success() {
            return Err(status_error(&mut response).await?);
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
            "no results".to_owned()
        } else {
            entries.join("\n\n")
        };
        Ok(ToolOutput::text(truncate_utf8(output, OUTPUT_LIMIT)))
    }
}

/// The shared `web search failed with status ...` error for a non-success
/// response, bounded like the response-body read. Callers add credential
/// redaction where the backend has a credential.
async fn status_error(response: &mut reqwest::Response) -> Result<String, String> {
    let status = response.status();
    let (body, truncated) = read_response_prefix(response, WEB_SEARCH_ERROR_PREVIEW_LIMIT)
        .await
        .map_err(|_| format!("web search failed with status {status}"))?;
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
        format!("web search failed with status {status}")
    } else {
        format!("web search failed with status {status}: {context}")
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
    text.replace(api_key, "[redacted]")
}
