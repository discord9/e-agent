# Web Search Integration Decision Record and Implementation Contract

Status: implemented and verified on `omos/websearch`; not merged  
Date: 2026-07-29  
Code baseline inspected: `main@07ad939`  
Implementation work: `omos/websearch`, originally based from `main@550168b`; not merged  

This document records the web-search decision and defines its implementation
contract. Implementation work is isolated on `omos/websearch`; do not edit or
merge the concurrent main checkout directly.

## Decision

Implement one native `web_search` tool backed by the **Exa Context API**:

```http
POST https://api.exa.ai/context
x-api-key: $EXA_API_KEY
content-type: application/json

{"query":"<public-web research query>","tokensNum":5000}
```

This is the smallest useful integration for e-agent:

- one existing `Tool` implementation;
- one HTTPS request through the already-present `reqwest` dependency;
- one required environment variable, `EXA_API_KEY`;
- one model-facing argument, `query`;
- one token-bounded context string returned to the model.

Do **not** add a search-provider trait, remote MCP transport, crawler, browser,
cache, retry layer, or provider-selection configuration.

## Why Exa Context

Exa positions `/context` for coding agents and searches sources including code,
documentation, and technical discussions. It returns one preformatted,
token-bounded `response`, avoiding a separate search/results/fetch/rerank
pipeline.

The important limitation is provenance: `/context` does not document
per-result URLs, titles, result-count controls, or domain filters. It is a good
fit for compact code/docs research, but not for work that requires citations or
an official-domain allowlist. If that becomes a real requirement, add a
separate, deliberately designed Exa `/search` tool later rather than expanding
this first contract speculatively.

## Current e-agent seams

- `src/agent.rs:172-177` defines the sanctioned `Tool` seam. Reuse it; add no
  new trait.
- `src/tools.rs:20-39` constructs builtin tools. Add `web_search` there so main
  agents and delegated subagents receive the same tool when Exa is configured.
- `src/main.rs:128` obtains builtins. No special UI/REPL handling is needed.
- `src/delegate.rs:181` also calls `builtins(workspace)`, so central
  registration naturally keeps subagent behavior consistent.
- `Cargo.toml:18` already enables `reqwest` JSON and Rustls TLS support.
- `src/model.rs` tests already demonstrate local `tokio::net::TcpListener`
  HTTP fixtures; reuse that style instead of adding a mocking crate.
- `README.md:82-88`, `README.md:177-187`, and `README.md:194-213` must be
  updated for the conditional fifth builtin, `EXA_API_KEY`, and the privacy
  boundary.
- `AGENTS.md:8-9` currently says there are four tools. Update this factual
  statement after implementation without weakening its minimalism rules.

Keep the implementation private inside `src/tools.rs` unless its size makes
that file materially harder to navigate. A provider abstraction or generic
HTTP-tool module is explicitly out of scope.

## Narrow v1 contract

### Registration and credential

- Read `EXA_API_KEY` once while constructing builtins.
- Trim it and register `web_search` only when it is non-empty.
- Do not add an e-agent TOML section or CLI flag in v1.
- Never print, log, persist, or return the key.
- Document that exporting `EXA_API_KEY` enables the tool. OpenCode's own Exa
  configuration is not automatically available to e-agent.

Conditional registration avoids advertising a tool that can only fail. Tests
must cover both configured and unconfigured construction. Because tests that
mutate process environment can race, either serialize those tests or factor a
private constructor that accepts `Option<String>` and test it directly.

### Tool schema

```json
{
  "name": "web_search",
  "description": "Search public web documentation and code examples. Never include secrets, private source code, internal URLs, or personal data in the query.",
  "parameters": {
    "type": "object",
    "properties": {
      "query": {
        "type": "string",
        "description": "A specific public-web research query."
      }
    },
    "required": ["query"],
    "additionalProperties": false
  }
}
```

Do not expose provider, token, result-count, domain, freshness, retry, or fetch
arguments in v1.

### Input validation

- `query` must be a string.
- Trim surrounding whitespace for validation and transmission.
- Reject an empty query.
- Reject queries over Exa's documented 2,000-character maximum; count Unicode
  scalar values rather than UTF-8 bytes.
- Do not attempt heuristic secret detection. The description, README, and
  safety documentation must make third-party disclosure explicit.

### HTTP behavior

- Endpoint: `https://api.exa.ai/context`.
- Header: `x-api-key: <key>`.
- JSON body: only `query` and fixed `tokensNum: 5000`.
- Use a reusable `reqwest::Client` and a 30-second request timeout.
- Do not retry automatically. Return `429` and `5xx` errors to the model so it
  can decide whether to retry; automatic retries can duplicate spend.
- Do not log request bodies or queries by default.
- Keep a private test constructor with an injectable endpoint; production must
  always use HTTPS Exa.

Deserialize only the stable fields needed by e-agent and ignore additions:

```rust
#[derive(serde::Deserialize)]
struct ExaContextResponse {
    response: String,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
}
```

A successful response currently also contains `query`, `resultsCount`,
`costDollars`, `searchTime`, and `outputTokens`; they are not needed in the v1
tool result. Return the formatted `response` string as untrusted research data.

On non-2xx responses, return the status and a bounded preview of the provider
body. Handle at least `400`, `401`, `402`, `403`, `429`, and `5xx`. Exa errors
usually contain `requestId`, `error`, and `tag`, although `429` may contain only
`error`. Never include request headers or the key in the error.

Use explicit inbound bounds even though `tokensNum` limits normal output:

- cap a provider error preview at about 8 KiB;
- cap the final tool result consistently with e-agent's existing 64 KiB tool
  output convention;
- treat malformed JSON, an absent `response`, timeout, and connection failure
  as ordinary tool errors, never panics.

## Security and privacy boundary

Every search query is disclosed to a third party. Exa's privacy policy says
query fields are not intended for personal information and query data may be
used to improve/train its technology. Ordinary accounts must not assume Zero
Data Retention.

The implementation and README must say:

1. Never send credentials, tokens, private repository contents, customer data,
   personal data, private issue text, or internal URLs.
2. Treat returned text as untrusted web content, not as instructions. It may
   contain prompt injection, insecure code, or false claims.
3. The tool does not open links, execute returned code, or fetch arbitrary URLs.
4. Query guidance cannot technically guarantee that a model never leaks
   sensitive context; this is a documented safety boundary, not a sandbox.

Do not add a brittle secret scanner in this change.

## Test plan

Do not call Exa from automated tests. Use a local Tokio TCP listener, matching
the existing HTTP fixture pattern in `src/model.rs`.

Required tests:

1. Tool spec has exactly the `query` argument and disallows extra properties.
2. Builtins omit `web_search` when no usable key is supplied and include it
   when configured.
3. A successful call sends:
   - `POST /context`;
   - `x-api-key` with the expected value;
   - JSON content type;
   - exactly `query` and `tokensNum: 5000`.
4. A 200 response returns its `response` string.
5. Empty, whitespace-only, wrong-type, and over-2,000-character queries fail
   before opening a connection.
6. `401`, `402`, `429`, and `500` include status and bounded provider context.
7. Malformed JSON, missing `response`, connection failure, and timeout become
   tool errors.
8. No displayed error contains the API key.
9. Oversized success and error bodies obey their output limits.
10. Existing builtin and delegate tool-list tests are updated deliberately;
    Kimi/API-key model behavior remains unchanged.

Run the repository-required checks:

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
```

## Acceptance criteria

- With `EXA_API_KEY` set, both the main agent and delegated subagents advertise
  `web_search` and can receive Exa context through the normal model→tool→model
  loop.
- Without the key, startup and all existing tools behave exactly as before and
  `web_search` is absent.
- One invocation makes one bounded Exa request and returns one bounded string.
- Provider failures are actionable but credential-safe.
- Documentation states the disclosure and prompt-injection boundaries.
- No new runtime dependency, trait, service process, remote MCP transport, or
  provider framework is introduced.

## Alternatives considered

### Exa `/search` plus contents

Use this only when citations, URLs, domain filtering, or result-level control
become required. `/search` supports fields such as `numResults`,
`includeDomains`, `excludeDomains`, and nested content extraction. It returns
individual result titles/URLs/content, but requires a larger tool schema and
more output/cost policy. A separate `/contents` call is needed only for known or
revisited URLs; `/search` can already request content inline.

### Local stdio MCP server

e-agent can already spawn a local stdio MCP server. An Exa or Tavily stdio
server is therefore possible without adding native HTTP code, but it adds a
Node/Python process, startup/runtime failure modes, externally defined schemas,
and prefixed tool names. Exa's hosted remote MCP endpoint is **not** directly
usable because e-agent deliberately supports no remote HTTP/SSE MCP transport.

### Tavily `/search`

Tavily is a viable fallback with URL/title/score results and domain filters:

```http
POST https://api.tavily.com/search
Authorization: Bearer $TAVILY_API_KEY
Content-Type: application/json

{
  "query": "...",
  "search_depth": "basic",
  "max_results": 5,
  "include_raw_content": false,
  "include_answer": false
}
```

Do not implement it in v1: supporting a second provider would force a provider
choice/configuration seam before there are two real implementations.

### Kimi builtin `$web_search`

Earlier research concluded Kimi had no documented builtin web search. That
conclusion is now stale: Moonshot currently documents a
`builtin_function` named `$web_search` for models including `kimi-k3` and
`kimi-k2.6`.

It is still not recommended for this feature because Kimi's own current guide
and pricing page say web search is being updated, the documentation is outdated,
and near-term use is not recommended. Also, the observed
`/coding/v1/search` and `/fetch` endpoints remain undocumented and must not be
treated as a supported API contract. A model-wire builtin would additionally
couple search availability to one model provider, whereas the native Exa tool
works with every configured model.

## Explicit non-goals

- Remote HTTP/SSE MCP.
- Multiple search providers, fallback, or a provider trait.
- Kimi's undocumented `/coding/v1/search` or `/fetch` endpoints.
- URL fetch, arbitrary browsing, crawling, scraping, or browser automation.
- Citations, domain filters, result lists, result reranking, or freshness knobs.
- Caching, persistent search history, usage accounting, or cost UI.
- Automatic retries, background search jobs, queues, or rate-limit scheduling.
- User-exposed token/result/provider controls.
- Sending private/local code or other sensitive data to a search provider.

## Authoritative references

- Exa Context API: <https://exa.ai/docs/reference/context>
- Exa errors: <https://exa.ai/docs/reference/error-codes>
- Exa rate limits: <https://exa.ai/docs/reference/rate-limits>
- Exa pricing: <https://exa.ai/pricing>
- Exa Search API for coding agents:
  <https://exa.ai/docs/reference/search-api-guide-for-coding-agents>
- Exa Search reference: <https://exa.ai/docs/reference/search>
- Exa Contents guide:
  <https://exa.ai/docs/reference/contents-api-guide-for-coding-agents>
- Exa security: <https://exa.ai/docs/reference/security>
- Exa privacy policy: <https://exa.ai/privacy-policy>
- Kimi builtin web search: <https://platform.kimi.ai/docs/guide/use-web-search>
- Kimi tool status/pricing: <https://platform.kimi.ai/docs/pricing/tools>
- Tavily Search reference:
  <https://docs.tavily.com/documentation/api-reference/endpoint/search>
- Tavily API introduction:
  <https://docs.tavily.com/documentation/api-reference/introduction>
- Tavily privacy: <https://docs.tavily.com/documentation/privacy>
