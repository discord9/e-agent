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

The `bash` tool can be sandboxed with `bubblewrap` when it is installed. The
sandbox is off unless explicitly enabled:

```toml
[sandbox]
enabled = true
# network = false            # unshare the network namespace (default: shared)
# workspace_writable = false # mount the workspace read-only (default: writable)
# Extra mounts, e.g. toolchain caches the agent needs inside the sandbox:
# writable_paths = ["/mnt/big/cargo-home"]
# readable_paths = ["~/.rustup", "~/.local"]
```

`writable_paths` / `readable_paths` are unioned with the project-local
`<workspace>/.e-agent/config.toml` `[sandbox]` section (which can add paths but
never change `enabled` / `network` / `workspace_writable`). A leading `~`
expands to the home directory; relative paths resolve against the workspace
root. Paths that do not exist on the host are skipped, so a missing cache
directory never breaks a run.

Note that the sandbox hides all of `$HOME` behind a fresh tmpfs and then
binds in only the workspace and these extra paths — so personal files such as
`~/.ssh`, `~/.gitconfig`, or `~/.config` are NOT visible inside the sandbox
unless explicitly listed in `readable_paths`. Toolchains installed under
`$HOME` (rustup, pyenv, nvm, ...) must be listed for the agent to use them.

See "Safety boundaries" for what the sandbox does and does not constrain.

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
the app; it never cancels a turn. Ctrl-C cancels the in-flight turn and quits
at the idle prompt. On exit, e-agent prints `e-agent --session <id>` so the
session can be resumed from the same workspace.

The four always-present tools are `read_file`, `write_file`, `edit_file`, and
`bash`. `web_search` is a fifth tool that searches public documentation and
code examples through the Exa Context API; it is enabled when `EXA_API_KEY` is
set to a non-whitespace value, or when `[web_search]` sets `api_key_file` /
`api_key_env` in the config (process env wins). OpenCode's own Exa
configuration is not inherited.
The three file tools use a capability-relative directory rooted at the
canonical workspace (the current directory by default). At startup, e-agent
loads a non-empty `AGENTS.md` from that workspace root into the system context
for both the main agent and delegated subagents. It does not search parent or
nested directories, and the instructions are not persisted in sessions.

Workspace skills live in `.e-agent/skills/<name>/SKILL.md` relative to the
canonical workspace root. At startup, the main agent loads all skills, sorted
by `<name>` in dictionary order, and injects each as a
`## Skill: <name>\n\n<content>` block — all skills are merged into a single
context segment. The injection order is: main role template
(`.e-agent/agents/main.md`) → workspace `AGENTS.md` → skills → MCP server
instructions. Skills are loaded for the main agent only; subagents do not
inherit them. Missing `.e-agent/skills`, empty directories,
non-directory entries, subdirectories without `SKILL.md`, and empty
`SKILL.md` files are all silently skipped. Real I/O or UTF-8 errors on a
`SKILL.md` that should be readable produce an error with path context.
`read_file` pages long files by 1-indexed lines (optional `offset`/`limit`,
default 2000 lines per read, capped at 64 KiB) and prints a continuation hint
when more lines remain.
Tool calls and their results are displayed as they happen; the final answer
ends the turn.

## Safety boundaries

The workspace capability constrains only the agent's file tools. It permits
symlinks that remain inside the workspace and rejects symlink escapes. It does
not constrain hardlink inode origins, does not make a read-then-write sequence
atomic, and is not a sandbox for the whole process.

`bash` runs `/bin/bash -lc` with the workspace as its current directory. By
default it is not sandboxed: commands can access files outside the workspace,
environment variables, and the network. On Unix, timed-out commands are killed
as a process group; on non-Unix platforms only the direct child is killed.
stdout and stderr are each captured up to 64 KiB, then drained and marked as
truncated.

Optional sandboxing is available when `bubblewrap` (`bwrap`) is installed and
`[sandbox] enabled = true` is set in the config. Every `bash` call — main agent
and subagents alike — is then wrapped in `bwrap`: system directories are
mounted read-only, the workspace is mounted read-write (`workspace_writable =
false` makes it read-only), `/tmp` and `/home` are fresh tmpfs, PID/IPC/UTS
namespaces are unshared, and TIOCSTI is blocked via `--new-session`. Network
stays available by default; `network = false` unshares it. When the host uses
systemd-resolved, the stub resolver at `/run/systemd/resolve` is mounted
read-only so that DNS resolution via the symlinked `/etc/resolv.conf` works
inside the sandbox. The sandbox constrains
the spawned command, not the agent process itself. This sandbox is best-effort:
it is not setuid bwrap, the host network is shared by default, and environment
variables of the parent process other than the stripped credential names
(`EXA_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `DEEPSEEK_API_KEY`,
`MOONSHOT_API_KEY`, `KIMI_API_KEY`) remain visible inside the sandbox.

Additionally, when the sandbox is enabled, the protection applied to the
workspace `.git` entry depends on the agent role:

- **Main agent (orchestrator):** the `.git` directory (or linked-worktree
  pointer file) is left writable so that `git add`, `git commit`,
  `git worktree`, `git cherry-pick`, and other repository operations work
  normally. The main agent needs to mutate Git metadata to drive the
  development workflow.
- **Delegated subagent / fixer:** the sandbox binds `<workspace>/.git`
  read-only **over itself** after all writable mounts. This prevents the
  subagent from deleting or corrupting the pointer, running `git init`, or
  writing any commit metadata — protecting the workspace Git metadata against
  accidental corruption by a delegated subtask.

This protection covers only the `.git` entry inside the workspace; it does
not cover a repository's object store, packed references, or other data that
resides outside the workspace (e.g. in a common Git directory referenced by a
linked-worktree pointer). This protection does not prevent `git` commands from reading
the repository, and it does not extend to any repository outside the workspace
that happens to be mounted via `writable_paths`.

Background bash commands inherit their parent's role: a background task
started by the main agent has writable `.git`; one started by a subagent has
`.git` bound read-only.

Every `web_search` query is disclosed to Exa, a third party. Never include
credentials, tokens, private repository contents, customer data, personal data,
private issue text, or internal URLs. Returned text is untrusted web content:
it may contain prompt injection, insecure code, or false claims. The tool does
not open links, execute returned code, or fetch arbitrary URLs. This guidance
is not a sandbox and cannot guarantee that a model will not disclose sensitive
context.

## Background tasks

The `bash` tool accepts `background: true` to run a command without blocking.
Bash tasks and subagents share one running-task registry with no built-in
concurrency limit. Each bash task starts a process and each subagent starts an
OS thread, so callers remain responsible for resource use. Background bash has
the same workspace cwd, process-group handling, and per-stream 64 KiB output
limit as foreground Bash, with a 30-minute timeout.
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

Each subagent runs on its own OS thread with its own tokio runtime and an empty
history. It shares the parent's running-task registry so nested background bash
commands stay visible and report completion to the parent. It gets the same
builtins as its parent: file/bash tools and, when
configured, `web_search`; no MCP tools and no `delegate` itself, so delegation
depth is capped at 1 by construction. Tool rounds are unlimited.

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
- `EXA_API_KEY` — optional; a non-whitespace value enables `web_search` through
  Exa Context.
- `RUST_BACKTRACE=1` — optional; appends a backtrace to printed error chains.

TOML config is optional. When present, it supplies the selected provider
credential and profile values instead of `OPENAI_*`; see [Run](#run). A
provider using `api_key_env` reads the variable named by that field (for
example, `KIMI_API_KEY`).

Errors print their causal chain (for example `cannot decode provider
response: error decoding response body: ...`), and provider HTTP failures
include a preview of the response body. Provider requests time out after
600 seconds in total, including the complete streaming response body.

## GreptimeDB session backend (experimental)

An optional GreptimeDB-backed session storage backend can be selected at
runtime via the TOML config file. It replaces the JSONL file backend for
session persistence while keeping background-task bookkeeping in JSONL.

### Build

```sh
cargo build --features greptime
```

### Configure

```toml
[session]
backend = "greptime"
conn = "host=127.0.0.1 port=4002 dbname=public"
```

The connection string is passed directly to `tokio-postgres`. Default port
for GreptimeDB's pg-wire protocol is `4002`. The server must be running
before e-agent starts (no auto-discovery or embedded mode).

### Table schema

```sql
CREATE TABLE IF NOT EXISTS session_entries (
    workspace_id    STRING       NOT NULL,
    session_id      STRING       NOT NULL,
    seq             BIGINT       NOT NULL,
    event_time      TIMESTAMP(9) NOT NULL TIME INDEX,
    entry_kind      STRING       NOT NULL,
    payload         STRING       NOT NULL,
    schema_version  INT          NOT NULL DEFAULT 1,
    agent_role      STRING       NOT NULL DEFAULT 'main',
    is_error        BOOLEAN      NOT NULL DEFAULT FALSE,
    appended_at     TIMESTAMP(9) NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, session_id)
) WITH (append_mode = 'true', sst_format = 'flat')
```

`workspace_id` is derived from the canonical workspace root path, so sessions
with the same name in different workspaces remain isolated.

### Known limitations

- GreptimeDB server must be running before e-agent starts
- `tokio-postgres` needs the `with-chrono-0_4` feature for TIMESTAMP params
  (already included in the `greptime` feature)
- `event_time` is real wall-clock time but may have nanosecond collisions
  across processes (same-ns entries get prev+1 via a monotonic atomic guard)
- No transactions — atomicity is per multi-row INSERT statement
- Subagents each open their own database connection (bounded, fine for
  single-user agents)

### Query patterns

| Operation | Query | Optimization |
|-----------|-------|-------------|
| connect (find max seq) | `last_value(seq ORDER BY event_time ASC) GROUP BY workspace_id, session_id` | LastRow scan hint → reads ~2 rows |
| load (all entries) | `SELECT seq, payload ... ORDER BY event_time DESC` | WindowedSortExec + app-side HashMap dedup |
| append | Parameterized multi-row INSERT | Single statement, all-or-nothing per chunk |

### Tests

Integration tests require a live GreptimeDB and opt-in via the `GREPTIME_PG`
environment variable:

```sh
GREPTIME_PG="host=127.0.0.1 port=4002 dbname=public" cargo test --features greptime
```

Without `GREPTIME_PG` the integration tests skip with a message.
## Non-goals

This is deliberately not a daemon, JSONL protocol,
automatic compaction trigger, subagent
framework, permission framework, plugin host, generic provider/auth framework,
parallel tool executor, task scheduler, priority system, or concurrency pool.
It deliberately does not fetch provider/model catalogs (including
models.dev), generate configuration, cache provider metadata, or infer
context windows; TOML profiles are local static settings only. `AGENTS.md`
loading is workspace-root-only: there is no parent/nested discovery or merging.
Subagents exist but are deliberately minimal: no agent-to-agent messaging,
no delegation deeper than 1 level, no
process-level isolation yet (subagents are threads, not subprocesses).
It does speak MCP to local stdio servers (tools only), but it does NOT do
remote MCP over HTTP/SSE, MCP OAuth, MCP resources/prompts, server-initiated
notifications, `listChanged` refresh, server restart on crash, or concurrent
server initialization.
It has one model seam and one tool seam. Main-agent tool rounds are unlimited
unless `--max-rounds` sets an explicit cap; subagent tool rounds are unlimited.
Reasoning-model `reasoning_content` is persisted in the session for
display/audit; it is never sent back to the API.
Web search deliberately has no browser, crawler, URL fetch, citations, domain
filters, multiple providers or provider trait, retries, cache, background
search, or remote MCP support.
Workspace skills are deliberately flat `.e-agent/skills/<name>/SKILL.md` only:
no YAML/front matter parsing, remote installation, registry, dependencies, hot
reload, slash commands, on-demand loading, config toggle, or recursive skill
discovery. Skills are main-agent-only and are not inherited by subagents.

GreptimeDB-specific non-goals (when built with `--features greptime`):

- No automatic migration of existing JSONL sessions — existing JSONL sessions
  are not migrated to GreptimeDB at startup (a one-shot importer was built and
  used during the PoC; removed before merge as an experiment artifact)
- No automatic migration between backends — switching backends in config does
  not transfer session data
- No cross-workspace session sharing — sessions with the same name in
  different workspaces are fully isolated by workspace_id
- No transaction support — atomicity is per multi-row INSERT statement, not
  across statements
- No background task persistence in GreptimeDB — background bookkeeping stays
  in JSONL files regardless of backend
- No distributed/cluster deployment — standalone GreptimeDB only
- No generic storage abstraction for future third backends
