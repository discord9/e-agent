# e-agent

e-agent is a Rust coding agent with async subagents, goal-driven continuous
execution, and terminal and web UIs. It is a small, single-crate program that
streams requests to OpenAI-compatible `/chat/completions` APIs or the ChatGPT
Codex `/responses` API, executes the model's requested workspace tools one at
a time, returns results to the model, and prints its final answer.

Use it as a one-shot CLI command, an interactive terminal UI (the default with
no prompt in a terminal), or a line-based REPL. `e-agent web` starts the local
web UI and HTTP API. SQLite is the default session backend; JSONL and
GreptimeDB are selectable in `[session]` (see
[SQLite session backend](#sqlite-session-backend)). The built-in tools cover
workspace files, persistent notes, shell and background work, and
main-session delegation. Background and subagent completion notifications are
delivered to the session when they finish. A human can explicitly arm goal
continuation with `/goal continue [N]`; active goals and restarts never arm it.
See [Session goals](#session-goals) for the full behavior. Local stdio MCP
servers can optionally add tools.

## Quick start

Install a Rust toolchain with Cargo, configure a provider in [Run](#run), then
from this repository checkout run one of these commands, replacing
`/path/to/workspace` with the workspace the agent should use:

```sh
cargo run -- --workspace /path/to/workspace "inspect src/main.rs and explain it" # one prompt
cargo run -- --workspace /path/to/workspace --repl                                # line-based REPL
cargo run -- --workspace /path/to/workspace web                                   # web UI at http://127.0.0.1:8766
```

With no prompt, `cargo run -- --workspace /path/to/workspace` opens the terminal
UI when stdout is a terminal.

The toolchain is pinned in `rust-toolchain.toml` (1.99.0); an inherited
`RUSTUP_TOOLCHAIN` overrides it, so unset it if the build picks the wrong
compiler.

## Run

Create `$XDG_CONFIG_HOME/e-agent/config.toml` (falling back to
`~/.config/e-agent/config.toml`):

```toml
default = "kimi/k3"

[providers.kimi]
base_url = "https://api.kimi.com/coding/v1"
api_key_file = "kimi-key"          # or api_key_env = "KIMI_API_KEY"

[models."kimi/k3"]
model = "k3"
reasoning_effort = "high"          # provider-specific pass-through
vision = true                      # set only for image-capable models
```

A provider needs exactly one of `api_key_file` or `api_key_env`; with neither it
is treated as an unauthenticated local endpoint (vLLM, Ollama, llama.cpp) and
requests omit `Authorization`. Provider names are inferred from the prefix
before `/`. `deepseek_compat = true` opts a profile into the DeepSeek Chat
thinking-mode wire (persisted reasoning is replayed on tool-call turns);
everything else keeps the standard wire where reasoning is display-only.

The keys most configurations touch:

| Key | Meaning |
|---|---|
| `default` | profile used when none is given |
| `[providers.<name>].base_url` | API root, e.g. `https://api.openai.com/v1` |
| `...api_key_file` / `api_key_env` | credential source, exactly one |
| `[models."<p>/<n>"].model` | wire model name |
| `reasoning_effort` | optional pass-through (`low`/`high`/`max` are common) |
| `thinking` | enable provider thinking mode where supported |
| `context_window` | token budget used for compaction |
| `vision` | allow image input for this profile |
| `deepseek_compat` | DeepSeek Chat reasoning-replay contract |
| `[sandbox].enabled` | run bash inside bubblewrap |
| `[sandbox].network` / `workspace_writable` | defaults for that sandbox |
| `[sandbox].writable_paths` / `readable_paths` | extra mounts |
| `[session].backend` | `sqlite` (default), `jsonl`, or `greptime` |
| `[roles]` | role → model profile routing |
| `[bash].timeout_secs`, `[background].timeout_secs` | command timeouts |
| `[images].max_inline` | distinct inline images per request (default 6, `0` = never offload) |

A workspace-level `.e-agent/config.toml` may add or override models, roles, MCP
servers, `[bash]`, and `[background]`; it cannot define providers or the session
backend. `--model`, `--base-url`, and `--profile` override the file for one run.

## Safety boundaries

The workspace and configured external capabilities constrain only the agent's
file tools, and they are not a process sandbox. Relative paths use the
workspace; absolute paths require an authorized external root, matched by the
configured logical destination (the canonical source backs the capability; the
input path is never canonicalized). Reads need a readable root, writes and edits
need a writable one, and a read-only winner rejects a writable operation outright
rather than falling back to a broader writable root while an exact read-only file
denies only itself (see the sandbox policy rules above). Directory capabilities
permit symlinks that stay inside their root and reject symlink escapes.
Exact-file capabilities use a durable handle opened at startup, so file tools
keep addressing the original inode if the trusted host renames or replaces that
pathname (Bash resolves the pathname on each invocation) — keep configured roots
stable for the run. Delegated custom workspaces must be canonical directories
inside the startup workspace or an authorized writable external directory;
read-only and exact-file capabilities cannot be rerooted, and configured alias
destinations never extend reroot. Capabilities do not constrain hardlink inode
origins or hardlink aliases to the protected project policy file, do not make
read/edit sequences atomic, and do not isolate the agent process.

`bash` runs `/bin/bash -lc` with the workspace as its current directory. By
default it is not sandboxed: commands can access files outside the workspace,
environment variables, and the network. On Unix, timed-out commands are killed
as a process group; on non-Unix platforms only the direct child is killed.
stdout and stderr are each captured up to 64 KiB, then drained and marked as
truncated.

`web_fetch` (the default `fetch_provider = "local"`) performs its HTTP GET in
this process, so it has exactly the same network reach as `bash`. There is no
public-only destination filter: filtering one tool while the shell it sits next
to can reach the same hosts is theatre, so the fetch reaches whatever the
process reaches.

Optional shell restriction is available when `[sandbox] enabled = true` is set.
On Linux/macOS this uses `bubblewrap` (`bwrap`) as described below. On Windows,
the **write-sandbox MVP is an accident-prevention mechanism**, using a restricted
primary token and stable synthetic capability ACEs. It restricts writes to the
workspace (only when `workspace_writable = true`) plus explicit
`writable_paths`; `workspace_writable = false` grants no workspace write ACE,
and `readable_paths` never grants write access. TEMP/TMP, HOME, Cargo/NuGet, and
engine caches are not implicitly writable.

The Windows MVP is not complete filesystem isolation: it does not restrict
reads or network access, and `network = false` is rejected before launch. Except
for the explicitly stripped credential variables (`EXA_API_KEY`,
`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `DEEPSEEK_API_KEY`, `MOONSHOT_API_KEY`,
and `KIMI_API_KEY`), parent-process environment variables remain visible to the
child process; this is not read or secret isolation. Locations already writable
through Everyone or the current logon SID, including some public locations, may
remain writable.

Only existing canonical directory paths on fixed local NTFS volumes are
accepted: UNC/device paths, non-directory roots, symlinks/reparse points, NULL
DACLs, and case-sensitive roots are rejected. The first run (or a versioned ACE
upgrade) of each write root scans its tree without following links; later
commands skip that scan because the exact current-version ACE is installed. The
ACE grants create/overwrite plus child rename/delete/atomic replace through
`FILE_DELETE_CHILD` inherited by directories, without granting `DELETE` on the
write root itself. Hard-linked file descendants whose aliases all stay inside
the write roots are allowed — this is what makes cargo's `link_or_copy` work
with a narrowed `~/.cargo/registry` write root instead of the broader
`~/.cargo` — while a hardlink that aliases a file outside the write roots is
rejected with every alias path listed, so align the write roots with the
locations the build actually shares. Nested symlink/reparse point descendants
are unsupported and rejected. Concurrent path replacement races (TOCTOU) remain
a risk.

Consequences: synthetic capability allow ACEs persist after successful
execution, and a failure on a later root leaves the earlier ones in place (the
process is not started and no whole-DACL rollback is attempted); these SID ACEs
are inert for ordinary tokens, but administrators may need to remove them
manually. Cancellation, timeout, and running-task registry teardown terminate
only the top-level process, so descendants may keep running with the capability
and keep writing the allowed roots; Job Object assignment with process-tree
termination is a future enhancement. Windows shell execution with protected Git
metadata (`protect_git = true`, used by delegated subagents/fixers) is
unsupported and fails closed before token or ACL changes with actionable
guidance in the error message; there is no `.git` ACL carve-out, and a role can
opt out with `protect_git = false` in its frontmatter (see "Role frontmatter:
`protect_git`" above), restoring a working shell for that role.

On Linux/macOS every `bash` call—main agent and subagents alike—is wrapped in
`bwrap`: system directories are mounted read-only, the workspace is mounted
read-write (`workspace_writable = false` makes it read-only), `/tmp` is the
host's real `/tmp` (bound writable for main-agent and subagent Bash alike),
`/home` is a fresh tmpfs, PID/IPC/UTS namespaces are unshared, and TIOCSTI is
blocked via `--new-session`. Network stays available by default; `network = false` unshares
it. When the host uses systemd-resolved, the stub resolver at
`/run/systemd/resolve` is mounted read-only so DNS resolution via the symlinked
`/etc/resolv.conf` works inside the sandbox. `gpu = true` (off by default and
global-config only) additionally exposes the host GPU device nodes and the
AMD ROCm sysfs subtrees described under `gpu` in the sandbox config; that
enlarges the kernel/driver attack surface and shares GPUs — a resource with no
per-command isolation — with model-generated code. The restriction constrains
the spawned command, not the agent process itself. It is best-effort: bwrap is
not setuid, the host network is shared by default, and environment variables of
the parent process other than the stripped credential names listed above remain
visible.

On Linux/Android, the workspace `.git` entry depends on the role: the main agent
leaves it writable for repository operations, while delegated subagents/fixers
bind it read-only (for a linked worktree, the `.git` pointer plus the
caller-authorized main-worktree metadata and administrative paths, with
writable aliases rejected) and normal workspace files stay writable. This does
not authorize other repositories or external object stores. Background commands
inherit the parent's role.

### Code mode: `run_rust` (experimental)

`[code_mode] enabled = true` in the user-level config registers a main-agent-only
tool that compiles and runs one model-supplied Rust source (edition 2021, std
only, ≤ 128 KiB) inside a forced bubblewrap sandbox. It follows the resolved
`[sandbox]` policy and needs a factory restart after a policy change.

## Background tasks

The `bash` tool accepts `background: true` to run a command without blocking.
Bash tasks and subagents share one running-task registry with no built-in
concurrency limit; callers remain responsible for resource use. Background bash
has the same workspace cwd, process-group handling, and per-stream 64 KiB output
limit as foreground Bash, with a 30-minute timeout. Completions surface as a TUI
notification while a turn is active, and the complete output is injected as a
clearly labelled user message at the next model call — whether that is the next
round of an active turn or, if the agent is idle, a new turn started by that
delivery. F2 toggles a tasks panel listing what is running. In one-shot mode a
still-running task is reported before exit; the process then best-effort
terminates its process group. Uninjected background results are not persisted in
sessions.

A `FinishWhenIdle` session (a delegated subagent, or a one-shot CLI session)
that finished its work waits indefinitely for its blocking background tasks
before finalizing. Each completion is injected and acknowledged, allowing the
model to continue in a follow-up turn. Detached tasks do not block finalization
and may outlive the session.

The durable background-task table is a live view of the tasks this session
still owns, not an execution history. Detached tasks are excluded. After a
restart, leftover rows are reported as killed with the process and then cleared.
Running the same session in two processes at once can cause false killed reports
and overwrite task records. Task records can lag behind running work when saving
fails, and a crash during cleanup can leave a finished task reported as killed
on the next start.

## Subagents

The `delegate` tool spawns a subagent with a fresh context to work on a
self-contained task. Use it for subtasks whose intermediate steps (searching,
reading many files, focused edits) would clutter the main context.

Each subagent runs as an independent background task with an empty history and
its own isolated agent state and event log; the parent's running-task registry
stays shared, so nested background bash commands remain visible and report
completion to the parent. It gets the same builtins as its parent — file and
bash tools plus, when configured, web search — but no MCP tools and no
`delegate` itself, so delegation depth is capped at 1 by construction. Tool
rounds are unlimited.

`fork: true` is the one explicit exception to that empty history. The child
starts from the parent's **completed-turn prefix** (the in-flight turn is
excluded, so put everything the child needs in `task`) and reads that prefix
back from the parent session, so spawning a fork does not copy the parent
history. It runs on the parent's **current** model, inheriting the parent's
system prompt and MCP tool set, and the parent's goal is frozen into its
request. A fork does not expose the parent's main-only tools, so its tool set
matches the parent's only when the parent exposed no such tool. `workspace` is
optional and defaults to the parent root: with that default and an identical
tool set the child's request reuses the parent's provider-side prompt cache; an
explicit different `workspace` only moves the child's file/bash tools and MCP
connections there, so the prefix and cache hit are no longer guaranteed. A fork
still cannot delegate further or ask the user questions. Constraints: `fork` is
mutually exclusive with `role` and `resume`, and the parent must have a
completed turn (a mid-turn compaction is rejected). This is the only case where
a subagent gets MCP tools.

Subagents always run in the background without blocking the main agent; there
is no foreground mode. The immediate tool result includes both the background
task number and subagent session ID, and its completion includes that same
session ID when delivered as a background task completion (waking an idle
agent). Legacy calls passing a `background` parameter are rejected.

A live parent and one of its currently registered child sessions may use
`send_message` for direct progress messages: the parent targets the child's
exact session ID and a child targets only `parent`. The receipt means queued,
not delivered. Endpoints and unconsumed queues are process-local and are not
durable or recoverable after restart; only a successfully committed Notice
survives replay, and replay never wakes a model turn. Messages cannot target
siblings, other main sessions, or finished sessions — a finished child is no
longer live; continue it with the `delegate` tool (`resume` its session id
with a fresh `task`, plus the original `role`/`workspace` if the task used
them). The `send_message` error message says the same when a target is not a
live child. Hard abort/restart, or a
persistence/consumption failure, can discard an accepted queued body; there is
no retry.

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
The agent summarizes everything before a bounded tail of the most recent
messages and appends a compaction entry (summary plus that retained tail) to
the append-only session log; earlier messages stay persisted and visible in the
TUI. The cut never splits a tool batch, and in a zero-copy fork the tail is
clamped to the inherited boundary so parent messages are only ever summarized
(see "Forking a session"). Subsequent model calls use the latest summary plus
everything after it. When a configured model profile provides a context window,
the agent also compacts automatically at 80% usage.

## Session goals

`/goal set <text>` records an objective; `/goal continue [N]` explicitly arms
N follow-up turns, and `/goal pause` / `/goal clear` stop it. Active goals never
arm continuation by themselves.

## Session output receipts (`eout1`) and `read_output`

Long tool output is stored once and referenced by an `eout1` receipt; the model
pages it with `read_output` instead of re-reading the transcript.

## History tool

`history` searches and reads past sessions, current session entries, and tool
calls through the session store, scoped to the workspace.

## Web UI / headless server

`e-agent --serve` (alias: `e-agent web`) starts a headless HTTP server that
serves the web UI at `/` and a token-authenticated `/api/*` surface for
managing live sessions. It binds `127.0.0.1:8766` by default; `--host` and
`--port` (or `-p`) override:

```sh
e-agent web                       # http://127.0.0.1:8766
e-agent --serve --port 9000       # http://127.0.0.1:9000
```

Every API route requires the server token as `Authorization: Bearer <token>`
(or `?token=<token>`, the query fallback for EventSource-style clients),
except the public read-only `GET /api/usage/dashboard`. That GET deliberately
ignores missing or invalid Authorization; it accepts no token in its browser
client. The token is generated once at first start — 32 random bytes,
base64url, written with mode 0600 to
`$XDG_STATE_HOME/e-agent/server.token` (falling back to
`~/.local/state/e-agent/server.token` when `XDG_STATE_HOME` is unset) — and
reused across restarts so browser clients keep working. The startup log
prints the token and the token file path.

The `/api` surface (JSON except for the SSE endpoint):

| Method | Path | Semantics |
|--------|------|-----------|
| GET | `/api/sessions` | list sessions |
| POST | `/api/sessions` | create a session |
| GET | `/api/sessions/{id}/events` | SSE: snapshot, then live events |
| GET | `/api/sessions/{id}/history` | segmented history (head or older) |
| GET | `/api/sessions/{id}/summary` | per-turn summary cache (desktop pet) |
| POST | `/api/sessions/{id}/prompt` | queue a prompt |
| POST | `/api/sessions/{id}/btw` | fork into a persistent subagent |
| GET | `/api/sessions/{id}/fork-candidates` | turn boundaries to fork at |
| POST | `/api/sessions/{id}/fork` | fork at a turn boundary into a `fork-…` session |
| POST | `/api/sessions/{id}/cancel` | release: preempt the in-flight turn without terminating the session (with no queued messages a WaitForInput session returns to Idle and stays usable) |
| POST | `/api/sessions/{id}/compact` | request compaction |
| GET | `/api/models` | switchable model profile names |
| GET | `/api/pet/config` | live desktop pet sprite settings |
| GET | `/api/pet/sprite` | configured local sprite-sheet bytes |
| POST | `/api/sessions/{id}/model` | switch the session's model at runtime |
| POST | `/api/sessions/{id}/undo` | undo the most recent file operation |
| PUT | `/api/sessions/{id}/title` | rename a session |
| PUT | `/api/sessions/{id}/pin` | pin a session |
| PUT | `/api/sessions/{id}/archive` | archive a session |
| DELETE | `/api/sessions/{id}` | cancel + remove from the registry |
| GET | `/api/usage/dashboard` | **public** read-only usage JSON (SQLite/Greptime; JSONL returns 503) |
| GET | `/api/provider-quotas` | account balance/quota cards for configured providers (authenticated) |
| GET | `/api/tasks` | running background tasks, all sessions |
| DELETE | `/api/sessions/{id}/tasks/{task_id}` | cancel one background task |
| GET | `/api/sessions/{id}/tasks/{task_id}/output` | full output of a running bash task |

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

A project-level `<workspace>/.e-agent/config.toml` `[mcp]` section overlays
these servers by name: a project server replaces the same-named global
server, and servers only the global config defines keep their definitions.
Because each entry spawns `command`, both the global and the project config
are trusted inputs — only open workspaces you trust, since opening a repo
executes the commands its `.e-agent/config.toml` declares. A global
`[mcp."<name>"] enabled = false` is a kill switch a project file cannot
re-enable: the merged config keeps the disabled global entry, so a
same-named project server is skipped (`connect_all` skips disabled servers).
Project MCP servers apply to the main session and to explicit `fork: true`
delegates, which reconnect the parent's project servers in the child's own
workspace (a server that fails to start is skipped, exactly as for a main
session). Ordinary subagents do not connect MCP.

Servers are connected one at a time at startup; a server that fails to start is
skipped with a warning and does not abort the session. Server-provided
instructions (from the MCP `initialize` response) are prepended to the model
context as a system message on every call, but are not persisted in sessions.
Only tools are supported — no resources, prompts, or server-initiated
notifications. MCP tools are sorted by name, so the tool array a provider sees is
stable across runs and a forked child can reuse the parent's prompt cache.

## Environment

- `OPENAI_API_KEY` — required in environment-variable mode.
- `OPENAI_BASE_URL` — API base URL, default: `https://api.openai.com/v1`.
- `OPENAI_MODEL` — model name, default: `gpt-4o-mini`.
- `EXA_API_KEY` — optional; a non-whitespace value enables `web_search` through
  Exa Context. `[web_search] provider = "searxng"` needs no environment
  variable: its `base_url` comes from the config file only. See
  [Web tools](#web-tools).
- `RUST_BACKTRACE=1` — optional; appends a backtrace to ordinary `anyhow` error
  chains (and enables their full Rust verbosity). Rust panics always print a
  forced stack regardless of this setting.
- `RUST_LOG` — optional `tracing` filter for operational diagnostics; defaults
  to `e_agent=info` (for example, `RUST_LOG=e_agent=debug`). Logs are compact,
  human-readable, and written to stderr without ANSI escapes.

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
credential and profile values instead of `OPENAI_*`; see [Run](#run).

## Web tools

`web_search` and `web_fetch` are registered together as soon as one web-search
source is usable: `EXA_API_KEY`, `[web_search] provider = "searxng"` plus a
`base_url`, or `provider = "deepseek"` plus a credential. With no usable source
neither tool exists, and a blank credential registers nothing.

`web_fetch` is a single-URL fetch performed by this process: it GETs the URL
over the network this process already has, follows at most 5 redirects, and
accepts `text/html`, `application/xhtml+xml`, `text/plain` and `text/markdown`
bodies up to 2 MiB. Bytes are decoded with the `Content-Type` charset, else a
`<meta charset>` sniff, else UTF-8 (lossy, so GBK/Big5 pages are readable).
HTML goes through Mozilla Readability (`dom_smoothie`) and comes back as the
article title plus Markdown; a page with no readable article is an error, not
a dump of tags and navigation. `text/plain` and `text/markdown` are returned
as they are. It never follows links found in the page, so it is not a crawler.
Because it runs in this process it reaches whatever the process reaches — the
same network `bash` has, not only public hosts (see
[Safety boundaries](#safety-boundaries)).

`[web_search] fetch_provider` selects the fetch transport:

```toml
[web_search]
provider = "searxng"
base_url = "http://127.0.0.1:8088"
fetch_provider = "deepseek"      # optional: "local" (default) or "deepseek"
api_key_env = "DEEPSEEK_API_KEY" # allowed here only because the fetch needs it
```

`"local"` (the default when the field is absent) uses this process.
`fetch_provider = "deepseek"` keeps the remote `tools/fetch` — DeepSeek's
servers do the fetching, so a page they cannot reach fails — and requires a
credential. The two choices are independent: a SearXNG search can pair with a
DeepSeek fetch. `provider` still rejects a `base_url` for `"deepseek"`, and
`api_key_file` / `api_key_env` are rejected for a SearXNG search unless
`fetch_provider = "deepseek"` needs them.

## Image input (multimodal)

`read_image` (tool) and the REPL `/image <path>` command attach png/jpeg/webp/gif
images up to 10 MiB; bytes are stored once in a content-addressed cache under
`$XDG_STATE_HOME/e-agent/images/`. Sessions persist only the reference, never
base64, and the wire re-reads the file when sending. The profile needs
`vision = true`, otherwise the turn fails with a model-does-not-support-images
error. `generate_image` (main session only, ChatGPT login) writes a PNG.

### Image budget

Both wires (chat and ChatGPT Responses) put at most **1024 estimated vision
tokens per image** on the wire. Each outgoing image is measured with DeepSeek's
published calculator — 14px patches, merged 3:1 per axis into 42px token cells,
`tokens = grid_h * (grid_w + 1) + 2` — and scaled **down** (never up) until the
estimate fits, keeping its aspect ratio in whole 14px patches. The provider's
own 544x544 total-pixel floor is its accounting only: a small image is sent
unchanged. The re-encoded variant is cached next to the original as
`<hash>.n<WxH>.<ext>` (JPEG quality 85 for opaque images, PNG when the source
has alpha) and reused on later requests; an image that already fits is sent
byte-identical with its original MIME type and is never re-encoded.

```toml
[images]
max_inline = 6      # distinct images kept inline per request; 0 = never offload
```

When one request would carry more than `max_inline` distinct images, the oldest
by first appearance in request order are replaced by a text part in place of the
image part:

```text
[image omitted: 4 older image(s); read again with read_image (hash <sha256>)]
```

The placeholder names the stored hash — the file name in the image store, which
`read_image` can read again when the store is an authorized external root — so
nothing is silently lost, only deferred. Offloading applies to user and tool
images (the same messages the image-rejection classifier walks), and the newest
`max_inline` are always kept. Because the projection is pure (same messages in,
same request bytes out), it is deterministic, but replacing an old image part
with text changes every byte after it: **offloading costs prompt-cache reuse
from the first changed message onward**.

## SQLite session backend

The default backend stores sessions in `<workspace>/.e-agent/sessions.db`
(override with `[session] path`, or select `backend = "jsonl"`).

## GreptimeDB session backend (experimental)

`[session] backend = "greptime"` and `conn = "host=… port=… dbname=…"` store
sessions in GreptimeDB instead; the server must be running before e-agent
starts.

## Non-goals

- No daemon, JSONL protocol, subagent framework, permission framework, plugin
  host, generic provider/auth framework, parallel tool executor, task
  scheduler, priority system, or concurrency pool.
- No provider/model catalog fetching, config generation, metadata caching, or
  context-window inference; TOML profiles are static local settings.
- `AGENTS.md` loading is workspace-root only: no parent or nested discovery.
- Subagents are runtime tasks, not processes: no agent-to-agent messaging
  beyond live parent/child `send_message`, no delegation past one level.
- Cancellation is a release, not a termination.
- No idle-runner GC, no shell-command classifier, and no forced turn ending for
  background waits.
- MCP is local stdio servers only; no remote HTTP/SSE, OAuth, resources,
  prompts, or server restart.
- Image offload is positional only: no retention/expiry by wall-clock time, no
  "recent turns" heuristic, no automatic re-read on offload, and no summariser.
- The web server has no multi-user model: one token, one workspace per process.
- Windows support is narrower than Linux (see the sandbox notes above).
