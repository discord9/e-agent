# AGENTS.md

Guidance for coding agents (and humans) working on e-agent.

## What this is

e-agent is a deliberately minimal Rust coding agent: single crate, streaming
OpenAI-compatible Chat Completions plus ChatGPT Codex Responses, six always-on
local tools (`read_file`, `write_file`, `edit_file`, `bash`,
`get_background_tasks`, `cancel_background_task`) plus conditional
`web_search` and main-agent-only `delegate`, a text REPL, a ratatui TUI, a
headless HTTP server (`--serve` / `web`) with a token-authenticated `/api`
and a browser web UI, and session persistence over three backends (JSONL
default, GreptimeDB, SQLite). Keep it that way.

## Prime directive: do not over-design

"Over-design" is a matter of taste, so apply these objective rules instead:

1. **Working loop before abstractions.** Never add infrastructure for a
   feature that does not exist yet.
2. **One adapter, no seam.** A trait needs two real implementations to
   justify existing; hypothetical future implementations do not count.
   Currently sanctioned seams: `Model` and `Tool` only.
3. **The deletion test.** Before adding a module, layer, or trait, ask: if
   this were deleted, would its complexity reappear across many callers, or
   simply vanish? If it vanishes, do not add it.
4. **State non-goals.** Every design proposal must list what it deliberately
   does NOT do. Extend the "Non-goals" list in README.md; never silently
   shrink it.
5. **Line budget.** Production code stays small. If a change roughly doubles
   a file, shrink the behavior, not just the formatting.

Historical context: the predecessor project died from protocol crates, event
sourcing, subagent frameworks, and permission systems built before a single
working model→tool→model loop existed. Do not reintroduce these patterns
without an explicit user request.

## Non-negotiables (current scope)

- Single crate: `src/lib.rs` core + `src/main.rs` thin frontend.
- Streaming `/chat/completions` and ChatGPT Codex `/responses` (SSE) with full
  transcript replay; persisted reasoning is display/audit only and is never
  replayed to either provider wire — the sole exception is an explicit
  DeepSeek Chat compatibility profile (`deepseek_compat = true`), which
  replays `reasoning_content` on thinking-mode tool-call assistant turns.
- Tool errors return to the model as `role:"tool"` messages; they never
  crash the agent loop.
- File tools stay cap-std capability-relative; `bash` is not sandboxed by
  default, but can be wrapped in `bwrap` via `[sandbox] enabled = true`
  (see README "Safety boundaries").
- Errors: anyhow chains displayed with `{:#}`; model-facing tool errors
  stay plain strings.
- API key files must never be committed (they are gitignored).
- Reasoning-model `reasoning_content` is persisted in the session for
  display/audit only and is never echoed back to the API by default
  (OpenAI/Kimi/Codex convention; Anthropic thinking blocks would be a
  wire-level change, not an agent-level one). The ONLY exception is an
  explicit DeepSeek Chat compatibility profile: when `deepseek_compat =
  true`, `thinking = true` and the assistant message carries `tool_calls`,
  the original `reasoning_content` (including an empty string) is echoed
  back on the next request — DeepSeek's thinking-mode contract requires it.
  This is per-profile explicit opt-in, never inferred from provider/model
  strings.
- Background task completions are injected at model-call boundaries within
  an active turn. When the agent is idle (TUI/REPL), a completion is
  delivered as a user message that starts a new turn.
- No task scheduler, priorities, or worker/concurrency pool.
- MCP support is limited to local stdio servers, tools only. No remote
  HTTP/SSE MCP, MCP OAuth, resources/prompts, notifications, `listChanged`
  refresh, server restart, or concurrent initialization.
- Subagents (`delegate` tool) are independent `SessionRunner` tasks on the
  shared Tokio runtime, with isolated Agent state: fresh history and builtin
  tools only (no MCP, no nested `delegate`). They share the parent's unbounded
  running-task registry so nested background bash stays visible. No
  agent-to-agent messaging or worker/concurrency pool. Process-level isolation
  (subagents as subprocesses) is the planned evolution but not implemented.

## Commands

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
```

All four must pass before a change is considered done.

## Session event semantics (main agent == subagent)

Every session — main or subagent — must observe the SAME event-production
contract, and there is exactly one fanout path per session: the runner's
`Shared::emit_agent` (see `src/runner.rs`). `Agent::emit` only invokes the
session's single `event_handler` closure, which the runner installs as
`Shared::emit_agent`; there is no separate observer list on `Agent`. The TUI,
the web SSE layer, and attached subagent views all observe the same events
through `Shared`'s broadcast channel.

- `Shared::emit` appends the event to the session's shared `log` AND
  broadcasts it to every live subscriber. ToolCall / ToolResult /
  AssistantText / Usage / UserPrompt / errors all go through this path.
- A session with no live subscriber still records every event in its `log` —
  the log is the source of truth for late attach / snapshot replay
  (`SessionHandle::attach` atomically clones the log and subscribes).
- The ONLY intentional asymmetry: while a compaction is streaming,
  `emit_agent` sends AssistantDelta / ReasoningDelta to the broadcast
  channel only (`emit_transient`) — they are never appended to the log.
  After the compaction entry is durably committed, the complete projection is
  emitted as a single event, so background compaction never paints partial
  deltas over a session's visible scrollback.
- A failed turn emits its error as an event too; a session must never fail
  silently into an empty log.

## Steering transport: main agent and subagent

Every session supports the same steering operations through its concrete
`SessionHandle` command channel: queue a prompt, request compaction, or cancel
the in-flight turn. Main-agent and subagent frontends use this same transport.
Queued prompts are transient UI state; `UserPrompt` is emitted and persisted
only when the runner consumes the prompt. Do not add a second steering path.

## Images: delegate to the `seer` role

The main model and most subagents run text-only models. When the user
references an image (a path, an attachment, "look at this screenshot") or
asks anything that requires seeing an image, do NOT say you cannot see it
and do NOT pass the image into your own context: delegate to a `seer`
subagent instead.

- Use the `delegate` tool with the absolute `workspace` as the first argument, `role: "seer"`, and a task that names the
  image path (or says the image is attached) and the user's exact question.
- The seer runs on a vision-capable model, calls `read_image` itself, and
  returns a text description you can relay verbatim.
- For follow-up questions about the same image, `resume` the same seer
  session (`resume: "<sub-...>"`) so it keeps its context.
