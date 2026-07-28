# AGENTS.md

Guidance for coding agents (and humans) working on e-agent.

## What this is

e-agent is a deliberately minimal Rust coding agent: single crate,
streaming OpenAI-compatible API, four tools (`read_file`, `write_file`,
`edit_file`, `bash`), a text REPL, a ratatui TUI, and JSON session persistence.
Keep it that way.

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
- Streaming `/chat/completions` (SSE) with full transcript replay; `reasoning_content` is persisted for display/audit but never echoed back to the API.
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
  HTTP/SSE MCP, OAuth, resources/prompts, notifications, `listChanged`
  refresh, server restart, or concurrent initialization.
- Subagents (`delegate` tool) are threads with fully isolated state: fresh
  history, own background slots, builtin tools only (no MCP, no nested
  `delegate`). No agent-to-agent messaging, no subagent session persistence.
  Process-level isolation (subagents as subprocesses) is the planned
  evolution but not implemented.

## Commands

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
```

All four must pass before a change is considered done.
