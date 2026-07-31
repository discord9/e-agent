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
# Optional: set true when the model accepts image input (attached via the
# `read_image` tool or the REPL `/image <path>` command). Defaults to false;
# a user message with images on a model without `vision = true` fails with
# "model X does not support image input". kimi/k3 and ChatGPT/Codex vision
# models should set it; deepseek-style models keep the default false.
vision = true
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

The default delegated model can be routed with `[roles] subagent`; named
role templates can also be routed by role name. Role templates are discovered
from `$XDG_CONFIG_HOME/e-agent/agents/<role>.md` (falling back to
`~/.config/e-agent/agents/`) and `<workspace>/agents/<role>.md`, with the
workspace file winning. This repository includes `designer`; unrouted named
roles fall back to the default subagent model, which itself falls back to the
main profile:

```toml
[models."kimi/k2"]
model = "k2"

[roles]
subagent = "kimi/k2"
```

### Read-only roles

A role template can declare itself read-only with a leading TOML frontmatter
block: the file's first line must be exactly `---`, the block may contain
`read_only = true` (unknown keys are ignored), and a second `---` line closes
it. The prompt is everything after the closing delimiter:

```markdown
---
read_only = true
---
Audit the workspace and report findings; do not modify anything.
```

A read-only role gets no `write_file`/`edit_file` tools. Its `bash`, when the
sandbox is enabled, runs in a narrowed bubblewrap policy: the workspace is
mounted read-only, extra writable roots are dropped, and the network is
disabled (`--unshare-net`). `read_file`, `get_background_tasks`,
`cancel_background_task`, and `web_search` (when configured) stay available —
`web_search` deliberately still reaches the network. Background bash commands
started inside the subagent inherit the role's narrowed sandbox too (they
never fall back to the parent's wider policy). If the sandbox is not enabled
(or bwrap is unavailable), a read-only role gets **no `bash` tool at all** —
fail closed rather than an unsandboxed shell.

Read-only is a tool boundary, not a host security framework: it constrains
which tools the model may call, and the bash sandbox remains a best-effort
bwrap policy (see "Safety boundaries"). Inside that sandbox `/tmp` and the
`$HOME` tmpfs remain writable scratch space; the file tools are the
authoritative write path, and they are absent.

The main session can request the same policy with `--read-only`: no
write/edit tools, no MCP tools (MCP tools carry no read-only marker), and the
main bash sandbox narrowed the same way — no bash at all when the sandbox is
disabled. Delegation stays available (spawning a subagent does not mutate the
host session), and each subagent resolves its own role template.

`--read-only` narrows **only the main session's own tools**; it is not
inherited by delegated subagents. A `delegate` subagent with no role override
resolves its default role, which carries the full toolset — including
`write_file`/`edit_file` and a writable workspace — so a subagent spawned from
a `--read-only` session **can still write to disk**. To keep subagents
read-only too, give their role template a `read_only = true` frontmatter as
described above; forced inheritance of the parent's read-only policy into
subagents is a planned future option, not implemented today.

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

`writable_paths` / `readable_paths` define one resolved policy shared by Bash
mounts and the file tools' external capabilities. A leading `~` expands to the
home directory; relative paths resolve against the main workspace. Existing
file/directory roots are canonicalized (including symlink aliases) and aliases
are deduplicated; missing global roots are skipped.

A project-local `<workspace>/.e-agent/config.toml` merges with these global
roots instead of replacing them. A project path that is a strict subpath of a
global root replaces that global root (narrowing it — the global ancestor is
dropped, so only the project subpath remains), while a project path with no
ancestor relationship is accumulated alongside the global roots. A project
path equal to a global root is a no-op, and multiple project subpaths of the
same global root all survive as separate narrowing points. An absent
file/section — or a `[sandbox]` with no paths — inherits all global roots
unchanged. If either path field appears, selection mode applies and the other
field is empty. Readable selections may come from global readable or writable
roots (downgrading authority), while writable selections must come from global
writable roots. Malformed or
unreadable project config is a startup error. Project config cannot change
`enabled`, `network`, or `workspace_writable`. A read-only child under a
writable parent is rejected rather than silently restoring write authority;
select a narrower writable root or downgrade the whole selected root instead.
The exact workspace-relative `.e-agent/config.toml` policy file is readable by
file tools but write/edit protected and must be changed by the user outside the
agent. Bash binds an existing policy over `/dev/null`; if only `.e-agent/`
exists, that directory is mounted read-only so a writable child cannot create
the policy. If neither exists, no mountpoint is needed or created.

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
`.e-agent/sessions/<name>.jsonl` in the workspace as an append-only log of
message and compaction entries. Without `--session`, each launch creates a
fresh unique session ID. History is restored for
model context on startup, while display projections are replayed in the TUI;
the model only sees the latest compaction summary and everything after it.
Legacy `.json` sessions are migrated on first load. `--version` (or `-V`) prints the exact
package build version without loading the workspace or configuration. Optional CLI overrides
are `--base-url URL`, `--model MODEL`, `--profile PROFILE`, `--workspace PATH`, `--session NAME`,
`--fork SESSION`, `--at N`, and `--max-rounds N` (tool-call rounds are unlimited by default;
the flag sets an explicit cap). `--read-only` applies the read-only role policy to the main
session (see "Read-only roles").
`--profile` selects a TOML profile and requires a TOML config. When config is
used, `--base-url` and `--model` are raw API wire-value overrides and take
precedence over the selected profile values.

### Forking a session

`e-agent --fork <session-id> [--at <n>] [PROMPT]` starts a brand-new session whose
history is a verbatim copy of the source session up to a completed turn. The
source session is never modified. The fork keeps the source's history prefix up
to (and including) the last completed turn — an assistant message with no
pending tool calls, or a compaction — and drops anything after it
(background-completion notices, for example). `--at <n>` picks the fork point
explicitly: `n` is 1-based and inclusive, and it must land on a turn boundary,
otherwise the fork fails rather than cutting mid-turn. Without a completed turn
in the source, the fork fails.

The new session starts with a `ForkedFrom` marker entry recording the source id
and fork point. The marker is display/audit only: it is rendered as a dim notice
in the TUI and is never sent to the provider on the model wire. The forked
session has fresh token accounting (usage counters start at zero) and is
resumed or forked again like any other session. The new id is printed as
`e-agent: forked session: <id>`; `--fork` is mutually exclusive with `--session`
and cannot be combined with `--at` without `--fork`. JSONL and GreptimeDB
backends behave equivalently.

Keys in the TUI: Esc always leaves the current view — it detaches from a
subagent view, closes the tasks panel, or (at the plain idle prompt) quits
the app; it never cancels a turn. Ctrl-C cancels the in-flight turn and quits
at the idle prompt. On exit, e-agent prints `e-agent --session <id>` so the
session can be resumed from the same workspace.

The TUI sets the terminal title (which Zellij adopts as its pane title) to
`e-agent — <title>`, where `<title>` is derived from the first real user
message in session history (trimmed, control-safe, ≤40 Unicode chars). An
empty/new session shows the session id until the first prompt is submitted.
The title is never persisted or used as a session key. Users who prefer an
explicit label can override it with `zellij action rename-pane
"<custom>"` at any time — Zellij's own title takes precedence.

The built-in local tool set always includes the capability-relative file tools
`read_file`, `write_file`, and `edit_file`, plus `bash`,
`get_background_tasks`, and `cancel_background_task`. Main-agent sessions also
register `delegate`; delegated subagents do not, which caps delegation depth at
1. `web_search` is registered for either kind of session only when
`EXA_API_KEY` is set to a non-whitespace value, or when `[web_search]` sets
`api_key_file` / `api_key_env` in the config (process env wins). It searches
public documentation and code examples through the Exa Context API. OpenCode's
own Exa configuration is not inherited. Configured local MCP servers may add
their tools to the main agent separately.
The three file tools use a capability-relative directory rooted at the
canonical workspace (the current directory by default). At startup, e-agent
loads a non-empty `AGENTS.md` from that workspace root into the system context
for both the main agent and delegated subagents. It does not search parent or
nested directories, and the instructions are not persisted in sessions.

Skills are loaded from two directories, merged by name, and injected into the
main agent's context (subagents do not inherit them):

- **Global skills**: `$XDG_CONFIG_HOME/e-agent/skills/<name>/SKILL.md` (typically
  `~/.config/e-agent/skills/...`), or `$HOME/.config/e-agent/skills/...` when
  `XDG_CONFIG_HOME` is unset — matching `Config::config_dir()`.
- **Workspace skills**: `.e-agent/skills/<name>/SKILL.md` relative to the
  canonical workspace root.

At startup, the main agent loads both directories. When a skill name exists in
both, the workspace version **fully replaces** the global one (no
concatenation). The merged set is sorted by `<name>` in dictionary order, and
each skill is injected as a `## Skill: <name>\n\n<content>` block — all skills
form a single context segment. The injection order is: main role template
(`agents/main.md`) → workspace `AGENTS.md` → skills → MCP server
instructions. Missing directories, empty directories, non-directory entries,
subdirectories without `SKILL.md`, and empty `SKILL.md` files are silently
skipped. Real I/O or UTF-8 errors on a `SKILL.md` that should be readable
produce an error with path context.
`read_file` pages long files by 1-indexed lines (optional `offset`/`limit`,
default 2000 lines per read, capped at 64 KiB) and prints a continuation hint
when more lines remain.
Tool calls and their results are displayed as they happen; the final answer
ends the turn.

## Safety boundaries

The workspace and configured external capabilities constrain only the agent's
file tools. Relative paths use the workspace; absolute paths require an
authorized canonical external root. Readable roots permit reads, while
writable roots permit read/write/edit. Exact-file roots do not grant authority
to siblings. Directory capabilities permit symlinks that remain inside their
root and reject symlink escapes. Exact-file capabilities use a durable handle
opened at startup: if the trusted host renames or replaces that pathname, file
tools still address the original inode, while Bash resolves and mounts the
pathname on each invocation. The trusted host should keep configured roots
stable for the run. Delegated custom workspaces must be canonical directories
inside the startup workspace or an authorized writable external directory;
read-only and exact-file capabilities cannot be rerooted. Capabilities do not
constrain hardlink inode origins or hardlink aliases to the protected project
policy file, do not make read/edit sequences atomic, and are not a process
sandbox.

`bash` runs `/bin/bash -lc` with the workspace as its current directory. By
default it is not sandboxed: commands can access files outside the workspace,
environment variables, and the network. On Unix, timed-out commands are killed
as a process group; on non-Unix platforms only the direct child is killed.
stdout and stderr are each captured up to 64 KiB, then drained and marked as
truncated.

Optional sandboxing is available when `bubblewrap` (`bwrap`) is installed and
`[sandbox] enabled = true` is set in the config. Bash mounts and file-tool
capabilities are independent boundaries that share the resolved path policy:
with bwrap disabled Bash retains ambient host access, while configured file
capabilities still apply; `workspace_writable` controls only the Bash mount.
Every `bash` call — main agent
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
concurrency limit. Each bash task starts a process; subagents run as independent
SessionRunner tasks on the shared Tokio runtime. Callers remain responsible for
resource use. Background bash has
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
self-contained task. Use it for subtasks whose intermediate steps (searching,
reading many files, focused edits) would clutter the main context.

Each subagent runs as an independent `SessionRunner` task on the shared Tokio
runtime with an empty history. Its isolated Agent state and event log are
exposed through a `SessionHandle`, while the parent's running-task registry
remains shared so nested background bash commands stay visible and report
completion to the parent. It gets the same builtins as its parent: file/bash
tools and, when
configured, `web_search`; no MCP tools and no `delegate` itself, so delegation
depth is capped at 1 by construction. Tool rounds are unlimited.

By default (`background` omitted or `true`) the subagent runs without blocking;
the immediate tool result includes both the background task number and subagent
session ID, and its completion includes that same session ID when delivered as
a background task completion (waking an idle agent). Pass `background: false`
for sync mode, which waits without a fixed time ceiling and returns the completed
answer with the subagent session ID directly. Sync cancellation, failure, or
closure returns an error beginning with `subagent session: <id>`.

A running background subagent can be watched live in the TUI: open the tasks
panel with F2, select it with Up/Down, and press Enter to attach. The
attached view replays everything the subagent has done so far and then
follows its stream (text, tool calls, results) with the same rendering as the
main view; the footer shows the task and whether it is still running. Esc
detaches back to the main view (it never cancels the main turn), and the
subagent keeps running after detach. While attached, prompts can be queued and
an in-flight turn can be cancelled; bash tasks have no event stream and cannot
be attached.

## Compaction

Use `/compact` in the TUI or `--repl` to manually summarize earlier history.
The agent summarizes everything before the current turn and appends a
compaction entry (summary plus the retained turn) to the append-only session
log; earlier messages stay persisted and visible in the TUI. Subsequent model
calls use the latest summary plus everything after it. When a configured model
profile provides a context window, the agent also compacts automatically at
80% usage.

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
- `RUST_BACKTRACE=1` — optional; appends a backtrace to ordinary `anyhow` error
  chains (and enables their full Rust verbosity). Rust panics always print a
  forced stack regardless of this setting.

### Panic reports

A Rust panic prints its thread, location, and forced stack to stderr while
omitting the panic payload. It also writes a private crash report to
`$XDG_STATE_HOME/e-agent/crash/latest.log`, or, when `XDG_STATE_HOME` is unset,
`~/.config/e-agent/crash/latest.log`. On the next startup, e-agent reports the
previous crash and renames `latest.log` to `previous.log`.

The global panic hook also observes Tokio worker and subagent task panics. Such
a panic may be caught by Tokio as a `JoinError`, so its diagnostic and report do
not by themselves mean the main TUI exited or that the application is fatal.

Panic reports are not guaranteed for `SIGKILL`, OOM termination, aborts,
segmentation faults, or a top-level `Err` return; those are not ordinary Rust
panics.

TOML config is optional. When present, it supplies the selected provider
credential and profile values instead of `OPENAI_*`; see [Run](#run). A
provider using `api_key_env` reads the variable named by that field (for
example, `KIMI_API_KEY`).

## Image input (multimodal)

Two entrances attach images to the conversation, both stored in a global
content-addressed cache (files named by SHA-256 hex, deduplicated across
sessions) at `$XDG_STATE_HOME/e-agent/images/`, falling back to
`~/.config/e-agent/images/` — the same base the crash directory uses:

- **`read_image` tool** (agent entrance): the model reads an image file
  (png/jpeg/webp/gif, up to 10 MiB; capability-path policy like `read_file`).
  The bytes are stored once under their hash; the tool result carries a
  structured marker that the runner strips into a text summary, then attaches
  a synthetic user message carrying the image reference. Base64 never enters
  the scrollback or the session file.
- **REPL `/image <path>`** (human entrance, REPL mode only): attaches the
  image to the next prompt; the placeholder line
  `[image attached: <path>]` is included in the prompt text.

Sessions persist only `ImageRef { hash, mime }` references — never base64.
At send time the wire re-reads the file and builds the provider format:
`image_url` parts (an object) for OpenAI-compatible chat, `input_image` parts
(a bare-string `image_url`) for ChatGPT Responses. A missing cache file
degrades to a `[image missing: <hash>]` text placeholder instead of failing.

A model only receives images when its profile sets `vision = true` (see
[Run](#run)). A user message with images on a non-vision model fails with a
clear `model X does not support image input` error before any request is sent.

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
- `event_time` is wall-clock time with microsecond precision.  Within a
  single process, the monotonic `next_event_time_us` atomic guarantees strict
  ordering (same-µs or backward clock → prev+1).  Dedup: per seq, only rows
  with the latest `event_time` are retained; at the same max `event_time`,
  identical deserialised `SessionEntry` values are folded and divergent ones
  cause a hard error. Canonical conversation order is the winning
  `event_time ASC`, with `seq ASC` only as a tie-breaker; seq remains identity
  and continuity metadata.
- No transactions — atomicity is per multi-row INSERT statement
- Subagents each open their own database connection (bounded, fine for
  single-user agents)

### Query patterns

| Operation | Query | Optimization |
|-----------|-------|-------------|
| connect (find max seq) | `SELECT MAX(seq) ... WHERE workspace_id=$1 AND session_id=$2` | Full partition scan (acceptable, sessions are bounded) |
| load (all entries) | `SELECT seq, event_time, payload ... ORDER BY event_time ASC, seq ASC` | App-side cross-version dedup (latest event_time per seq), then canonical winning `event_time ASC, seq ASC` order |
| append | Parameterized multi-row INSERT | Single statement, all-or-nothing per chunk |

### Import tool: `e-agent-import-jsonl`

A standalone binary for manual, incremental JSONL-to-GreptimeDB import. Only
appends entries that do not yet exist, and refuses to write when the existing
DB prefix diverges from the JSONL file. Re-runnable under idle target + strict
prefix conditions.  Duplicates (identical-deserialised-`SessionEntry` retries
at the same max event_time) are folded; divergent duplicates at the same
event_time are rejected.

#### Build

```sh
cargo build --features greptime --bin e-agent-import-jsonl
```

#### Usage

```sh
e-agent-import-jsonl --session <SESSION_ID> [--workspace <PATH>] [--conn <CONN>] \
    [--dry-run]
```

- `--session` (required) — session name matching `[a-zA-Z0-9_-]+`.
- `--workspace` (default: current directory) — workspace root; canonicalised
  and used to derive the `workspace_id` that scopes the session.
- `--conn` (default: from config `[session] backend = "greptime"`) — pg-wire
  connection string (e.g. `"host=127.0.0.1 port=4002 dbname=public"`).
- `--dry-run` — print the range that *would* be appended without inserting rows.
  NOTE: still connects to DB, which may execute CREATE TABLE.

#### Safety

1. The tool requires the JSONL file at exactly `.e-agent/sessions/<id>.jsonl`
   to exist (empty files are valid). Legacy `.json` format is rejected.
2. The DB's seq values are verified to be strictly 0..N continuous before
   planning (gaps or divergent-deserialised-`SessionEntry` duplicates cause a
   hard error; identical deserialised entries are silently folded).
3. If the DB has **more** entries than the JSONL file, the tool errors out
   without writing — it will never truncate.
4. If the existing DB prefix **differs** from the same-length prefix of the
   JSONL file, the tool reports the first divergent sequence number and
   refuses to write.
5. When the prefix matches, the tool re-reads the DB immediately before
   writing (TOCTOU mitigation) to detect concurrent changes. If the DB
   changed, the tool errors out.
6. After writing, the tool reloads the DB and verifies it matches the JSONL
   file exactly by length and per-entry content. Verification failure is
   reported as an error.
7. **Not safe for concurrent writers**: GreptimeDB has no transactions, so a
   concurrent writer between the pre-write check and the INSERT can still
   race. Ensure the target session is **idle** during import.
8. **Partial commits**: chunks of >9000 entries use separate INSERT
   statements. If the first chunk commits and the second fails, some entries
   are written. Fix the issue (e.g. network, JSONL source) and re-run.
9. **Duplicate handling**: within a seq, older `event_time` rows are
   silently overwritten by the latest `event_time` row. If the latest
   `event_time` has multiple physical rows (same microsecond), their
   payloads are deserialised and compared as `SessionEntry` values.
   Identical deserialised entries are folded silently; divergent entries
   cause a hard error — stop writers, inspect with SQL, and resolve
   manually.



#### Example

```sh
# Dry-run to see what would be imported
e-agent-import-jsonl --session 20250331-120000-abc3 --dry-run

# Real import (normal append, prefix must match)
e-agent-import-jsonl --session 20250331-120000-abc3

```

### Tests

Integration tests require a live GreptimeDB and opt-in via the `GREPTIME_PG`
environment variable:

```sh
GREPTIME_PG="host=127.0.0.1 port=4002 dbname=public" cargo test --features greptime
```

Without `GREPTIME_PG` the integration tests skip with a message.
## Non-goals

This is deliberately not a daemon, JSONL protocol, subagent
framework, permission framework, plugin host, generic provider/auth framework,
parallel tool executor, task scheduler, priority system, or concurrency pool.
It deliberately does not fetch provider/model catalogs (including
models.dev), generate configuration, cache provider metadata, or infer
context windows; TOML profiles are local static settings only. `AGENTS.md`
loading is workspace-root-only: there is no parent/nested discovery or merging.
Subagents exist but are deliberately minimal: no agent-to-agent messaging,
no delegation deeper than 1 level, and no process-level isolation yet
(subagents are runtime tasks, not subprocesses).
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
Workspace skills are deliberately flat `<skills-dir>/<name>/SKILL.md` only
(global skills from `Config::config_dir()/skills/`, workspace skills from
`.e-agent/skills/`): no YAML/front matter parsing, remote installation,
registry, dependencies, hot reload, slash commands, on-demand loading, config
toggle, or recursive skill discovery. Skills are main-agent-only and are not
inherited by subagents. The same-name override is a full replacement, not a
merge or concatenation.

GreptimeDB-specific non-goals (when built with `--features greptime`):

- No automatic migration of existing JSONL sessions — existing JSONL sessions
  are not migrated to GreptimeDB at startup; use `e-agent-import-jsonl` for
  manual incremental import
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
