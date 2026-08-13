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
workspace file winning. This repository includes six role templates —
`designer`, `explorer`, `fixer`, `main`, `oracle`, and `seer`; unrouted named
roles fall back to the default subagent model, which itself falls back to the
main profile:

```toml
[models."kimi/k2"]
model = "k2"

[roles]
subagent = "kimi/k2"
```

A project-local `<workspace>/.e-agent/config.toml` overlays `default`,
`[models]`, `[roles]`, `[mcp]`, and `[tui]` onto this global config: models
merge by name (a project model replaces the same-named global model; global
models the project does not define keep their definitions; an absent or empty
`[models]` keeps all global models), roles merge per key, and MCP servers
merge by name the same way (a project server replaces the same-named global
server; servers the project does not define survive). A project `default`
replaces the global default profile when present; no project `default` keeps
the global one, and it participates in the normal priority chain — an
explicit `--profile` wins over it, and it wins over `[roles] main`. The
project file cannot change `[providers]`, `[web_search]`, or `[session]`, and
it only takes effect when a global config exists (it is an override layer,
not a standalone config). `[sandbox]`, `[background]`, `[bash]`, and
`[delegate]` project overrides are described in the relevant sections
below.

The project file is strict: unknown sections (a typo, or a section the
project file cannot carry) are a startup error naming the offending field
instead of being silently ignored. And since a project `[mcp]` server starts
the command it configures, `<workspace>/.e-agent/config.toml` is trusted
input — treat it like the global config: only open workspaces you trust,
because opening a workspace runs the MCP commands that workspace's file
declares. A global `[mcp."<name>"] enabled = false` is a kill switch that a
project file cannot re-enable (see the MCP section).

### TUI submit/newline keys

The TUI's input boxes accept a limited set of Enter variants configured in
the global config under `[tui]` — e.g. swap the roles so Enter inserts a
newline and Option+Enter submits:

```toml
[tui]
submit = "alt+enter"   # default "enter"
newline = "enter"      # default "alt+enter"
```

Each field accepts exactly one of `enter`, `alt+enter`, `option+enter` (the
macOS alias for `alt+enter`), `ctrl+enter`, or `shift+enter`, matched
literally. `submit` must differ from `newline`; an unsupported key string or
a `submit == newline` collision is a startup error listing the supported
keys (no silent fallback). A project `[tui]` section replaces the global one
wholesale — fields the project omits fall back to the built-in defaults, not
to the global values; no project `[tui]` keeps the global mapping (see the
project-config paragraph above).

**Terminal caveat:** `shift+enter` is unreliable — most terminals do not
report the Shift modifier and send a bare Enter instead, so a
`shift+enter` binding behaves like `enter` there. Prefer `alt+enter` (or
`option+enter`) for the secondary key; it is the only modifier most
terminals report alongside Enter.

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

### Role frontmatter: `protect_git`

Subagent/fixer `bash` protects the workspace `.git` metadata by default
(`protect_git = true`): on Linux/macOS the sandbox binds `<workspace>/.git`
read-only so a delegated agent cannot delete or corrupt the repository (the
main agent leaves `.git` writable so it can orchestrate git operations). The
same frontmatter block can opt a role out with `protect_git = false`:

```markdown
---
protect_git = false
---
Fix the code and run the checks.
```

This matters on Windows: the write-sandbox MVP has no way to make `.git`
read-only, so when `protect_git = true` it fails closed — every shell command
in a delegated subagent/fixer is rejected before execution with guidance in
the error message. Setting `protect_git = false` in the role's frontmatter
(and accepting that `.git` stays writable inside the sandbox) restores a
working shell for that role. The key defaults to `true` when omitted, so
Linux/macOS behavior is unchanged; only roles that explicitly opt out lose the
`.git` protection.

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

When the sandbox is **enabled**, the workspace itself becomes a logical policy
entry for the file tools: with `workspace_writable = false` the file tools
deny writes/edits/removes inside the workspace (reads stay allowed), exactly
like the read-only Bash mount, while an explicit writable child path still
wins by the most-specific-entry rule and the workspace entry wins over an
external ancestor or an equal external destination. When the sandbox is
**disabled**, the workspace is not a policy entry and the file tools keep the
historical always-writable workspace (Bash is unsandboxed); external roots
stay active either way.

`writable_paths` / `readable_paths` define one resolved policy shared by Bash
mounts and the file tools' external capabilities. A leading `~` expands to the
home directory; relative paths resolve against the main workspace. Existing
file/directory roots are canonicalized (including symlink aliases); only
identical (canonical source, configured destination) pairs are deduplicated,
while two configured aliases of the same canonical source with different
destinations are both preserved as separate mount entries (the canonical
authority itself is never duplicated — the path vectors still deduplicate by
source). Missing global roots are skipped. Each configured root is
kept as a **(canonical source, configured logical destination)** pair: Bash
mounts the canonical source at the configured destination, and the file tools
open their capability from the canonical source but look up absolute paths by
the configured logical destination (the path the user wrote, before
canonicalization). Lookup is lexical — the input path is never ambiently
canonicalized — and picks the single most-specific logical winner among all
entries; an equal destination
resolves to the workspace entry. The winner's mode is then enforced: a
writable operation against a most-specific read-only winner is rejected
outright and never falls back to a broader writable parent (reads select the
same winner). The same canonical source can therefore exist
independently as a canonical read-write entry and an alias read-only entry
(e.g. a `~/.cargo` symlink onto a canonical writable root: the alias stays
read-only while the canonical path stays writable).

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
writable roots: the user-level `[sandbox]` defines the authorized roots, and a
project can only narrow or accumulate within them — it can never widen them.
A project path outside every global root is a startup error naming the
offending path and the fix: add that path or an ancestor to the user-level
`[sandbox]` (e.g. `writable_paths` for writes), or remove/narrow the
project-local entry. With no user-level `[sandbox]` at all, project path
fields are rejected with the same guidance; project scalar overrides such as
`enabled = true` still apply without any global `[sandbox]`. Malformed or
unreadable project config is a startup error. The project `[sandbox]` scalars
`enabled`, `network`, and `workspace_writable` override the global keys
per-key: a key present in the project config replaces the global value, and
absent project keys keep the global values (so a project can turn the sandbox
on or off, or flip only the network policy, without restating the paths). A
read-only child under a
writable parent is rejected rather than silently restoring write authority;
select a narrower writable root or downgrade the whole selected root instead.
After narrowing, the configured mounts are filtered by the final canonical
roots: a stale mount whose canonical source is no longer inside any final
writable root (a narrowed-away global writable parent) is dropped so Bash
cannot regain the removed authority through the stale bind, while unrelated
global mounts and independent read-only alias mounts whose source lies inside
a final writable root survive.
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
is read from standard input. Sessions persist as an append-only log of
message and compaction entries; the default SQLite backend persists them
to `.e-agent/sessions.db` in the workspace (JSONL remains selectable via
`[session] backend = "jsonl"`), and `[session] backend` can switch
persistence to GreptimeDB as well (see the backend sections below). Without `--session`, each launch creates a
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
and cannot be combined with `--at` without `--fork`. JSONL, GreptimeDB, and
SQLite backends behave equivalently.

### TUI commands

Slash commands in the TUI input line (the web UI's input exposes the same
command set through its slash-command menu):

- `/compact` — compress the context (manual compaction).
- `/rename <标题>` — rename the session.
- `/btw <问题>` — fork a side subagent for the question.
- `/fork [N]` — fork a new session from the last (or the N-th) completed
  turn boundary.
- `/undo` — undo the most recent file operation (`edit_file` / `write_file`).
- `/goal` / `/goal set <目标>` / `/goal pause|resume|clear` — view the
  session's goal, create a new one (human-only), or pause/resume/clear the
  current one.
- `/help [命令]` — show the command list, or per-command usage detail
  (`/help fork`); an unknown command name prints a hint back to the list.
- `/model <profile>` — switch the session's model at runtime (not listed by
  bare `/help`; the profile must be resolvable from the config).

The TUI renders with a truecolor (24-bit) Solarized Light palette; terminals
without truecolor support may render colors incorrectly.

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

At startup, the main agent scans both directories once (a fixed snapshot, no
hot reload) and injects a brief index — never the bodies — into its system
context: each skill becomes one `- **name**: description — /absolute/path/SKILL.md`
line, sorted by name. `name`/`description` come from the SKILL.md's leading
`---` frontmatter (single-line `name:` / `description:` fields); missing,
unterminated or field-less frontmatter falls back to the directory name / a
fixed short phrase. When a skill's directory name exists in both, the
workspace version **fully replaces** the global one. The model opens a skill
with the existing `read_file` tool: workspace skills through the workspace
capability, global skills through an extra read-only capability rooted at the
global skills directory that only the main session's file tools carry
(subagents and btw forks never see it). The injection order is: main role
template (`agents/main.md`) → workspace `AGENTS.md` → skills → MCP server
instructions. Missing directories, non-directory entries, and symlinked skill
directories / `SKILL.md` files are silently skipped; the global skills root
must itself be a real directory, not a symlink.
`read_file` pages long files by 1-indexed lines (optional `offset`/`limit`,
default 2000 lines per read, capped at 64 KiB) and prints a continuation hint
when more lines remain.
Tool calls and their results are displayed as they happen; the final answer
ends the turn.

## Safety boundaries

The workspace and configured external capabilities constrain only the agent's
file tools. Relative paths use the workspace; absolute paths require an
authorized external root, matched by the configured logical destination (the
canonical source backs the capability; the input path is never canonicalized).
Readable roots permit reads, while
writable roots permit read/write/edit. For a writable operation the
most-specific logical winner decides: a read-only winner (e.g. a read-only
child under a writable parent) rejects the operation outright rather than
falling back to a broader writable root, and an exact read-only file denies
only itself — its siblings follow the parent's own policy. Exact-file roots do not grant authority
to siblings. Directory capabilities permit symlinks that remain inside their
root and reject symlink escapes. Exact-file capabilities use a durable handle
opened at startup: if the trusted host renames or replaces that pathname, file
tools still address the original inode, while Bash resolves and mounts the
pathname on each invocation. The trusted host should keep configured roots
stable for the run. Delegated custom workspaces must be canonical directories
inside the startup workspace or an authorized writable external directory;
read-only and exact-file capabilities cannot be rerooted, configured alias
destinations never extend reroot, and when the sandbox is enabled a read-only
workspace is not writable provenance either — reroot then derives only from
writable canonical directory capabilities (an explicit writable canonical
child of a read-only workspace still reroots through its external
capability). Capabilities do not
constrain hardlink inode origins or hardlink aliases to the protected project
policy file, do not make read/edit sequences atomic, and are not a process
sandbox.

`bash` runs `/bin/bash -lc` with the workspace as its current directory. By
default it is not sandboxed: commands can access files outside the workspace,
environment variables, and the network. On Unix, timed-out commands are killed
as a process group; on non-Unix platforms only the direct child is killed.
stdout and stderr are each captured up to 64 KiB, then drained and marked as
truncated.

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
through Everyone or the current logon SID, including
some public locations, may remain writable. Only existing canonical directory paths on
fixed local NTFS volumes are accepted. UNC/device paths, non-directory roots,
roots that are symlinks/reparse points, NULL DACLs, and case-sensitive roots
are rejected. Before the first installation (or a versioned ACE upgrade), every
write root that needs its complete inheritable capability ACE is scanned without
following links. Hard-linked file descendants whose aliases all stay inside the
write roots are allowed; a hardlink that aliases a file outside the write roots
(out-of-bounds) is rejected with every alias path listed. Nested
symlink/reparse point descendants are unsupported and rejected. Once the exact current-version ACE is
installed, later commands skip both ACL propagation and that root's full-tree
scan. The ACE grants create/overwrite plus child rename/delete/atomic replace
through `FILE_DELETE_CHILD` inherited by directories, without granting `DELETE`
on the write root itself. Concurrent path replacement races (TOCTOU) remain a
risk. In practice this makes cargo's `link_or_copy` (which hardlinks cached
files, e.g. from `~/.cargo/registry/src`, into the build target directory)
work when every alias stays inside the write roots — for example a writable
workspace plus `~/.cargo/registry` as a narrowed write root instead of the
broader `~/.cargo`. A hardlink that would alias a file outside the write roots
is still rejected with all alias paths listed, so align the write roots with
the locations the build actually shares. Windows shell
execution with protected Git metadata (`protect_git = true`, used by delegated
subagents/fixers) is unsupported and fails closed before token or ACL changes
with actionable guidance in the error message; there is no `.git` ACL
carve-out. A role template can opt out per role with `protect_git = false` in
its frontmatter (see "Role frontmatter: `protect_git`" above), restoring a
working shell for that role under the Windows sandbox.

Synthetic capability allow ACEs persist after successful execution. If adding
an ACE to a later root fails, ACEs already added to earlier roots may also
remain; the process is not started and no whole-DACL rollback is attempted.
These synthetic SID ACEs are inert for ordinary tokens that do not contain the
SID, but administrators may need to remove them manually. Cancellation,
timeout, and running-task registry teardown currently terminate only the
top-level process. Descendants may keep running with the capability and continue
writing the allowed roots. Atomic assignment to a Job Object, with process-tree
termination on cancellation, timeout, and registry teardown, is a future
lifecycle enhancement.

On Linux/macOS every `bash` call—main agent and subagents alike—is wrapped in
`bwrap`: system directories are mounted read-only, the workspace is mounted
read-write (`workspace_writable = false` makes it read-only), `/tmp` and `/home`
are fresh tmpfs, PID/IPC/UTS namespaces are unshared, and TIOCSTI is blocked via
`--new-session`. Network stays available by default; `network = false` unshares
it. When the host uses systemd-resolved, the stub resolver at
`/run/systemd/resolve` is mounted read-only so DNS resolution via the symlinked
`/etc/resolv.conf` works inside the sandbox. The restriction constrains the
spawned command, not the agent process itself. It is best-effort: bwrap is not
setuid, the host network is shared by default, and environment variables of the
parent process other than the stripped credential names (`EXA_API_KEY`,
`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `DEEPSEEK_API_KEY`, `MOONSHOT_API_KEY`,
`KIMI_API_KEY`) remain visible.

On Linux/macOS, the workspace `.git` entry depends on the role. The main agent
leaves it writable for repository operations. Delegated subagents/fixers bind
`<workspace>/.git` read-only over itself after writable mounts. This covers only
the workspace `.git` entry, not external object stores or other repositories.
Background commands inherit the parent's role.

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

A `FinishWhenIdle` session (a delegated subagent, or a one-shot CLI session)
that finished its work waits for its blocking background tasks before
finalizing. `[delegate] finalize_wait_secs` caps that wait (default 10
minutes; `0` disables the cap and waits forever). On expiry the session
finalizes as completed without cancelling the tasks — they keep running in
the shared registry, where the parent agent can still read their output or
cancel them from the task panel.

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

## Session goals

Every session can carry at most one current goal — a minimal persistence
layer (a deliberate subset of a DSH-style goal system, no scheduler/rounds/
verifier). Fields: `id`, `revision`, `objective`, `success_criteria`,
`status` (`active|paused|blocked|completed`), `progress`, `evidence`,
`blocked_reason`.

- **Human-only creation**: `/goal set <objective>` (TUI/REPL/web), or
  `POST /api/sessions/{id}/goal` with `{"action":"set","objective":…}`.
  Creation is rejected while a non-completed goal exists; a completed or
  cleared goal frees the slot. The model never creates goals.
- **Model updates**: the `get_goal` and `update_goal` tools read the current
  goal and apply transitions (`progress`, `pause`, `resume`, `block`,
  `complete`, `clear`) under an `id` + `revision` CAS; `get_goal` returns
  the FULL machine-usable snapshot (all fields as JSON), while the provider
  context keeps a short ≤400-char projection. `update_goal` can replace
  `success_criteria` and append `evidence` (both must be string arrays).
  `complete` requires non-empty `evidence` in the SAME call — pure analysis
  may use an explicit `unverified: <analysis>` string; a completed goal
  keeps its evidence. `resume` unblocks paused OR blocked goals (clearing
  `blocked_reason`).
- **Append-only persistence**: every change appends a complete snapshot as a
  `SessionEntry::GoalUpdated` (`goal: null` is the clear tombstone). The
  latest snapshot wins on resume (fold) and survives compaction: a short
  projection is prepended to every provider context, so the goal is never
  lost across compaction/resume/restart. After a clear, the context injects
  an explicit `none (cleared)` override so a stale compaction summary can
  never re-introduce the old goal.
- **Forks inherit**: `--fork`/`/fork`/btw forks copy the source prefix up to
  the fork boundary, including goal updates before it; the forked session
  folds the newest snapshot naturally.
- **Subagent isolation**: subagents get the goal tools but their runner
  applies them against the subagent's own (usually empty) goal state — a
  subagent can never take a mutable reference to its parent's goal.
- **UI**: the TUI and web UI render goal updates as a notice line plus a
  fixed GoalBar (`🎯 [status] objective`); the UI shows evidence/`unverified`
  only, never a claim of independent verification.

Non-goals (deliberately out of scope): todo/plan/workflow stages, auto goal
rounds / max rounds, deadlines, reminders, goal DAGs / multiple goals per
session, and a verifier agent.

## Web UI / headless server

`e-agent --serve` (alias: `e-agent web`) starts a headless HTTP server that
serves the web UI at `/` and a token-authenticated `/api/*` surface for
managing live sessions. It binds `127.0.0.1:8766` by default; `--host` and
`--port` (or `-p`) override:

```sh
e-agent web                       # http://127.0.0.1:8766
e-agent --serve --port 9000       # http://127.0.0.1:9000
```

Every `/api/*` request must authenticate with the server token as
`Authorization: Bearer <token>` (or `?token=<token>`, the query fallback for
EventSource-style clients that cannot set headers). The token is generated
once at first start — 32 random bytes, base64url, written with mode 0600 to
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
| POST | `/api/sessions/{id}/model` | switch the session's model at runtime |
| POST | `/api/sessions/{id}/undo` | undo the most recent file operation |
| PUT | `/api/sessions/{id}/title` | rename a session |
| PUT | `/api/sessions/{id}/pin` | pin a session |
| PUT | `/api/sessions/{id}/archive` | archive a session |
| DELETE | `/api/sessions/{id}` | cancel + remove from the registry |
| GET | `/api/tasks` | running background tasks, all sessions |
| DELETE | `/api/sessions/{id}/tasks/{task_id}` | cancel one background task |
| GET | `/api/sessions/{id}/tasks/{task_id}/output` | full output of a running bash task |

SSE connections are kept alive with 15-second heartbeat pings. Ctrl-C shuts
the server down gracefully: SSE streams self-close on the shutdown signal and
in-flight requests get a hard 2-second drain deadline before the process
force-exits; a second Ctrl-C kills it outright.

A known limitation of `DELETE /api/sessions/{id}` (pre-existing, tracked
separately): for a WaitForInput web session it can leave an idle runner
behind. `cancel` is a *release* that parks a WaitForInput session at Idle,
and any other holder of the live session (e.g. an in-flight request) keeps
the session's runner task alive past its removal from the registry — an
unaddressable idle runner that only ends when the last holder drops it or
the process exits. The transcript stays; a later resume still works.

## Config hot reload

The headless server (`--serve` / `web`) and the TUI watch the config files
(the global `config.toml` plus the project override
`<workspace>/.e-agent/config.toml`) and, when a file changes, re-read and
atomically swap the effective config. A change takes effect without
restarting the process:

- `[models]` / `[providers]` / `[roles]` — new profile definitions are
  immediately available to the web `/model` autocomplete and `POST
  /api/sessions/{id}/model` / TUI `/model` switches, and newly built
  sessions (web `POST /api/sessions`, a fresh CLI session) start with the
  edited default and role routing.
- `[mcp]`, `[bash]` timeout, `[background]` timeout, `[delegate]`
  finalize wait — applied to sessions built after the reload.
- Other sections are carried into the reloaded config but only take effect
  where a runtime read exists.

A reload that fails to parse or resolve (a typo, a missing key file, a
`chatgpt`-routed profile without a login) is rejected and logged; the
previous config stays, so editing the config never breaks a running server.

Deliberately NOT hot-reloaded (restart required):

- `[sandbox]` — workspace roots and file capabilities are wired at startup.
- `[session]` backend — stores are connected at startup.
- `[web_search]` key changes — the key is injected into the process env once
  at startup (`std::env::set_var` is only safe single-threaded).
- `AGENTS.md` / skills instructions and existing sessions' models (existing
  sessions keep their model until a `/model` switch).

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
Project MCP servers apply to subagents too — tools are built from the unified
effective config, and subagents inherit the workspace's MCP toolset.

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
  The bytes are stored once under their hash; the tool result is structured —
  a text summary plus the image reference — and both ride on the canonical
  `Tool` message (no synthetic user message is created). Base64 never enters
  the scrollback or the session file.
- **REPL `/image <path>`** (human entrance, REPL mode only): attaches the
  image to the next prompt; the placeholder line
  `[image attached: <path>]` is included in the prompt text.

Sessions persist only `ImageRef { hash, mime }` references — never base64.
At send time the wire re-reads the file and builds the provider format:
`image_url` parts (an object) for OpenAI-compatible chat, `input_image` parts
(a bare-string `image_url`) for ChatGPT Responses. An image-bearing tool
result is encoded natively on both wires: the chat wire emits all tool
results as plain `role:"tool"` text and then appends at most one aggregated
temporary user image message per consecutive tool batch (wire-only, never
persisted), while the Responses wire emits a `function_call_output` whose
`output` is an array of `input_text` + `input_image` parts. A missing cache
file degrades to a `[image missing: <hash>]` text placeholder instead of
failing.

A model only receives images when its profile sets `vision = true` (see
[Run](#run)). On a non-vision model the request copy strips image parts from
user and tool messages with a text-degradation note, while the persisted
history keeps them — switching back to a vision model restores the images on
the next request. An explicit `/image` prompt on a non-vision model fails
with a clear `model X does not support image input` error before any request
is sent.

Errors print their causal chain (for example `cannot decode provider
response: error decoding response body: ...`), and provider HTTP failures
include a preview of the response body. Provider requests time out after
600 seconds in total, including the complete streaming response body.

## SQLite session backend

A local SQLite session storage backend is available and compiled in by
default (`sqlite` is a default cargo feature). It replaces the JSONL file
backend for session persistence; select it via the TOML config:

```toml
[session]
backend = "sqlite"
path = "/home/you/.local/share/e-agent/sessions.db"
```

`path` is a path to a SQLite-compatible database file (`:memory:` works for
tests); it is used as written — `~` is not expanded, and a relative path
resolves against the process working directory, so an absolute path is
recommended. The parent directory is created on first connect. Sessions of
different workspaces are isolated by a `workspace_id` derived from the
canonical workspace root, so one database file can serve many workspaces.

The database keeps three tables:

- `session_entries` — the append-only transcript log, primary-keyed on
  `(workspace_id, session_id, seq, event_time_us)` with the same entry kinds
  and ordering semantics as the other backends.
- `running_tasks` — in-flight background bash/delegate bookkeeping; a resumed
  session finds tasks killed by a previous process.
- `sessions` — a per-session metadata audit log (created_at/model/role/
  title/pinned/archived/writer) backing the web UI's list/rename/pin/archive
  views.

Connections use `PRAGMA journal_mode=WAL` and `busy_timeout=5000`, so
processes sharing one file wait out short writer locks instead of failing.
The strictly monotonic per-process `event_time_us` keeps seq continuity
identical to the JSONL/GreptimeDB backends. Like GreptimeDB, SQLite has no
automatic migration from JSONL and no cross-backend migration. SQLite is
the default backend since 0.1.1+ (absent `[session]` resolves to
`.e-agent/sessions.db`); JSONL stays selectable for legacy workspaces.

## GreptimeDB session backend (experimental)

An optional GreptimeDB-backed session storage backend can be selected at
runtime via the TOML config file. It replaces the JSONL file backend for
session persistence; background-task bookkeeping lives in the backend's
`running_tasks` table (JSONL sidecar files are only used by the JSONL
backend).

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
Cancellation is a *release*, not a termination: `cancel` preempts the
in-flight operation and never ends the session — the only hard termination
is `DELETE /api/sessions/{id}` or the tasks-panel cancel (which aborts a
subagent through its parent's background-task registry). Idle-runner
lifecycle management (idle timeouts / GC for parked sessions) is not in
scope.
Windows restricted-token MVP is only an accident-prevention write restriction:
no read isolation, network isolation, Job Object, AppContainer, broker/IPC,
ConPTY, dedicated user, protected-git shell execution, or bwrap-equivalent
filesystem view. Everyone/logon-SID writable public locations can remain
writable, synthetic capability ACEs persist, and cancellation currently
terminates only the top-level process.
It does speak MCP to local stdio servers (tools only), but it does NOT do
remote MCP over HTTP/SSE, MCP OAuth, MCP resources/prompts, server-initiated
notifications, `listChanged` refresh, server restart on crash, or concurrent
server initialization.
It has one model seam and one tool seam. Main-agent tool rounds are unlimited
unless `--max-rounds` sets an explicit cap; subagent tool rounds are unlimited.
Config hot reload is deliberately scoped: `[models]`/`[providers]`/`[roles]`
(and anything else read at session-build time) hot-reload in the server and
TUI via mtime polling with validate-before-swap, but `[sandbox]`, the
`[session]` backend, and web-search key env injection stay startup-fixed and
require a restart; there is no reload HTTP endpoint, no config diffing, no
watch(1)/inotify, and no per-section partial reload (a bad edit is rejected
wholesale and the last good config is kept).
Reasoning-model `reasoning_content` is persisted in the session for
display/audit; it is never sent back to the API.
Web search deliberately has no browser, crawler, URL fetch, citations, domain
filters, multiple providers or provider trait, retries, cache, background
search, or remote MCP support.
The web UI is a single self-contained HTML page, assembled from disk on
every request in dev builds and compiled into the binary via `include_str!`
in release builds: deliberately no frontend bundler/build pipeline, no
hashed asset routes, no HTTP compression, no runtime asset discovery, and no
KaTeX font packaging.
Workspace skills are deliberately flat `<skills-dir>/<name>/SKILL.md` only
(global skills from `Config::config_dir()/skills/`, workspace skills from
`.e-agent/skills/`): frontmatter disclosure is limited to single-line
`name`/`description` with no body injection, sanitizer, or caps; no remote
installation, registry, dependencies, hot reload, slash commands, on-demand
loading, config toggle, or recursive skill discovery. Global skills are
main-agent-only: subagents and btw forks neither inherit the index nor the
read capability. The same-name override is a full replacement, not a merge
or concatenation.
Session goals are a single-current-goal persistence layer only: no
todo/plan/workflow stages, no auto goal rounds / max rounds, no deadlines,
no reminders, no goal DAGs / multiple goals per session, no goal verifier
agent, and no scheduler/driver — the goal is a snapshot with a CAS, not a
task runner.

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
- No distributed/cluster deployment — standalone GreptimeDB only
- No generic storage abstraction for future third backends
