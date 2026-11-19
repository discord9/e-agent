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

This checkout pins Rust 1.99.0 in `rust-toolchain.toml`, which rustup installs
and applies automatically to the `cargo` commands above. Rust 1.101 nightly is
incompatible with the default `sqlite` feature: `branches` 0.4.5 (pulled in by
`turso_core` 0.7.2) selects `core::intrinsics::abort`, removed in that
nightly, so the build there fails to compile; 1.99.0 stable compiles it. An
explicit toolchain overrides the pinned file: the `RUSTUP_TOOLCHAIN`
environment variable and `cargo +<toolchain>` both take precedence over
`rust-toolchain.toml`. Check or force it with:

```sh
unset RUSTUP_TOOLCHAIN             # if an inherited override is active
rustup show active-toolchain       # expect 1.99.0-...
cargo +1.99.0 build --release --locked
```

### Android / Termux (ARM64)

The Android/Termux ARM64 release installer uses
`e-agent-aarch64-linux-android.tar.gz`, `SHA256SUMS`, `install-termux.sh`, and
`termux-web.sh`, `termux-stop.sh`, `termux-service.sh`, and both shortcut PNG icons from a published release. The package is cross-compiled;
the installer also configures Termux service supervision and rotating logs.
In Termux, run this one command to install the prerequisites and download and
run the installer:

```sh
pkg install curl coreutils tar && (installer=$(mktemp) && curl -fsSL https://github.com/discord9/e-agent/releases/latest/download/install-termux.sh -o "$installer" && bash "$installer"; status=$?; rm -f "$installer"; exit "$status")
```

The installer resolves the latest release tag once, then fetches every payload
from that pinned tag. Re-running the same command installs an upgrade in place
after downloading and validating all assets; it replaces `$PREFIX/bin/e-agent`.
Any different existing Widget shortcut is retained (including an older default
launcher) — only a shortcut identical to the current generated template counts
as unchanged — so use `--force-shortcut` to replace one explicitly. Existing
config, credentials, workspace, token, and other files are not reset. To install
a particular release, download that release's `install-termux.sh` and run
`bash install-termux.sh --version TAG`; this is an explicit pin, not automatic
rollback.

To activate an upgraded binary, run `sv restart e-agent-web`. The installer does
not replace Cargo's usual `$HOME/.cargo/bin/e-agent`, and a pre-existing
`$PREFIX/bin/e` alias is left untouched: check which executable is selected with
`command -v e-agent` and verify the release binary with
`$PREFIX/bin/e-agent --version`. An older foreground server must be stopped
first; the installer leaves its new service disabled if port 8766 is already
occupied or its state is uncertain. Only an explicit TCP connection refusal
counts as a free port (timeouts, resets, and non-HTTP listeners do not), and the
start shortcut applies the same check.

The installer installs `termux-services` if needed and creates
`$PREFIX/var/service/e-agent-web/run` plus its `log/run` subservice. The Web
server runs under runit, which restarts it after it exits; stdout and stderr
feed `svlogd`, rather than the Widget terminal. Logs are private and rotate
at 1 MiB, retaining up to ten archives, with daily rotation:

```sh
sv status e-agent-web
sv restart e-agent-web
sv down e-agent-web                       # stop until next startup/click
sv-disable e-agent-web                    # keep disabled across Termux starts
tail -n 100 "$PREFIX/var/log/sv/e-agent-web/current"
```

Reinstalling updates generated service files, preserves edited `run`,
`log/run`, and rotation settings, and keeps a disabled service disabled. To
change the workspace permanently, edit the service's `run` file (default:
`$HOME/e-agent-workspace`); changing a Widget's environment does not
reconfigure an already-running supervisor.

Install Termux:Widget from the same source as Termux. At the end of installation
the installer opens its shortcut chooser: select `e-agent-web` to add a
**one-icon (1×1) desktop shortcut** and approve any launcher permission prompt,
then use the picker again to add `e-agent-stop` as the second icon. The installer
places a green play icon (start/open) and a red square icon (stop) in
`$HOME/.shortcuts/icons`; custom icons are preserved unless `--force-shortcut`
is used, and shortcuts that keep old icons must be removed from the launcher and
added again. Clicking the start icon (or refreshing the Widget list) starts the
supervised service if needed, waits for authenticated readiness, opens the
browser, and returns — closing the terminal or browser does not stop the
service. The stop icon disables and stops the whole service (forcing termination
after five seconds if needed) and it stays stopped across Termux starts until
the start icon is clicked again; it then checks port 8766 and returns an error
instead of claiming success if it still responds or the check is uncertain. A
preserved older foreground launcher must be stopped separately — the shortcut
does not kill unmanaged processes — and it does not undo actions already sent to
other apps or stop independently detached programs. On first visit, paste the
server token into the UI; only an authenticated e-agent server on
`127.0.0.1:8766` is opened, and unrelated services are refused.

The latest port-check and stop-reporting changes have not yet been verified
on a phone. Earlier service, log and shortcut behavior was checked separately.

Android can still kill Termux and its child processes. Allow Termux background
operation and exclude it from battery optimization in your device settings.
The enabled service starts when Termux's service daemon starts; phone-boot
startup additionally requires Termux:Boot and is not installed automatically.
The installer does not install APKs, configure model credentials, grant Android
permissions, or configure ADB; configure `~/.config/e-agent/config.toml`
separately. Existing credentials, token, and workspace are preserved.

For session IDs, storage, and CLI options, continue in [Run](#run). See
[Safety boundaries](#safety-boundaries) before enabling or relying on sandbox
settings; [Background tasks](#background-tasks) and [Subagents](#subagents)
explain asynchronous work. Further reference: [session history](#history-tool),
[web server](#web-ui--headless-server), [local MCP](#mcp-local-servers), and
[storage backends](#sqlite-session-backend).

#### Termux APKs and domestic downloads

Install Termux and Termux:Widget from the **same distribution channel**. Their
APK signatures must match; do not mix F-Droid and GitHub builds. The official
[F-Droid Termux page](https://f-droid.org/packages/com.termux/) and
[Termux:Widget page](https://f-droid.org/packages/com.termux.widget/) are the
stable download references. F-Droid is not required if you download APKs
directly; Android 7 or later is required. In China, TUNA provides an F-Droid
mirror ([setup guidance](https://mirrors.tuna.tsinghua.edu.cn/help/fdroid/));
its direct APK links are a convenience and may change as releases update:

- [Termux 0.118.3 (TUNA mirror)](https://mirrors.tuna.tsinghua.edu.cn/fdroid/repo/com.termux_1002.apk)
- [Termux:Widget 0.15.0 (TUNA mirror)](https://mirrors.tuna.tsinghua.edu.cn/fdroid/repo/com.termux.widget_1001.apk)

These mirror URLs were checked on 2026-10-07; availability and download speed
are not guaranteed. Install/update both from the same channel. On first use,
open Termux once and add or refresh the `e-agent-web` shortcut in Termux:Widget.
Android 10 may require allowing the Widget shortcut activity under the system's
“Display over other apps” settings; the shortcut cannot enable that setting
for you.

#### Optional experimental same-phone ADB

On Android 11 or later, Termux can optionally use Android's wireless debugging
to control the **same phone**. This is experimental: Android's official ADB
guidance primarily describes a workstation, and loopback self-connection and
device/OEM support are not verified. Install the optional client with
`pkg install android-tools`, then enable Developer options and Wireless
debugging yourself. Pair using the pairing port and code shown by Android;
enter the code only at the interactive prompt, not in a script or agent log.
The pairing and connection ports are different:

```sh
adb pair 127.0.0.1:PAIR_PORT
adb connect 127.0.0.1:CONNECT_PORT
adb devices
adb -s 127.0.0.1:CONNECT_PORT shell input keyevent KEYCODE_HOME
adb -s 127.0.0.1:CONNECT_PORT exec-out screencap -p > screenshot.png
```

Replace the port placeholders with the current values shown in Wireless
debugging. Always select the exact intended device serial; never rely on
implicit selection when multiple devices are listed. Ports can change and
reconnection may be needed. Disconnect only the selected target when finished
with `adb disconnect 127.0.0.1:CONNECT_PORT`. This does not require root or
grant access to private app data. ADB commands can affect the phone and files
outside the e-agent workspace; workspace and shell sandbox settings do not
contain those effects. Do not use ADB to automate pairing, enable TCP port
5555, change global settings, or bypass user permission prompts.

References: [Android ADB](https://developer.android.com/tools/adb) and
[Termux android-tools package](https://github.com/termux/termux-packages/tree/master/packages/android-tools).

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

Omitting both `api_key_file` and `api_key_env` configures an unauthenticated
local provider (vLLM, Ollama, llama.cpp, ...). Requests to it omit the
`Authorization` header entirely; a provider whose configured key file or
environment variable trims to empty is still rejected. The minimal setup is:

```toml
[providers.local]
base_url = "http://127.0.0.1:8000/v1"

[models."local/qwen"]
model = "qwen3-local"
```

DeepSeek Chat thinking mode has a wire contract of its own. `deepseek_compat`
defaults to **false**, so every profile that does not set it — OpenAI, Kimi,
any other OpenAI-compatible provider, and the Codex Responses wire — keeps
the pre-existing standard wire unchanged: persisted reasoning is
display/audit only and is never replayed. Setting `deepseek_compat = true`
on a profile explicitly opts that profile into the DeepSeek Chat
compatibility wire:

- `content: ""` instead of an absent/null `content` on content-less
  assistant turns — gated by `deepseek_compat` alone;
- when `thinking = true` as well, assistant turns that carry `tool_calls`
  echo their original `reasoning_content` (including an empty string) back
  to the API on the next request — DeepSeek returns a 400 "reasoning_content
  must be passed back" error without it. Plain assistant reasoning (no tool
  calls) is never replayed, even with both flags set.

```toml
default = "deepseek/deepseek-chat"

[providers.deepseek]
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"

[models."deepseek/deepseek-chat"]
model = "deepseek-chat"
reasoning_effort = "high"
thinking = true
# Explicit DeepSeek Chat wire compatibility. Defaults to false; profiles
# that leave it unset keep the standard wire unchanged.
deepseek_compat = true
```

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
role templates can also be routed by role name. Role templates are discovered from three layers, in increasing priority:
`$XDG_CONFIG_HOME/e-agent/agents/<role>.md` (falling back to
`~/.config/e-agent/agents/`), `<workspace>/agents/<role>.md` (legacy), and
`<workspace>/.e-agent/agents/<role>.md` (canonical). All layers are scanned;
the canonical project file wins over the legacy project file, which wins over
the global file. The delegate tool dynamically discloses each discovered
role as `name — description` when its description is present, or as plain
`name` otherwise; the role parameter remains a simple enum of role names.
Descriptions are package metadata for choosing a role, not additional routing
or schema types. This repository ships templates for `designer`, `explorer`,
`fixer`, `main`, `oracle`, `seer`, `skeptic`, and `verifier` (`main` is
non-delegable); unrouted named roles fall back to the default subagent model,
which itself falls back to the main profile. The `oracle` gives a comprehensive
correctness verdict, the `skeptic` challenges a concrete claim or assumption
from a selected adversarial lens, and the `verifier` checks risky changes
against user-visible invariants through real product entrypoints:

```toml
[models."kimi/k2"]
model = "k2"

[roles]
subagent = "kimi/k2"
# Optional separate model for behavior audits:
# auditor = "kimi/k2"
```

A candidate run of three consecutive simple bash status probes triggers a
read-only second-stage behavior audit in main, delegate, fork, and btw sessions.
Capped automatic Goal turns skip this optional audit; their tool results still
commit. The `[roles] auditor` model is used when configured; otherwise the
default subagent model (including its main-profile fallback) is used. The
auditor runs asynchronously, without delaying turn progress or steering, and
receives only a bounded, classified summary of recent tool calls/results, not
raw prompts, commands, paths, or output. An exact JSON verdict of `FAIL` or
`SUSPECT` yields one advisory notice at a safe boundary while the session
remains live; `CLEAN`, invalid responses, errors, timeouts, or an unavailable
model produce none. A verdict arriving after session termination is dropped.
No tools are blocked and no running task is cancelled.

A project-local `<workspace>/.e-agent/config.toml` overlays `default`,
`[models]`, `[roles]`, `[mcp]`, `[tui]`, and `[pet]` onto this global config: models
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
not a standalone config). `[sandbox]`, `[background]`, and `[bash]` project overrides are described in the relevant sections
below.

The project file is strict: unknown sections (a typo, or a section the
project file cannot carry) are a startup error naming the offending field
instead of being silently ignored. And since a project `[mcp]` server starts
the command it configures, `<workspace>/.e-agent/config.toml` is trusted
input — treat it like the global config: only open workspaces you trust,
because opening a workspace runs the MCP commands that workspace's file
declares. A global `[mcp."<name>"] enabled = false` is a kill switch that a
project file cannot re-enable (see the MCP section). `[code_mode]` is
user-level only for the same reason (see "Code mode: `run_rust`").

### Desktop pet sprite sheet

The web UI carries no pet artwork and renders no desktop pet by default. To
show one, add a user-supplied local sprite sheet under `[pet]` in the global or
project config; the browser reads it through the authenticated server and never
receives an arbitrary file path:

```toml
[pet]
spritesheet = "/absolute/path/to/maid.webp"
sprite_cols = 8       # default 8
sprite_rows = 11      # default 11
frame_width = 192     # default 192
frame_height = 208    # default 208
idle_row = 0          # default 0; one row is one animation state
idle_frames = 6       # default 6; row 0 of the bundled sheet has 6 real frames —
                      # set the chosen row's real frame count
loop_ms = 1200        # default 1200; duration of one idle-row cycle
                      # (the UI honors at least ~16 ms per frame, so
                      # effective cycles never go below 16 * idle_frames)
```

Only PNG, JPEG, WebP, and GIF files up to 10 MiB are served. Each sprite-sheet
row is one animation state: `idle_row` stays fixed while `idle_frames` controls
how many cells in that row play. Remove the `[pet]` section (or its
`spritesheet` key) to hide the pet; it also stays hidden if config or sprite
loading fails, and it is available on narrow mobile screens when configured. The
config is read live, so no server restart is needed. A project `[pet]` section
replaces the global one wholesale, like `[tui]`; omitted fields use built-in
defaults rather than global values. Sprite files are user-supplied and are never
embedded in or distributed with e-agent.

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

### Role descriptions and read-only roles

A role template may declare a short disclosure description in its leading TOML
frontmatter with `description = "..."`. The value is trimmed; missing or blank
values are treated as absent, and descriptions may not contain newlines or
carriage returns. The delegate tool shows described roles as `name —
description` while keeping the role schema as an enum of names.

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

A read-only role loses the write and edit tools. Its shell, when the sandbox is
enabled, runs in a narrowed bubblewrap policy: the workspace is mounted
read-only, extra writable roots are dropped, and the network is disabled
(`--unshare-net`). Read tools stay available, including the network read tools
(web search, and DeepSeek's page fetch), which deliberately still reach the
network. Background bash commands started inside the subagent inherit the
role's narrowed sandbox too (they never fall back to the parent's wider
policy). If the sandbox is not enabled (or bwrap is unavailable), a read-only
role gets **no `bash` tool at all** —
fail closed rather than an unsandboxed shell.

Read-only is a tool boundary, not a host security framework: it constrains
which tools the model may call, and the bash sandbox remains a best-effort
bwrap policy (see "Safety boundaries"). Inside that sandbox `/tmp` is the
host's real `/tmp` (writable for main and subagents alike) and the `$HOME`
tmpfs remains writable scratch space; the file tools are the
authoritative write path, and they are absent.

The main session can request the same policy with `--read-only` (MCP tools are
dropped too, since they carry no read-only marker). Delegation stays available
(spawning a subagent does not mutate the host session), and each subagent
resolves its own role template.

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
(`protect_git = true`): on Linux/Android the sandbox binds Git metadata
read-only so a delegated agent cannot delete or corrupt the repository (the
main agent leaves its own `.git` writable so it can orchestrate git operations).
For a delegated linked worktree, this includes the `.git` pointer and only the
caller-authorized main-worktree Git metadata and administrative paths; ordinary
workspace file writes remain writable. Writable aliases to that metadata are
rejected. The same frontmatter block can opt a role out with `protect_git = false`:

```markdown
---
protect_git = false
---
Fix the code and run the checks.
```

This matters on Windows: the write-sandbox MVP has no way to make `.git`
read-only, so with `protect_git = true` every shell command in a delegated
subagent/fixer is rejected before execution with guidance in the error message.
Setting `protect_git = false` in the role's frontmatter (accepting that `.git`
stays writable inside the sandbox) restores a working shell for that role; the
key defaults to `true` when omitted. For ordinary repositories that opt-out
disables the protection, but for delegated linked worktrees the retained Git
metadata stays read-only even then — the opt-out does not authorize writable
metadata aliases.

The `bash` tool can be sandboxed with `bubblewrap` when it is installed. The
sandbox is off unless explicitly enabled:

```toml
[sandbox]
enabled = true
# network = false            # unshare the network namespace (default: shared)
# workspace_writable = false # mount the workspace read-only (default: writable)
# gpu = false                # expose GPU devices (/dev/kfd, /dev/dri, /dev/nvidia*) to sandboxed bash
# Extra mounts, e.g. toolchain caches the agent needs inside the sandbox:
# writable_paths = ["/mnt/big/cargo-home"]
# readable_paths = ["~/.rustup", "~/.local"]
```

`gpu` is global-config-only: a project `.e-agent/config.toml` that sets it is
rejected at startup.

`gpu` and the runtime installation are two separate halves of one policy.
`gpu = true` grants the host GPU device nodes (`/dev/kfd`, `/dev/dri`, plus any
`/dev/nvidia*`, mounted read-write) and the sysfs subtrees the AMD ROCm runtime
reads: `/sys/devices` for the KFD topology and the DRM devices, `/sys/dev/char`
for the device-number links HIP uses to match KFD nodes to render nodes, and
`/sys/module/amdgpu` for the module state `rocminfo` gates on. The sysfs
subtrees are mounted read-only. These grants are unconditional under the flag —
they are not detected from the host GPU vendor, so they simply do not resolve
on hosts without those paths. `gpu = true` alone does not make a ROCm
installation visible: the loader, the `rocminfo` binary and the Python
environment must be mounted through `readable_paths`. Both are needed for a
working `rocminfo`/HIP inside the sandbox, e.g.

```toml
[sandbox]
enabled = true
gpu = true
readable_paths = ["/opt/rocm", "/home/<user>/venv"]   # runtime + interpreter
```

`/opt/rocm` is usually a symlink chain (`/opt/rocm` → `/etc/alternatives/rocm`
→ `/opt/rocm-<version>`); configured roots keep BOTH their configured location
and their canonical source, so the whole chain resolves inside the sandbox.
A root that does not exist on the host is skipped.

Security exposure: `gpu = true` hands model-generated commands the host GPUs,
which are shared resources with no isolation between the sandboxed command and
other GPU users: compute and VRAM capacity are shared, and the exposed
driver/device interfaces and the read-only `/sys/devices` device tree enlarge
the kernel/driver attack surface. Because a configured path can overlap a
device node, the GPU grants are installed last in the argument vector, so
`gpu = true` wins over a configured `readable_paths`/`writable_paths` entry
covering the same path — that precedence is deliberate (the explicit GPU opt-in
is the narrower authority) and means an overlapping configured entry is NOT a
way to device-block a GPU-enabled sandbox.

On an AMD host, verify a working policy on the host with
`scripts/verify_gpu_sandbox.sh`: it runs the ignored acceptance test
(`tools::tests::gpu_acceptance_amd_rocm_sandbox`) through the production Bash
tool and requires `rocminfo` to enumerate the requested architecture, PyTorch
to see that HIP device, and a real tensor operation to match the CPU result.
`--gpu=false` runs the negative control on the same path.

`writable_paths` / `readable_paths` define one resolved policy shared by Bash
mounts and the file tools' external capabilities. A leading `~` expands to the
home directory; relative paths resolve against the main workspace; missing
global roots are skipped. Each configured root is kept as a **(canonical
source, configured logical destination)** pair: Bash mounts the canonical source
at the configured destination, and the file tools look up absolute paths by the
configured logical destination (the path the user wrote), lexically — the input
path is never ambiently canonicalized. The single most-specific logical winner
decides, with an equal destination resolving to the workspace entry; a writable
operation against a most-specific read-only winner is rejected outright and
never falls back to a broader writable parent. The same canonical source can
therefore exist as a canonical read-write entry and an alias read-only entry
(e.g. a `~/.cargo` symlink onto a canonical writable root: the alias stays
read-only while the canonical path stays writable).

When the sandbox is **enabled**, the workspace itself becomes a policy entry:
`workspace_writable = false` makes the file tools deny writes/edits/removes
inside the workspace (reads stay allowed), exactly like the read-only Bash
mount, while an explicit writable child path still wins by the most-specific
rule. When the sandbox is **disabled**, the workspace is not a policy entry and
the file tools keep the always-writable workspace; external roots stay active
either way.

A project-local `<workspace>/.e-agent/config.toml` merges with the global roots
instead of replacing them: a project path that is a strict subpath of a global
root replaces (narrows) that root, unrelated project paths accumulate, an equal
path is a no-op, and an absent file/section inherits the global roots unchanged.
If either path field appears, selection mode applies and the other field is
empty: readable selections may downgrade global writable roots, but writable
selections must come from global writable roots — the user-level `[sandbox]`
defines the authorized roots, and a project can only narrow or accumulate
within them, never widen them. A project path outside every global root is a
startup error naming the offending path and the fix (add that path or an
ancestor to the user-level `[sandbox]`, or remove/narrow the project entry);
with no user-level `[sandbox]` at all, project path fields are rejected with the
same guidance, while project scalar overrides such as `enabled = true` still
apply. Malformed or unreadable project config is a startup error. The project
scalars `enabled`, `network`, and `workspace_writable` override the global keys
per-key, so a project can flip one policy without restating the paths. After
narrowing, mounts are filtered by the final canonical roots: a stale mount whose
source is no longer inside any final writable root is dropped so Bash cannot
regain the removed authority through a stale bind, while unrelated global mounts
and read-only alias mounts inside a final writable root survive.

The exact workspace-relative `.e-agent/config.toml` policy file is readable by
file tools but write/edit protected and must be changed by the user outside the
agent. Bash binds an existing policy over `/dev/null`; if only `.e-agent/`
exists, that directory is mounted read-only so a writable child cannot create
the policy. If neither exists, no mountpoint is needed or created.

Note that the sandbox hides all of `$HOME` behind a fresh tmpfs and then binds
in only the workspace and these extra paths — so personal files such as
`~/.ssh`, `~/.gitconfig`, or `~/.config` are NOT visible inside the sandbox
unless explicitly listed in `readable_paths`. Toolchains installed under `$HOME`
(rustup, pyenv, nvm, ...) must be listed for the agent to use them.

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
Compaction and fork displays are reconstructed from their durable entries, so no
session migration is needed; legacy `.json` sessions are migrated on first load.
`--version` (or `-V`) prints the exact package build version without loading the
workspace or configuration. Optional CLI overrides are `--base-url URL`,
`--model MODEL`, `--profile PROFILE`, `--workspace PATH`, `--session NAME`,
`--fork SESSION`, `--at N`, and `--max-rounds N` (tool-call rounds are unlimited
by default; the flag sets an explicit cap). `--read-only` applies the read-only
role policy to the main session (see "Read-only roles").
`--profile` selects a TOML profile and requires a TOML config. When config is
used, `--base-url` and `--model` are raw API wire-value overrides and take
precedence over the selected profile values.

### Forking a session

`e-agent --fork <session-id> [--at <n>] [PROMPT]` starts a brand-new session
that inherits the source session's history up to a completed turn. The fork is
read-through, never a copy: the new session stores its own entries plus a
marker naming where it was cut, and rebuilds the inherited prefix by reading
the source read-only.

The prefix ends at (and includes) the last completed turn — an assistant
message with no pending tool calls, or a compaction — and drops everything
after it (background-completion notices, for example). `--at <n>` picks the
fork point explicitly: `n` is 1-based and inclusive, and it must land on a turn
boundary, otherwise the fork fails rather than cutting mid-turn. Without a
completed turn in the source, the fork fails; if the source is deleted,
rewritten, or unreadable, loading the fork fails closed with an explicit error
naming the source — never a silently truncated transcript. The newest goal at
or before the cut is frozen into the fork, so a fork starts with the goal it
inherited. Forking a fork resolves the whole chain.

The new session has fresh token accounting (usage counters start at zero) and is
resumed or forked again like any other session. The new id is printed as
`e-agent: forked session: <id>`; `--fork` and `--session` are mutually
exclusive, and `--at` cannot be used without `--fork`. JSONL, GreptimeDB, and
SQLite backends behave equivalently.

Known boundaries of a read-through fork: inherited parent entries have no
receipt (`eout1` refs are session-local, so oversized inherited fields stay
full in the request instead of being truncated), the model context never
replays rows a compaction already summarized, and older inherited segments are
read on demand rather than loaded up front. A compaction never copies inherited
messages into the fork: a retained tail that would reach into the inherited
prefix is clamped to that boundary, so inherited messages are summarized while
`retained` stays the fork's own messages.

### TUI commands

Slash commands in the TUI input line (the web UI's input exposes the same
command set through its slash-command menu):

- `/compact` — compress the context (manual compaction).
- `/rename <标题>` — rename the session.
- `/btw <问题>` — fork a side subagent for the question.
- `/fork [N]` — fork a new session from the last (or the N-th) completed
  turn boundary.
- `/undo` — undo the most recent file operation (`edit_file` / `write_file`).
- `/goal` / `/goal set <目标>` / `/goal pause|resume|clear` / `/goal continue [N]`
  — view the session's goal, create a new one (human-only), pause/resume/clear
  the current one, or arm live continuation with an optional token cap.
- `/help [命令]` — show the command list, or per-command usage detail
  (`/help fork`); an unknown command name prints a hint back to the list.
- `/model <profile> [effort|default|omit]` — switch the session's model at
  runtime (not listed by bare `/help`; the profile must be resolvable from the
  config). The optional second token overrides `reasoning_effort` for this
  session only: a literal value is sent as-is, `default` clears the override, and
  `omit` leaves the field out of the request entirely (for Codex that removes the
  whole `reasoning` object, including its summary request). Switching profile
  without a token uses that profile's configured effort and clears any previous
  override; the override is in memory only.

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

The built-in local tool set is small and capability-based: capability-relative
workspace file tools (read/write/edit) rooted at the canonical workspace,
persistent project notes under `.e-agent/notes` with deterministic,
non-persisted paging (write is unavailable to read-only roles), shell execution
with background-task listing and cancellation, and main-session delegation —
only the main agent can delegate, which caps delegation depth at 1. Web search
(plus DeepSeek's page fetch) is added to either kind of session when a backend
is configured:

```toml
[web_search]
provider = "searxng"                 # default: "exa"; or "deepseek"
base_url = "http://127.0.0.1:8088"   # SearXNG only; no API key needed
```

With the default `provider = "exa"`, the tool is registered when `EXA_API_KEY`
is set to a non-whitespace value or when `[web_search]` sets `api_key_file` /
`api_key_env` (process env wins); queries go through the Exa Context API.
With `provider = "searxng"`, it is registered from `base_url` alone — SearXNG
needs no key, and setting `api_key_file` / `api_key_env` alongside it is a
config error. SearXNG queries are `GET {base_url}/search?q=...&format=json`,
and the `results` array (title/url/content) is returned as plain text. To stand
up a local instance (uv + systemd `--user`), run
`scripts/setup-searxng.sh`; it prints the config snippet above when done.
An unknown `provider` value is a config error. OpenCode's own Exa configuration
is not inherited, and the backend never falls back to another provider: switch
`provider` explicitly.

With `provider = "deepseek"`, `api_key_file` / `api_key_env` supply the
DeepSeek key (same rules as Exa, again with a configured process env winning),
and a **non-empty** `base_url` is a config error because both endpoints are
fixed at `https://api.deepseek.com/v1/tools/{search,fetch}` (an empty or
all-whitespace `base_url` counts as unset, not as a conflict). This backend
registers two tools: `web_search` posts the query with a fixed `count = 5` and
`need_content = false` and returns title/URL/summary(/date) as plain text, and
the separate `web_fetch` tool takes one required `url` and returns the page
title plus Markdown. Both truncate output at 64 KiB and redact the key from
upstream error text before that text is truncated. `web_fetch` validates only
that the URL starts with a lowercase `http://` or `https://` prefix and is at
most 8192 characters: it does not parse or check the rest of the URL, and it
does not reject private-network or `localhost` targets, because the target URL
is handed to the DeepSeek side to fetch. `web_fetch` exists only on the
DeepSeek backend (Exa and SearXNG have no fetch endpoint); like `web_search`,
it is available to read-only sessions and subagents because it is a network
read. Configured local MCP servers may add their tools to the main agent
separately.
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
(subagents and btw forks never see it). The injection order is: main role template (`.e-agent/agents/main.md`, with
legacy and global role fallback) → workspace `AGENTS.md` → skills → MCP server
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

`[code_mode] enabled = true` in the **user-level** config (a project `.e-agent/config.toml` cannot carry it) registers the experimental `run_rust` tool for the **main agent only** — subagents never inherit it; read-only main sessions get it only when a `[sandbox]` policy exists to narrow. It compiles one model-supplied Rust source (edition 2021, std only, ≤ 128 KiB) and runs it, both stages in a forced Linux bubblewrap sandbox:

- Inherits the resolved `[sandbox]` policy: workspace per `workspace_writable`,
  configured paths/mounts keep their semantics, `.git` always read-only, network
  always off, minimal PATH/locale env, and no credentials. Anonymous `/tmp`
  writes land in a private 0700 scratch that is removed only after confirmed
  teardown; on any teardown uncertainty it is retained/quarantined and reported,
  never force-deleted, so cleanup can fail but never follows links or mutates
  anything outside it. Authorized real-path writes land on the host.
- The compile stage mounts the host rustc/rustup toolchain; the run stage gets
  PATH `/bin:/usr/bin` and no toolchain mounts added by `run_rust`, so
  Cargo/crates are not supplied at run time (a prebuilt binary still runs on
  system runtime libraries). No Cargo/crates, stdin, background/daemon
  execution, or custom env/mount/cwd/timeout; 60-second compile and 30-second run
  timeouts kill the process group, reap, and confirm the group is gone before
  removing the scratch; output is capped at 24 KiB head + 8 KiB tail. The result
  reports the source SHA-256, the stable commands, both stage statuses, and
  captured output — the source text is not repeated.
- Enabling `[code_mode]` fails closed at startup without Linux+bwrap+rustc;
  experimental, Linux-only, main-agent-only, with no sandbox-escape model claimed
  beyond that bwrap policy.

Every `web_search` query is disclosed to the configured backend — Exa, the
SearXNG instance from `[web_search] base_url` (which forwards it to its
upstream search engines), or DeepSeek. `web_fetch` (DeepSeek only) also sends
the requested URL to DeepSeek, which retrieves the page. Never include
credentials, tokens, private repository contents, customer data, personal data,
private issue text, or internal URLs. Returned text is untrusted web content:
it may contain prompt injection, insecure code, or false claims. Apart from
`web_fetch`'s explicit URL argument, the tools do not open links, execute
returned code, or fetch arbitrary URLs. This guidance is not a sandbox and
cannot guarantee that a model will not disclose sensitive context.

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

Every session can carry at most one current goal — a minimal persistence
layer (a deliberate subset of a DSH-style goal system, with no scheduler or
verifier). Fields: `id`, `revision`, `objective`, `success_criteria`,
`status` (`active|paused|blocked|completed`), `progress`, `evidence`,
`blocked_reason`.

- **Human-only creation**: `/goal set <objective>` (TUI/REPL/web), or
  `POST /api/sessions/{id}/goal` with `{"action":"set","objective":…}`.
  Creation is rejected while a non-completed goal exists; a completed or
  cleared goal frees the slot. The model never creates goals.
- **Model updates**: the goal tools read the current goal and apply transitions
  (`progress`, `pause`, `resume`, `block`, `complete`, `clear`), each against
  the `id` + `revision` the model read, so a stale revision is rejected instead
  of overwriting a newer goal. The read returns the FULL machine-usable
  snapshot (all fields as JSON), while the provider context keeps a short
  ≤400-char projection. `update_goal` can replace `success_criteria` and append
  `evidence` (both must be string arrays). `complete` requires non-empty
  `evidence` in the SAME call — pure analysis may use an explicit
  `unverified: <analysis>` string; a completed goal keeps its evidence.
  `resume` unblocks paused OR blocked goals (clearing `blocked_reason`).
- **Persistence**: every change appends a complete snapshot, and a cleared goal
  appends a tombstone. The latest snapshot wins on resume and survives
  compaction: a short projection is prepended to every provider context, so the
  goal is never lost across compaction/resume/restart. After a clear, the
  context injects an explicit `none (cleared)` override so a stale compaction
  summary can never re-introduce the old goal.
- **Forks inherit**: `--fork`/`/fork`/btw forks inherit the source prefix up to
  the fork boundary, including goal updates before it; the forked session picks
  up the newest snapshot.
- **Explicit goal continuation**: `/goal continue` arms an in-memory indefinite
  driver; `/goal continue N` arms it with a cumulative actual-token soft cap. An
  active goal alone never auto-arms. The driver continues after ordinary text,
  empty text, no-tool rounds, and unchanged revisions, and resumes after
  required background follow-up. Usage is charged as input plus output tokens
  with saturation; a capped driver may overshoot on its final call but never
  starts another call after reaching the cap. Missing usage under an explicit
  cap fails closed. Driver state is live-only and never persisted; a restart
  starts unarmed. A prompt takes precedence over the next automatic call
  without implicitly disarming the driver; Cancel, inactive/cleared goals,
  unrecoverable errors, closed sessions, and budget exhaustion disarm it.
- **Own background work suspends the next automatic turn**: a *fresh*
  automatic Goal turn starts only while this session has no own non-detached
  background work — bash/pwsh tasks (`running_background`) and live
  `delegate`/btw children, which stay outstanding until their completion entry
  is durably committed and, for an idle interactive btw child, until its
  conversation closes or is cancelled. The interrupted turn's own model rounds,
  same-turn queued maintenance (compaction, agent message, or goal command),
  human input, the ordinary completion reaction, and `FinishWhenIdle` are
  deliberately NOT gated: a Goal turn that starts its own background work still
  finishes that turn, and only the next automatically admitted turn waits for
  it. This child set is Goal-admission state only — it is not a task registry,
  never cancels or freezes already-running work, and adds no scheduler,
  priority, or generalized reservation.
- **A fresh automatic turn announces itself durably**: each genuinely new
  automatic Goal turn commits one model-facing Notice
  (`[goal continuation] Check the current goal …`) before its first provider
  call: it asks the model to re-read status/remaining work/evidence and then
  continue the relevant work or update the goal, instead of repeating its
  previous answer. It is an instruction to the model, not a guarantee that the
  model complies or reports truthfully. It is durable (a resumed or
  late-attached view replays the audit line), not a user prompt and not a
  UI-only display, and it never arms a separate reaction request. One per fresh
  automatic turn: queued prompts, waiting-input answers, cap exhaustion,
  inactive goals, and cancelled sessions do not generate a new reminder of their
  own. Replaying it never arms or wakes the driver — a restart still starts
  unarmed.
- **Subagent isolation**: subagents get the goal tools but their runner applies
  them against the subagent's own (usually empty) goal state — a subagent can
  never mutate its parent's goal.
- **UI**: the TUI and web UI render goal updates as a notice line plus a fixed
  GoalBar (`🎯 [status] objective`); the UI shows evidence/`unverified` only,
  never a claim of independent verification.

Non-goals (deliberately out of scope): todo/plan/workflow stages, deadlines,
reminders, goal DAGs / multiple goals per session, goal-specific
verification (the verifier role independently checks risky product changes
before integration), and any background-work scheduler, priority ordering, or
generalized work reservation — the delegate/btw child set used by Goal
admission is admission state only.

## Session output receipts (`eout1`) and the `read_output` tool

Sessions persist entries losslessly; only oversized provider-request fields are
shown as a UTF-8-safe head/tail projection with a `read_output` ref. New refs
are durable direct numeric references: `eout1.<entry-id>.<field-code>` (for
example `eout1.12.t`). They persist across restart because they name a durable
entry in the current session: JSONL uses its 0-based nonblank entry ordinal;
SQLite and Greptime use logical session-local `seq` and read that seq's latest
row. Field codes are `b` background output, `t` tool content, `n` notice text,
`a` assistant content, `u` user content, `s` compaction summary, and `r`
compaction retained data. The runner always binds a direct ref to its current
store, workspace, and session; a ref contains no backend or session selector.

Historical self-contained `eout1.<base64url JSON>` refs remain accepted (their
location/field/size JSON is parsed directly and any old `m` member is ignored),
so persisted history containing old long refs stays readable.

`read_output` is always available and takes `ref`, byte `offset` (default 0),
and `limit` (default 12 KiB, maximum 32 KiB). It returns a UTF-8-safe JSON page
with offsets, full-field SHA-256, and `next_offset`; the newest `read_output`
tool result is kept full in the next provider request so the requested page is
actually visible to the model.

Non-goal (this stage): the total context-window estimator that degrades
*many medium* messages, and per-tool output-cap expansion for producers that
already discard bytes (bash/MCP output caps stay as they are — persisted
originals are the precondition for exact retrieval). Bounding only applies
to fields that are certainly already persisted before replay.

## History tool

The always-available `history` tool has `list`, `read`, and `search` actions.
`list` returns newest logical entries by `seq` (default limit 20, maximum 100),
`read` returns one complete entry by `seq`, and literal, case-sensitive
`search` examines User content, Assistant text and tool calls (tool name plus
the unparsed argument JSON), and Notice text. Recovering a lost edit means
searching a distinctive fragment of what was written — a function name or
another unique string — not `"write_file"`, which lists every write. A `search`
is bounded to 60 seconds; expiry returns an explicit timeout error (never an
empty or partial result page), so narrow the scope or query.

An omitted `scope` normalizes to `session`, and every call returns
`workspace_id` and `session_id` provenance on each entry; `list`/`search`
additionally return `next_cursor`. Selector defaults resolve against the
runner-bound session: in session scope, an omitted `workspace_id` means the
current workspace, and an omitted `session_id` means the current session only
while the selected workspace is the current one — a foreign `workspace_id` must
name its `session_id`. Supplying IDs without a `scope` is the same as `scope:
"session"`.

Explicit scopes query the same configured store read-only:

- `scope: "global"`: list/search transcripts across workspace IDs in that store.
- `scope: "workspace"`: list/search one `workspace_id` (defaults to current).
- `scope: "session"`: list/search/read one session, within `workspace_id`
  (defaults to current) and `session_id` (defaults to current when the
  workspace is current).

```json
{"action":"search","query":"9004","scope":"session"}
{"action":"search","scope":"global","query":"9004","limit":20}
{"action":"search","scope":"workspace","workspace_id":"/work/db","query":"9004"}
{"action":"read","scope":"session","workspace_id":"/work/db","session_id":"review","seq":42}
```

Results include `workspace_id`, `session_id`, `seq`, and the complete logical
`entry`; pass a non-null `next_cursor` with the same action, scope, selectors
and query to continue (equivalent omitted and explicit selectors are
interchangeable). Ordering is workspace ID ascending, session ID ascending,
then sequence descending — not cross-session chronological order. Search scans
the full logical history, so `limit` caps the page, never the scan, and no
fixed window silently hides older matches; `next_cursor` is a live view, not a
frozen snapshot, so entries added before the cursor require a fresh query.
Limits are entry counts, not bytes: one complete transcript is loaded at a time,
so a very large single session can still require substantial memory.

Database scopes use only the existing connection; they do not create/resume
sessions, run schema setup, or select another database. SQLite global scope
covers only the configured database file. JSONL has no cross-workspace catalog:
its global scope covers transcript files in the current root, including those
without metadata sidecars; foreign workspace selectors return an error and
never open model-supplied filesystem paths. Backend failures are logged
internally but sanitized in model-facing errors.

Non-goals: physical-row inspection, arbitrary SQL or backend selection,
federating database instances or JSONL roots, searching Tool result content,
reasoning, or other non-User/Assistant/Notice fields, persistent search
indexes, and a separate history storage or truncation layer. Cross-session/
workspace access, formerly excluded, is now explicitly available through
scopes within the configured store.

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

Background completion records may carry cancellation provenance (`user` for an
authenticated API/TUI cancellation, `agent` for the model cancellation tool,
`system` for a configured timeout or cleanup); it annotates existing records
only, and teardown creates none.

Usage dashboard accepts `from`/`to` (default: the last 7 days, maximum 366
days, half-open `[from,to)`), `bucket=hour|day` (UTC), optional
`root_session_id` (the root and its direct children only; no recursion),
`model`, `role`, and `kind` filters, and `top_n=1..100` (default 20). Totals,
trend, and all three dimensions are complete for the filtered range; only
`top_sessions` is bounded. Each top-session row represents one session;
`models` and `kinds` are sorted arrays. Usage without session metadata uses
the stable role value `unknown` (also accepted by the `role` filter). `null`
means the provider did not report an enrichment metric, not zero; total is
input plus output only. Dashboard fields contain no transcript/reasoning text,
configuration, database credentials, arbitrary SQL, or write capability.

```sh
curl 'http://127.0.0.1:8766/api/usage/dashboard?bucket=day&top_n=10'
```

The web UI's usage view also shows an account row above its filters, backed by
the authenticated `GET /api/provider-quotas`. It returns one card per distinct
`(endpoint URL, api key)` account; providers whose `base_url` host has no known
usage endpoint are skipped, and ChatGPT is recognized by its login credentials
rather than by host (DeepSeek/SiliconFlow balances, Zhipu/GLM Coding Plan and
Kimi Code quota windows, OpenCode Go rolling/weekly/monthly percentages, and
ChatGPT/Codex plan and rate windows through the existing `e-agent login`
credentials). The endpoint caches nothing and has no background poller: each
request queries the live upstream APIs concurrently, and one provider's failure
is reported as that provider's own card. Quota windows are drawn as **remaining**
credit — a full bar means plenty left, the color tracks what is left (>30%
normal, 10–30% warning, ≤10% danger), the used percentage stays in the
tooltip and `aria-valuetext`, and a window with a missing upstream percentage
renders as unknown instead of a full bar. Non-goals: billing history, cost
accounting or price calculation, auto top-up, arbitrary user-supplied usage
endpoints, and background quota monitoring or notifications.

SSE connections are kept alive with 15-second heartbeat pings and carry live
display events alongside durable ones. Ctrl-C shuts the server down gracefully:
SSE streams self-close on the shutdown signal and in-flight requests get a hard
2-second drain deadline before the process force-exits; a second Ctrl-C kills it
outright.

### Multi-workspace launcher (development)

`scripts/multi_web.py` can launch several local web servers from a JSON config;
`scripts/multi_web.example.json` is a development example, not a replacement for
existing configs. The example's explicit `../target/debug/e-agent` command is
resolved relative to the config file, so keep the example under `scripts` or set
an explicit debug-binary path (including `CARGO_TARGET_DIR` if used). Build it
from the repository root with `cargo build --locked --bin e-agent`; the launcher
does not build implicitly, and Rust changes require a rebuild and launcher
restart (for frontend changes use the web UI's refresh action). The runtime still
reads UI assets from the Rust source checkout, which must remain available. The
example's landing port is 18765 to avoid common preview-port collisions, and
browser opening is off by default; adjust both for your local use.

For an optional per-user systemd service, edit
`scripts/e-agent-multi-web.service.example` for your checkout and config, install
it with `install -Dm644 scripts/e-agent-multi-web.service.example
"$HOME/.config/systemd/user/e-agent-multi-web.service"`, and run `systemctl
--user daemon-reload`; explicitly enable/start it only if desired. These are
manual instructions, not launcher actions: it runs one foreground launcher, not
a build or a supervisor per server. Children start sequentially, failures go to
the journal, unhealthy children are not restarted automatically, SIGTERM stops
only that launcher's children with bounded per-child waits, and
`Restart=on-failure` applies only to the launcher process.

The e-agent startup log includes its server token. The sample service inherits
child output into the user journal; treat journal access as private and do not
share logs containing startup output. This does not change HTTP logging or
expose tokens through the launcher landing page. Existing servers are never
adopted: before a planned cutover, arrange to stop the old instances and decide
how to preserve their live sessions. Do not point a new launcher at occupied
ports expecting it to take them over. Per-workspace session storage remains
whatever each server's own configuration selects.

A known limitation of `DELETE /api/sessions/{id}` (pre-existing, tracked
separately): for a WaitForInput web session it can leave an idle runner
behind. `cancel` is a *release* that parks a WaitForInput session at Idle,
and any other holder of the live session (e.g. an in-flight request) keeps
the session's runner task alive past its removal from the registry — an
unaddressable idle runner that only ends when the last holder drops it or
the process exits. The transcript stays; a later resume still works.

## Config hot reload

The headless server (`--serve` / `web`) and the TUI watch the config files (the
global `config.toml` plus the project override
`<workspace>/.e-agent/config.toml`) and, when one changes, re-read and
atomically swap the effective config. Per-source TOML leaf edits are reported
for `origin=global` and `origin=project`: each names its stable TOML path,
add/remove or before/after values, and a status of `effective` (the source wins
the overlay and feeds a reload-aware consumer for future/new sessions — it does
not promise provider behavior or mutate existing sessions), `shadowed`
(explaining the overriding or restart-only rule), or `unknown` (precedence wins,
but resolution, canonicalization, clamping, or defer behavior prevents a
stronger claim). Diagnostics use sanitized snapshots: credential values are
never retained or compared, and MCP environment values plus provider
compatibility fields are redacted. No-op and mtime-only reloads still validate
and refresh runtime state but emit no source edit; rejected reloads keep the old
runtime and emit no success diff.

- `[models]` / `[providers]` / `[roles]` — new profile definitions are
  immediately available to the web `/model` autocomplete, `POST
  /api/sessions/{id}/model` and TUI `/model` switches, and newly built sessions
  start with the edited default and role routing.
- `[mcp]`, the `[bash]` foreground residence threshold, and the `[background]`
  timeout — winning edits apply to sessions built after the reload; existing
  MCP connections are unchanged. `[bash] timeout_secs` defaults to 30 seconds:
  longer commands continue in the background instead of being killed, `0`
  disables automatic adoption and runs foreground commands to completion, and
  explicit `background: true` / `detached: true` behavior is unchanged. Adoption
  needs a live completion channel (otherwise the command stays foreground so its
  result is not lost), uses the facade's configured shell where supported, and
  is not bounded by `[background] timeout_secs`; sandboxed Windows commands keep
  the previous behavior and are killed when the threshold expires.
- Existing sessions, startup file capabilities, TUI input mapping, and the
  session backend are unchanged.

Restart required: `[sandbox]` scalar policy and workspace/file capabilities are
wired at startup (sandbox path edits are retained as unresolved source
diagnostics), the `[session]` backend connects at startup, a `[web_search]`
backend change injects the resolved key or SearXNG `base_url` into the process
environment once at startup, and `AGENTS.md`/skills instructions plus existing
sessions' models stay as loaded (a session keeps its model until a `/model`
switch).

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
  variable: its `base_url` comes from the config file only.
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

Sessions persist only image references (hash plus MIME type) — never base64.
At send time the wire re-reads the cached file and builds the provider format
(OpenAI-compatible chat image parts, or ChatGPT Responses input-image parts). An
image-bearing tool result is encoded natively on both wires: the chat wire emits
tool results as plain `role:"tool"` text and then appends at most one aggregated
temporary user image message per consecutive tool batch (wire-only, never
persisted), while the Responses wire returns the image inside that tool result's
output parts. A missing cache file degrades to a `[image missing: <hash>]` text
placeholder instead of failing.

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

### Image generation (`generate_image`)

The main agent (never read-only sessions, subagents, or btw forks) can
generate an image from a text prompt on **Linux and Android only**:
`generate_image {prompt, path, transparent_background?}` POSTs directly to the
ChatGPT Codex image endpoint
(`https://chatgpt.com/backend-api/codex/images/generations`, model
`gpt-image-2`, quality/size `auto`) with the shared ChatGPT login — the same
lazy credential cache `auth = "chatgpt"` profiles resolve through, so an
`e-agent login` after startup is picked up on first use.

Every other platform (Windows, macOS, BSD, …) is unsupported: the tool is
refused after path/policy checks and before any request (nothing is spent) with
an explicit "unavailable on this platform" error. Only the Linux/Android
exclusive-create path can publish without following a parent symlink that
appeared after the preflight.

`path` must be a workspace-relative `.png` path with only normal components
(checked lexically: no `.`, `..`, empty, absolute, drive or backslash forms).
An occupied directory entry (including a dangling final symlink), a symlinked
parent component, or a policy-denied target (read-only workspace, read-only
external entry, protected policy file) is refused before any request is sent;
those are policy checks, not filesystem-permission probes, so a permission
failure can still surface at the final write. The final write is an exclusive
create (`O_EXCL`), so a race can never overwrite an existing file; if the
process cannot finish writing, a partial new file may remain — the error says
so, and no automatic removal is attempted. Image bytes are structurally checked
(PNG signature + IHDR, nonzero dimensions — not decoded) before the file becomes
visible, and the result names only the relative path and byte count, never
base64. Response and decoded image sizes are boundedly capped, redirects are not
followed, and there is no automatic retry after an ambiguous network failure; a
failure reports only the HTTP status and a short safe provider request id
(≤128 identifier characters; omitted otherwise) — never the response body —
and HTTP 401 returns "run `e-agent login`" as a normal tool error.
Credential-resolution failures always surface as one fixed actionable login
message, so the auth chain's own diagnostics cannot reflect file contents. The
request uses native HTTP, which the `[sandbox]` file-tool policy does not wrap
(like `web_search`).

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
- `running_tasks` — a projection of the live in-flight background
  bash/delegate tasks, not an execution ledger; a resumed session finds the
  rows a previous process left behind and reports them as killed.
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
- `event_time` is wall-clock time with microsecond precision; within one process
  a monotonic counter keeps it strictly increasing (same-µs or a backward clock
  → prev+1). Per seq only rows with the latest `event_time` are retained, and at
  the same maximum `event_time` identical retried entries are folded while
  divergent ones are a hard error. Canonical order is `event_time ASC` with
  `seq ASC` only as a tie-breaker; seq stays identity/continuity metadata.
- No transactions — atomicity is per multi-row INSERT statement
- **Concurrent-write detection is fail-closed, point-in-time, not atomic**:
  pg-wire has no transactions and no conditional write or lease, so the max-seq
  pre-check, the INSERT, and a post-insert re-read are separate statements.
  After committing, every written row must still be the physical winner for its
  seq; if a foreign writer superseded or diverged a row at verification time the
  append fails with a `concurrent write conflict` instead of latest-winning
  silently or advertising an unreachable receipt location. A foreign writer can
  still commit at the same seqs after the verification query returns — a race
  irreducible without a server-side lease.
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
2. Before planning, the DB's seq values must be strictly 0..N continuous:
   gaps cause a hard error, and divergent duplicates at one seq are rejected
   while identical retried entries are silently folded.
3. A DB with **more** entries than the JSONL file errors out without writing —
   it will never truncate.
4. A DB prefix that **differs** from the same-length JSONL prefix is refused,
   so import only ever appends a matching prefix.
5. The tool re-reads the DB immediately before writing to detect concurrent
   changes and errors out if it changed. It is still **not safe for concurrent
   writers**: a writer can race between that check and the INSERT, so keep the
   target session idle during import.
6. Large imports use separate INSERT chunks; if a later chunk fails after an
   earlier one commits, some entries are written. Fix the cause and re-run —
   the append-only prefix check makes this safe.
7. Within a seq, the latest `event_time` row wins. Identical retried entries at
   the same time are folded; divergent entries at one seq are a hard error —
   stop writers, inspect with SQL, and resolve manually.
8. `--dry-run` still connects (and may execute CREATE TABLE) but inserts no
   rows.

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

Termux service setup does not install APKs, bypass Android permission prompts,
configure ADB, or promise survival of Android process killing or phone reboot.

This is deliberately not a daemon, JSONL protocol, subagent
framework, permission framework, plugin host, generic provider/auth framework,
parallel tool executor, task scheduler, priority system, or concurrency pool.
It deliberately does not fetch provider/model catalogs (including
models.dev), generate configuration, cache provider metadata, or infer
context windows; TOML profiles are local static settings only. `AGENTS.md`
loading is workspace-root-only: there is no parent/nested discovery or merging.
Subagents exist but are deliberately minimal: no arbitrary agent-to-agent messaging (only direct live parent/child `send_message`),
no delegation deeper than 1 level, and no process-level isolation yet
(subagents are runtime tasks, not subprocesses).
Cancellation is a *release*, not a termination: `cancel` preempts the
in-flight operation and never ends the session — the only hard termination
is `DELETE /api/sessions/{id}` or the tasks-panel cancel (which aborts a
subagent through its parent's background-task registry). Idle-runner
lifecycle management (idle timeouts / GC for parked sessions) is not in
scope.
No shell-command classifier and no forced turn ending/continuation for
background waits; the do-not-wait reminder only changes where the guidance is
delivered (a durable user-shaped Notice on the next request, never trailing
tool-result text and never an extra model call). Interrupted or cancelled
batches do not emit this reminder.
Windows restricted-token MVP is only an accident-prevention write restriction:
no read isolation, network isolation, Job Object, AppContainer, broker/IPC,
ConPTY, dedicated user, protected-git shell execution, or bwrap-equivalent
filesystem view. Everyone/logon-SID writable public locations can remain
writable, synthetic capability ACEs persist, and cancellation currently
terminates only the top-level process.
Sandboxed GPU access (`gpu = true`) is limited to mounting host GPU resources:
the device nodes plus the three read-only AMD ROCm sysfs subtrees that make
AMD enumeration work (granted unconditionally under the flag, not
vendor-detected). There is no per-host PCI/DRM node enumeration (host-specific
topology would rot), no runtime installation, no device filtering or GPU
selection, and no new NVIDIA mechanism beyond the existing `/dev/nvidia*`
binds.
`run_rust` (experimental `[code_mode]`) has no Cargo/crates, network access,
stdin, background or daemon execution, or custom env/mount/cwd/timeout;
Cargo/toolchain is not supplied on the run-stage PATH/mounts (compile-only
toolchain exposure, removed after the compile stage). A failed or uncertain
teardown retains/quarantines its scratch path rather than deleting it.
Workspace and configured-path writes are NOT blocked by design: they fully
inherit the resolved `[sandbox]` policy like bash. Main-agent-only, Linux-only,
opt-in.
It does speak MCP to local stdio servers (tools only), but it does NOT do
remote MCP over HTTP/SSE, MCP OAuth, MCP resources/prompts, server-initiated
notifications, `listChanged` refresh, server restart on crash, or concurrent
server initialization.
It has one model seam and one tool seam. Main-agent tool rounds are unlimited
unless `--max-rounds` sets an explicit cap; subagent tool rounds are unlimited.
Config hot reload is deliberately scoped: `[models]`/`[providers]`/`[roles]`
(and anything else read at session-build time) hot-reload in the server and
TUI via mtime polling with validate-before-swap, but `[sandbox]`, the
`[session]` backend, and the web-search backend env injection stay
startup-fixed and require a restart. Reload emits sanitized per-source TOML leaf diagnostics;
uncertain changes are `unknown`, and credential values are never compared.
There is no reload HTTP endpoint, no watch(1)/inotify, and no per-section partial reload (a bad edit is rejected
wholesale and the last good config is kept).
Reasoning-model `reasoning_content` is persisted in the session for
display/audit; it is never sent back to the API, except by an explicit
`deepseek_compat = true` thinking-mode tool-call profile (see Run).
Web search deliberately has no browser, crawler, citations, domain filters,
provider registry or provider trait, fallback chain between backends, retries,
cache, background search, or remote MCP support: it has exactly the three
config-selected backends documented above (`exa`, the default; `searxng`; and
`deepseek`, which additionally registers the separate `web_fetch` tool) and no
per-engine behavior differences are special-cased.
`web_fetch` is deliberately one DeepSeek-side retrieval of the single
model-supplied `http(s)` URL: no JavaScript/browser rendering, no crawling or
link following, no robots.txt handling, no per-site rules, no retries, and no
cache. The one SearXNG diagnostic exception: when `results` is empty
while `unresponsive_engines` is non-empty, the tool output names the failed
engines and reasons instead of a bare `no results`, so a broken backend is
distinguishable from a genuinely empty query; a non-empty result set is
returned unchanged.
Image generation (`generate_image`) is deliberately one concrete tool: one
fixed model/quality/size, no CLI, SDK, account store, API-key fallback,
generic provider framework, or per-call knobs beyond the prompt and
transparent background; no URL fetching, automatic retries, or idempotency
keys after an ambiguous failure; no inline base64 in the result or the
session log; and Linux/Android only — no degraded publisher on Windows or
any other platform.
The web UI is a single self-contained HTML page, assembled from disk on
every request in dev builds and compiled into the binary via `include_str!`
in release builds: deliberately no frontend bundler/build pipeline, no
generic hashed-asset pipeline beyond the one bundled CJK diagram font, no HTTP
compression, no runtime asset discovery, and no KaTeX font packaging.
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
todo/plan/workflow stages, no deadlines, no reminders, no goal DAGs /
multiple goals per session, no goal verifier agent, and no scheduler or persistent workflow driver —
`/goal continue` is only a process-local runner loop; the goal remains a snapshot with a CAS, not a task runner. A fresh automatic
continuation waits for this session's own non-detached background work (bash
tasks and live delegate/btw children), but that child set is Goal-admission
state only: no background-work scheduler, priority queue, preemption of
already-admitted work, or generalized reservation.

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
