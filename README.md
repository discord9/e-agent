# e-agent

e-agent is a small streaming Rust coding agent for OpenAI-compatible
`/chat/completions` APIs and the ChatGPT Codex `/responses` API. It sends a
prompt to a model, executes requested tools one at a time inside a workspace,
returns each result to the model, and prints the model's final answer.

## Run

The usual setup needs no environment variables. Create
`$XDG_CONFIG_HOME/e-agent/config.toml` (or, when that is unset or absent,
`$HOME/.config/e-agent/config.toml`):

```toml
default = "kimi/k3"

[providers.kimi]
base_url = "https://api.kimi.com/coding/v1"
api_key_file = "kimi-key" # absolute or relative to this config file
# Alternatively: api_key_env = "KIMI_API_KEY"

[models."kimi/k3"]
model = "k3"
# Optional: Kimi Coding k3 accepts "low", "high", or "max".
reasoning_effort = "high"
```

Provider names are inferred from the part of the profile before `/`. API-key
providers need exactly one of `api_key_file` or `api_key_env`; key files are
trimmed and must not be empty. `reasoning_effort` is an optional pass-through
request field with no CLI or environment override. For Kimi Coding `k3`, its
canonical values are `low`, `high`, and `max`; omitting it uses Kimi Coding's
`high` default. Other providers may define different values.

ChatGPT/Codex profiles use the separate Responses API and browser login; they
do not accept an API key or base URL:

```toml
[providers.chatgpt]
auth = "chatgpt"

[models."chatgpt/gpt-5.6-sol"]
model = "gpt-5.6-sol"
reasoning_effort = "high"
```

Run `e-agent login` to complete the browser login, then select that profile as
usual. `e-agent logout` removes only `$XDG_CONFIG_HOME/e-agent/auth.json` (or
`$HOME/.config/e-agent/auth.json`). ChatGPT credentials are never stored in a
workspace session. `--model` is allowed for these profiles; `--base-url` is
rejected because the Codex endpoint is fixed.

Built-in roles can be routed to a different profile with `[roles]`. The only
role today is `subagent` (the model the `delegate` tool spawns); unrouted
roles fall back to the main profile:

```toml
[models."kimi/k2"]
model = "k2"

[roles]
subagent = "kimi/k2"
```

Then run:

```sh
cargo run -- "inspect src/main.rs and explain it"
```

Without a TOML config, e-agent keeps its existing OpenAI environment-variable
mode:

```sh
export OPENAI_API_KEY=...
export OPENAI_BASE_URL=https://api.openai.com/v1
export OPENAI_MODEL=gpt-4o-mini
cargo run -- "inspect src/main.rs and explain it"
```

When no prompt argument is supplied and stdout is a terminal, e-agent starts
an interactive TUI (scrollback plus an input line with proper Unicode editing);
use `--repl` for a plain line-based REPL instead. If stdin is piped, the prompt
is read from standard input. Sessions persist to
`.e-agent/sessions/<name>.jsonl` in the workspace (default name `default`) as
an append-only log of message and compaction entries; the whole history is
restored and replayed in the TUI on startup, while the model only sees the
latest compaction summary and everything after it. Legacy `.json` sessions
are migrated on first load. Optional CLI overrides are `--base-url URL`,
`--model MODEL`, `--profile PROFILE`, `--workspace PATH`, `--session NAME`, and `--max-rounds N`
(tool-call rounds are unlimited by default; the flag sets an explicit cap).
`--profile` selects a TOML profile and requires a TOML config. When config is
used, `--base-url` and `--model` are raw API wire-value overrides and take
precedence over the selected profile values.

Keys in the TUI: Esc always leaves the current view — it detaches from a
subagent view, closes the tasks panel, or (at the plain idle prompt) quits
the app; it never cancels a turn. Ctrl-C cancels the in-flight turn.

The available tools are `read_file`, `write_file`, `edit_file`, and `bash`.
The three file tools use a capability-relative directory rooted at the
canonical workspace (the current directory by default). `read_file` pages long
files by 1-indexed lines (optional `offset`/`limit`, default 2000 lines per
read, capped at 64 KiB) and prints a continuation hint when more lines remain.
Tool calls and their results are displayed as they happen; the final answer
ends the turn.

## Safety boundaries

The workspace capability constrains only the agent's file tools. It permits
symlinks that remain inside the workspace and rejects symlink escapes. It does
not constrain hardlink inode origins, does not make a read-then-write sequence
atomic, and is not a sandbox for the whole process.

`bash` runs `/bin/bash -lc` with the workspace as its current directory. It is
not sandboxed: commands can access files outside the workspace, environment
variables, and the network. On Unix, timed-out commands are killed as a process
group; on non-Unix platforms only the direct child is killed. stdout and stderr
are each captured up to 64 KiB, then drained and marked as truncated.

## Background tasks

The `bash` tool accepts `background: true` to run a command without blocking.
Up to 4 background tasks may run concurrently (bash and subagents share the
same slots); each has the same workspace cwd, process-group handling, and
per-stream 64 KiB output limit as foreground Bash, with a 30-minute timeout.
Completions surface as a TUI notification while a
turn is active, and the complete output is injected as a clearly labelled user
message at the next model call — whether that is the next round of an active
turn or, if the agent is idle, a new turn started by that delivery. F2 toggles
a tasks panel listing what is running. In
one-shot mode a still-running task is reported before exit;
the process then best-effort terminates its process group. Uninjected background
results are not persisted in sessions.

## Subagents

The `delegate` tool spawns a subagent with a fresh context to work on a
self-contained task and returns its final answer. Use it for subtasks whose
intermediate steps (searching, reading many files, focused edits) would
clutter the main context.

Each subagent runs on its own OS thread with its own tokio runtime, its own
background-task slots, and an empty history — it shares no state with the
parent. It gets only the builtin file/bash tools: no MCP tools and no
`delegate` itself, so delegation depth is capped at 1 by construction. It is
limited to 32 tool rounds.

With `background: true` the subagent runs without blocking and its answer is
delivered as a background task completion (waking an idle agent). Sync mode
(the default) waits for the answer with a 30-minute ceiling.

A running background subagent can be watched live in the TUI: open the tasks
panel with F2, select it with Up/Down, and press Enter to attach. The
attached view replays everything the subagent has done so far and then
follows its stream (text, tool calls, results) with the same rendering as the
main view; the footer shows the task and whether it is still running. Esc
detaches back to the main view (it never cancels the main turn), and the
subagent keeps running after detach. Attaching is read-only; bash tasks have
no event stream and cannot be attached.

## Compaction

Use `/compact` in the TUI or `--repl` to manually summarize earlier history.
The agent summarizes everything before the current turn and appends a
compaction entry (summary plus the retained turn) to the append-only session
log; earlier messages stay persisted and visible in the TUI. Subsequent model
calls use the latest summary plus everything after it. Compaction has no
automatic trigger.

## MCP (local servers)

e-agent can connect to local MCP servers over stdio and expose their tools
to the model with a `<server>_<tool>` name prefix. Servers are declared in
the same TOML config file as model profiles
(`$XDG_CONFIG_HOME/e-agent/config.toml`, falling back to
`~/.config/e-agent/config.toml`):

```toml
[mcp.engram]
command = ["/path/to/engram", "mcp", "--tools=agent"]
enabled = true        # optional, default true
# cwd = "/other/dir"  # optional, defaults to the workspace root
# env = { KEY = "value" }
```

Each entry spawns `command` with the workspace as its current directory
(override with `cwd`), plus optional `env`. Servers are connected one at a
time at startup; a server that fails to start is skipped with a warning and
does not abort the session. Server-provided instructions (from the MCP
`initialize` response) are prepended to the model context as a system
message on every call, but are not persisted in sessions. Only tools are
supported — no resources, prompts, or server-initiated notifications.

## Environment

- `OPENAI_API_KEY` — required in environment-variable mode.
- `OPENAI_BASE_URL` — API base URL, default: `https://api.openai.com/v1`.
- `OPENAI_MODEL` — model name, default: `gpt-4o-mini`.
- `RUST_BACKTRACE=1` — optional; appends a backtrace to printed error chains.

TOML config is optional. When present, it supplies the selected provider
credential and profile values instead of `OPENAI_*`; see [Run](#run). A
provider using `api_key_env` reads the variable named by that field (for
example, `KIMI_API_KEY`).

Errors print their causal chain (for example `cannot decode provider
response: error decoding response body: ...`), and provider HTTP failures
include a preview of the response body. Provider requests time out after
600 seconds in total, including the complete streaming response body.

## Non-goals

This is deliberately not a daemon, JSONL protocol,
automatic compaction trigger, database, event store, subagent
framework, permission framework, plugin host, generic provider/auth framework,
parallel tool executor, task scheduler, priority system, or concurrency pool.
It deliberately does not fetch provider/model catalogs (including
models.dev), generate configuration, cache provider metadata, or infer
context windows; TOML profiles are local static settings only.
Subagents exist but are deliberately minimal: no agent-to-agent messaging,
no delegation deeper than 1 level, no subagent session persistence, no
process-level isolation yet (subagents are threads, not subprocesses).
It does speak MCP to local stdio servers (tools only), but it does NOT do
remote MCP over HTTP/SSE, MCP OAuth, MCP resources/prompts, server-initiated
notifications, `listChanged` refresh, server restart on crash, or concurrent
server initialization.
It has one model seam and one tool seam. Main-agent tool rounds are unlimited
unless `--max-rounds` sets an explicit cap; subagents remain capped at 32.
Reasoning-model `reasoning_content` is persisted in the session for
display/audit; it is never sent back to the API.
