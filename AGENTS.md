# AGENTS.md

Guidance for coding agents (and humans) working on e-agent.

## What this is

e-agent is a deliberately minimal Rust coding agent: single crate, streaming
OpenAI-compatible Chat Completions plus ChatGPT Codex Responses, four always-on
local tools (`read_file`, `write_file`, `edit_file`, `bash`) plus conditional
`web_search`, a text REPL, a ratatui TUI, and JSON session persistence. Keep it
that way.

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
  replayed to either provider wire.
- Tool errors return to the model as `role:"tool"` messages; they never
  crash the agent loop.
- File tools stay cap-std capability-relative; `bash` is NOT sandboxed
  (see README "Safety boundaries").
- Errors: anyhow chains displayed with `{:#}`; model-facing tool errors
  stay plain strings.
- `kimi-key` must never be committed (it is gitignored).
- Reasoning-model `reasoning_content` is persisted in the session for
  display/audit only and is never echoed back to the API
  (OpenAI/Kimi convention; Anthropic thinking blocks would be a wire-level
  change, not an agent-level one).
- Background task completions are injected at model-call boundaries within
  an active turn. When the agent is idle (TUI/REPL), a completion is
  delivered as a user message that starts a new turn.
- No task scheduler, priorities, or worker/concurrency pool.
- MCP support is limited to local stdio servers, tools only. No remote
  HTTP/SSE MCP, MCP OAuth, resources/prompts, notifications, `listChanged`
  refresh, server restart, or concurrent initialization.
- Subagents (`delegate` tool) are threads with isolated agent state: fresh
  history and builtin tools only (no MCP, no nested `delegate`). They share the
  parent's unbounded running-task registry so nested background bash stays
  visible. No agent-to-agent messaging or worker/concurrency pool.
  Process-level isolation (subagents as subprocesses) is the planned evolution
  but not implemented.

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
contract through `Agent::emit` → fanout to `event_handler` + all `observers`:

- ToolCall / ToolResult / AssistantText / Usage always go through `emit`.
- Streaming deltas (AssistantDelta / ReasoningDelta) in `run_loop` fan out to
  BOTH the per-turn handler and every session observer — a session with no
  live subscriber still records them (the log is the source of truth for
  late attach / snapshot replay).
- The ONLY intentional asymmetry: `compact()` deltas go to the handler only,
  because compaction is background maintenance and must not paint over a
  session's visible scrollback.
- Never drop a SessionSink based on live-receiver count — a session with no
  attached view has zero receivers but its log must keep filling.
- A failed turn emits its error as an event too; a session must never fail
  silently into an empty log.

## Steering transport: main agent vs subagent

Every session supports the same two steering operations — queue a prompt,
cancel the in-flight turn. Only the *transport* differs, and only because of
where the frontend lives:

- **Subagent (cross-thread):** the frontend steers through `SessionHandle`'s
  `Steer::{Prompt, Cancel}` channel. `send_input` also records a
  `UserPrompt` event in the session log first (see event semantics above).
- **Main agent (same task):** the TUI holds `&mut Agent`, so it calls
  `agent.run(prompt)` and cancels by dropping the in-flight future directly —
  no `Steer` channel is used.

These are two transports for one semantic contract, not two message types.
Do not add a third path; if the main agent is ever moved behind a handle, it
should adopt the same `Steer` channel rather than inventing a new one.
