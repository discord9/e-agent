# OpenTelemetry Integration Research

Status: research complete; not implemented  
Date: 2026-07-29  
Scope: e-agent process telemetry, not its storage backend

## Executive summary

OpenTelemetry is technically straightforward to add to e-agent, but a broad
logs + metrics + traces integration would conflict with the project's minimal
scope and create unnecessary privacy and lifecycle risk.

The recommended first increment is an **opt-in, Cargo-feature-gated,
trace-only integration**. It should emit a small number of operation spans for
agent turns, model calls, tool calls, and compaction. It must not export prompt
text, model output, reasoning, tool arguments/results, shell commands, file
paths, credentials, or session contents.

The difficult part is not creating spans. The TUI exits with
`std::process::exit`, which bypasses normal destructors, so the exporter must be
explicitly flushed and shut down after terminal restoration. The implementation
must also use one process-wide provider: subagents run on separate threads with
current-thread Tokio runtimes and must not initialize their own exporters.

## Current codebase findings

### No existing telemetry stack

`Cargo.toml` currently has no direct dependency on `tracing`, `log`, or any
OpenTelemetry crate. Adding OTel is therefore a new infrastructure surface, not
just another subscriber layer.

### Useful instrumentation seams

- `src/agent.rs:388-420` — `Agent::run` / `run_turn`: one span per logical
  agent turn, including automatic follow-up turns for background completions.
- `src/agent.rs:493-583` — `run_loop`: one child span per model round and one
  child span per tool execution.
- `src/agent.rs:426-469` — `compact`: a distinct model operation whose token
  accounting intentionally differs from a regular turn.
- `src/agent.rs:476-490` — usage accounting: safe numeric fields are already
  centralized here.
- `src/model.rs:109-240` and `src/codex.rs:86-185` — provider streaming lasts
  until the complete SSE response is consumed, so a model span around
  `Model::complete` measures the useful end-to-end request duration.
- `src/agent.rs:589-596` — tool dispatch is centralized, allowing one
  instrumentation point for every local, MCP, and delegated tool.

No new telemetry trait is justified. The existing `Model` and `Tool` seams are
enough; instrumentation belongs around calls at the agent orchestration layer.

### Event fanout is not a safe telemetry seam

`AgentEvent` (`src/agent.rs:90-120`) carries user prompts, assistant text and
deltas, reasoning deltas, tool arguments/results, notices, and background-task
output. Exporting all events, or deriving generic span events from `Agent::emit`,
would leak sensitive data and produce very high-volume telemetry.

Instrumentation must be operation-based with explicitly allowlisted fields.

### Runtime and shutdown constraints

- `src/main.rs:18` uses the Tokio multi-thread runtime, which is suitable for a
  process-wide batch span processor.
- `src/delegate.rs:181-189` creates a separate OS thread and current-thread
  runtime for each subagent. Subagents may emit through the global subscriber,
  but must not create or shut down providers.
- Context does not automatically cross arbitrary `std::thread::spawn`
  boundaries. Parentage must be propagated explicitly if parent → delegated
  subagent traces are required; otherwise subagent turns may be independent
  roots in the first version.
- `src/tui.rs:264-276` restores the terminal and then calls
  `std::process::exit` to avoid hanging on long-lived MCP tasks. This bypasses
  normal `Drop`, so telemetry needs an explicit, bounded shutdown in that exit
  path after the terminal is restored.
- The subscriber must not install a terminal `fmt` layer. Writing tracing
  output to stdout/stderr would corrupt the ratatui alternate screen.

## Recommended first increment

### Build and activation

Add a Cargo feature named `telemetry`; leave it disabled by default. A normal
e-agent build must not compile the OTel stack.

Even in a telemetry-enabled build, do not implicitly export to localhost.
Initialize the provider only when an explicit OTLP endpoint is configured.
Use standard `OTEL_*` exporter/resource/sampling variables rather than adding
an e-agent TOML configuration section in the first version.

### Module shape

A small `src/telemetry.rs` module passes the deletion test: without it, provider
construction, environment handling, flush behavior, and tests would spread
through `main`, `tui`, and `agent`.

Its interface should stay close to:

```text
Telemetry::init_from_env() -> Result<Option<Telemetry>>
Telemetry::shutdown(self) -> Result<()>
```

It is a concrete module, not a new adapter seam. There is only one real exporter
implementation.

### Initial spans

| Span | Suggested safe fields |
| --- | --- |
| `agent.turn` | agent role (`main`/`subagent`), outcome, round count |
| `model.call` | model identifier, operation (`turn`/`compact`), outcome, input/output token counts, coarse retry count/status class |
| `tool.call` | tool name, outcome, background boolean |
| `agent.compact` | outcome, input/output token counts |

Durations come from span timing and do not need separate metrics in the first
increment. Collector/backend processing can derive call counts, latency
distributions, and error rates from these spans.

### Attribute privacy policy

Allowed values must be low-cardinality and explicitly selected. Never attach:

- prompt, completion, reasoning, or compaction-summary text;
- tool arguments/results, shell commands, or background-task output;
- file paths/content, workspace paths, internal URLs, or session transcripts;
- request/response headers, authorization values, API keys, or OAuth tokens;
- provider error bodies or arbitrary formatted error chains;
- session IDs, tool-call IDs, user identifiers, or URLs with query strings.

Attribute count/length limits protect memory and cost but are not redaction.
The allowlist at each instrumentation site is the privacy control.

## Current Rust OTel stack

The compatible set researched in July 2026 is:

- `opentelemetry` 0.32.1;
- `opentelemetry_sdk` 0.32.1;
- `opentelemetry-otlp` 0.32.1;
- `tracing` 0.1.44;
- `tracing-subscriber` 0.3.23;
- `tracing-opentelemetry` 0.33.0.

Pin the initial OTel versions and upgrade the compatible set together. OTel
Rust tracing APIs remain beta and OTLP exporters are release candidates; the
0.32 release also made breaking setup/configuration changes.

Use `tracing` spans plus the OTel layer rather than direct OTel span management.
It keeps async span lifetimes and propagation in the Rust instrumentation model
instead of leaking OTel context handling across the agent loop.

Do not enable `tracing-opentelemetry`'s metrics feature, a tracing `fmt` layer,
or `opentelemetry-appender-tracing` in the first increment.

## OTLP transport

Compile exactly one transport.

- **HTTP/protobuf** may have a smaller incremental dependency graph because
  e-agent already uses reqwest/Rustls. It also fits HTTP-only proxies and the
  conventional port 4318.
- **gRPC/tonic** is the conventional choice for a collector on port 4317, but
  adds tonic, HTTP/2, tower, protobuf, and TLS-provider surface.

The transport should follow the actual backend deployment rather than enabling
both “just in case.” Run `cargo tree -e features` and compare release builds
before making binary-size or startup-cost claims.

Relevant exporter variables include:

```text
OTEL_EXPORTER_OTLP_ENDPOINT
OTEL_EXPORTER_OTLP_TRACES_ENDPOINT
OTEL_EXPORTER_OTLP_PROTOCOL
OTEL_EXPORTER_OTLP_TRACES_PROTOCOL
OTEL_EXPORTER_OTLP_HEADERS
OTEL_EXPORTER_OTLP_TIMEOUT
OTEL_SERVICE_NAME
OTEL_RESOURCE_ATTRIBUTES
OTEL_TRACES_SAMPLER
OTEL_TRACES_SAMPLER_ARG
OTEL_BSP_MAX_QUEUE_SIZE
OTEL_BSP_EXPORT_TIMEOUT
```

Builder calls can override environment values. Creating an exporter/provider is
still the application's responsibility; exporter-selection environment
variables do not perform full Rust SDK autoconfiguration by themselves.

## Testing strategy

No automated test should require a real collector.

1. Use the SDK test span exporter or a simple in-memory exporter.
2. Verify span names, parentage, outcome, and numeric attributes.
3. Assert the exact attribute-key allowlist and prove sample prompt/tool values
   never appear in exported spans.
4. Test explicit shutdown/flush, including the TUI exit helper without entering
   a real terminal.
5. Run both build matrices:
   - default `cargo test` / clippy / build;
   - telemetry-feature test / clippy / build.
6. If protocol wiring needs coverage later, use a local mock OTLP HTTP server or
   an explicitly opt-in collector integration test.

## Expected implementation footprint

Likely files:

- `Cargo.toml` / `Cargo.lock` — optional feature and pinned dependencies;
- `src/lib.rs` — telemetry module export;
- `src/telemetry.rs` — provider setup, environment handling, shutdown;
- `src/main.rs` — one process-wide owner;
- `src/tui.rs` — explicit shutdown before `process::exit`;
- `src/agent.rs` — operation spans and safe numeric fields;
- `README.md` — opt-in setup, privacy policy, and failure behavior.

The implementation is a small-to-medium PR, but the transitive dependency and
release-binary impact are deliberately left unquantified until measured with a
release-equivalent build.

## Explicit non-goals

- OTel logs or a generic bridge from `AgentEvent`.
- A metrics provider in the first increment.
- Prompt/model/tool/session content capture.
- Automatic reqwest, Tokio, MCP, filesystem, or process instrumentation.
- Per-subagent providers/exporters.
- Multiple exporter transports or a telemetry-provider trait.
- A telemetry dashboard, collector deployment, storage schema, or retention
  policy in this report.
- Changing session persistence as part of the telemetry integration.

Storage-backend design, including a GreptimeDB Standalone backend shared by
sessions and telemetry, is a separate migration investigation.

## Sources

- OpenTelemetry OTLP Rust 0.32.1:
  <https://docs.rs/opentelemetry-otlp/0.32.1/opentelemetry_otlp/>
- OpenTelemetry SDK trace provider:
  <https://docs.rs/opentelemetry_sdk/0.32.1/opentelemetry_sdk/trace/struct.SdkTracerProvider.html>
- OpenTelemetry batch span processor:
  <https://docs.rs/opentelemetry_sdk/0.32.1/opentelemetry_sdk/trace/struct.BatchSpanProcessor.html>
- `tracing-opentelemetry` 0.33.0:
  <https://docs.rs/tracing-opentelemetry/0.33.0/tracing_opentelemetry/>
- OpenTelemetry Rust 0.32 release notes:
  <https://github.com/open-telemetry/opentelemetry-rust/blob/main/docs/release_0.32.md>
- OTLP protocol exporter specification:
  <https://opentelemetry.io/docs/specs/otel/protocol/exporter/>
- SDK environment-variable specification:
  <https://opentelemetry.io/docs/specs/otel/configuration/sdk-environment-variables/>
