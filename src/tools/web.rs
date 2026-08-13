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
    api_key: String,
    client: reqwest::Client,
    endpoint: String,
    timeout: Duration,
}

#[derive(Deserialize)]
pub(super) struct ExaContextResponse {
    response: String,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
}

impl WebSearch {
    pub(super) fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: web_search_client(),
            endpoint: WEB_SEARCH_ENDPOINT.into(),
            timeout: WEB_SEARCH_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(super) fn for_test(api_key: String, endpoint: String, timeout: Duration) -> Self {
        Self {
            api_key,
            client: web_search_client(),
            endpoint,
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
        let mut api_key: HeaderValue = self
            .api_key
            .parse()
            .map_err(|_| "web search API key is invalid".to_string())?;
        api_key.set_sensitive(true);

        let mut response = self
            .client
            .post(&self.endpoint)
            .timeout(self.timeout)
            .header("x-api-key", api_key)
            .json(&json!({"query": query, "tokensNum": WEB_SEARCH_TOKENS}))
            .send()
            .await
            .map_err(|_| "web search request failed".to_string())?;
        if !response.status().is_success() {
            let status = response.status();
            let (body, truncated) =
                read_response_prefix(&mut response, WEB_SEARCH_ERROR_PREVIEW_LIMIT)
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
            let error = if context.is_empty() {
                format!("web search failed with status {status}")
            } else {
                format!("web search failed with status {status}: {context}")
            };
            return Err(redact_api_key(error, &self.api_key));
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
            redact_api_key(context.response, &self.api_key),
            OUTPUT_LIMIT,
        )))
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
    text.replace(api_key, "[redacted]")
}
