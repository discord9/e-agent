# Changelog

All notable changes to e-agent are tracked here. Format based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); this project
does not yet follow semantic versioning strictly.

## [Unreleased]

### Added

- **配置热重载 (config hot reload)** — the headless server and the TUI now
  poll the config files (`config.toml` + project `.e-agent/config.toml`)
  and, on a valid change, atomically swap the effective config: edited
  `[models]` / `[providers]` / `[roles]` become available to `/model`
  switches immediately and to newly built sessions; `[mcp]`, `[bash]` and
  `[background]` apply to sessions built after the reload. A reload that
  fails to parse or resolve is rejected and logged, keeping the last good
  config. The `[session]` backend and web-search key changes remain
  restart-only; `[sandbox]` reloads live (see below).
- **`/help <command>` per-command details (TUI + web)** — the TUI's `/help`
  now accepts an optional command name and shows a 2–4 line usage detail
  (`/help fork`); unknown names print a hint pointing back at the list. The
  same details are wired into the web UI's slash-command menu, keeping both
  surfaces in sync (`src/tui.rs` HELP_DETAILS ↔ `src/ui/sessions.js`).
- **Project-level `[tui]` config** — a project-local
  `<workspace>/.e-agent/config.toml` can now set `[tui]` (the submit/newline
  key mapping); the project section replaces the global one wholesale, and
  fields it omits fall back to the built-in defaults, not the global values.
- **TUI submit/newline key mapping (`[tui] submit` / `[tui] newline`)** —
  the TUI's Enter handling is configurable: each field accepts exactly one
  of `enter`, `alt+enter` (`option+enter`), `ctrl+enter`, or `shift+enter`,
  matched literally. `submit` must differ from `newline`; an unsupported key
  string or a `submit == newline` collision is a startup error listing the
  supported keys (no silent fallback).
- **Ctrl-C shutdown deadline (headless server)** — Ctrl-C now starts a
  graceful drain with a hard 2-second cap (`SHUTDOWN_DRAIN_TIMEOUT`): SSE
  streams self-close on the shutdown signal so in-flight requests finish in
  milliseconds, and the process force-exits at the deadline instead of
  waiting on a hung connection; a second Ctrl-C still kills it outright.
- **Session links and the PR/issue watcher** — a session can hold
  pull-request, issue, and plain links (`link_ref`, `unlink_ref`,
  `list_refs`, and the web UI's chips; SQLite/Greptime backends). A linked
  open PR/issue is watched while the session is live: one hidden background
  task revalidates it every five minutes and turns a REAL transition —
  checks going failing, a review decision becoming changes-requested or
  approved, a merge, a close, a new comment or review — into ONE dim notice
  that starts exactly one model turn. What a user can rely on: the add
  itself is never news and the add-time observation is the baseline (a later
  chip refresh cannot move it, so a failure between the add and the first
  poll is still reported); a restart replays nothing from before it; a burst
  of changes in one poll is one message; a merged/closed link is never polled
  again; two URLs that name one pull request (`#27` and
  `#27#discussion_r1`) report one object once; an unlink ends that link's
  baseline, so unlink → close → re-add never announces a close that happened
  while it was unlinked; and a merge or close is delivered even when its
  baseline observation was still in flight. Runs of unchanged polls cost one
  conditional read per resource plus one unconditional GraphQL request per
  pull request (GitHub publishes the aggregate review decision without a
  validator), and produce no turn. Watchers stay MAIN SESSION ONLY: a
  subagent is offered none of the three verbs and never watches anything.

### Changed

- **`[sandbox]` hot reloads (live shell policy)** — a winning `[sandbox]`
  edit reaches the next `bash` / background command of an already-running
  session without a restart; a command already running keeps its policy,
  read-only roles re-derive their narrowing (and fail closed when the policy
  disappears), and `/btw` forks share the parent's policy. The policy is
  re-resolved only when the `[sandbox]` inputs change, and such edits are
  reported `effective` instead of restart-only (a global `[sandbox]` scalar a
  project overlay overrides is `shadowed`). On Linux/bwrap a sandboxed command
  can no longer author the workspace policy file through the paths the plan
  mounts (an absent `.e-agent/` is created agent-side and projected read-only,
  and a writable mount alias whose source contains the policy directory is
  refused); the Windows write sandbox has no such carve-out, so a project
  `[sandbox]` edit is rejected by the reload there and needs a restart.
  File-tool roots and `run_rust`'s forced policy stay startup-fixed (a process
  restart).
- **Links are one session's state** — a link row is keyed by workspace +
  session + URL and needs no `sessions` metadata row, so adding one over the
  HTTP API no longer 404s for an id that has none (an id created by another
  client, a store copied to a new root); the number form still refuses an id
  with no live session, because there is no workspace whose `origin` could be
  read. Unlinking a PR in one session leaves every other session's watch of
  the same PR exactly as it was: the lifetime a watch compares itself against
  is per session now, so one session's unlink cannot silently re-baseline
  another's.
- **Sandbox `/tmp` is the host's real `/tmp` again** — bubblewrap bash no
  longer mounts a private `/tmp` tmpfs, and the delegated-subagent
  read-only `/tmp` remount is gone: inside the sandbox `/tmp` is the host
  `/tmp`, writable for the main agent and subagents alike, so tools that
  expect a shared writable scratch space (cargo, TMPDIR users) run
  directly. `run_rust`'s private scratch `/tmp` binding is unchanged.

### Fixed

- **`history` selector semantics** — a `scope: "session"` call without IDs
  now defaults to the runner-bound session instead of erroring. An omitted
  scope means session, including when IDs are supplied. In session scope,
  omitted IDs default to the current workspace/session; a foreign workspace requires an
  explicit `session_id` and is rejected before any store read. `list`,
  `search`, and `read` all take the scoped path with `workspace_id`/
  `session_id` provenance and (for list/search) `next_cursor`; no-scope
  cursors work and are bound to the resolved selectors. The legacy newest-100
  search window is gone — `limit` (default 20, maximum 100) caps the page,
  never the scan — so matches older than 100 entries are found and paginated.
  Explicit foreign scopes, ID validation, and the existing empty/not-found
  behavior are unchanged.
- **Watcher correctness (session links)** — the watcher now has a single
  observation contract, so a few classes of missed or duplicated notice are
  gone. Lists are read the way GitHub actually orders them (ascending,
  paginated to the end, with an incomplete page treated as a failed read
  instead of a short list), so a comment on page six of a busy PR is found;
  a `304` only ever says "unchanged since the document we already parsed", so
  a check or review that changed while a nested read failed is retried and
  reported exactly once; and a review decision comes from GitHub's own
  aggregate `reviewDecision` (and only from the document that actually
  carries it), never from counting individual votes.
- **Interrupted `session_refs` migration** — the one-time rebuild that made
  the URL the link identity now runs as ONE transaction. Previously a crash
  between its statements could leave every link in the old table while the
  new-key table looked migrated, so those links were never read again; now an
  interruption rolls back and the next start retries the whole rebuild.

## [0.1.1] — 2026-08-04

### Fixed

- **Vision/image handling (session poisoning)** — a `read_image` result on a
  non-vision model no longer commits a synthetic image-bearing user message
  that locked every later model call behind the wire gate (the session
  appeared permanently stuck). The runner now applies model switches queued
  during tool execution before interpreting the result, skips the attachment
  on non-vision models with a visible notice, and rejects explicit `/image`
  prompts loudly without killing the session. Already-poisoned sessions can
  be recovered with `/compact` (or plain resumption): image parts are
  stripped from requests sent to non-vision models only, while persisted
  history and retained tails keep them losslessly (switching back to a
  vision model restores them).
- **Web UI tool rendering** — `edit_file` results render as a git-style
  `-`/`+` diff with line numbers (red old / green new), `read_file` renders
  a line-numbered content view of the read range, `write_file` renders an
  all-added diff; truncation + expand, narrow-screen wrapping, and WCAG AA
  contrast applied (Solarized Light).
- **Windows write sandbox (real-use feedback)**:
  - File-level `DELETE` is now granted through a second inherit-only
    capability ACE, so delete / rename-overwrite / atomic-replace and git
    operations (index/ref updates) work from the restricted token instead of
    failing with `Access is denied` and leaving `.git/index.lock` behind.
    The root itself still never receives `DELETE`; installed ACLs are
    re-read and verified before a process starts.
  - Hard-linked descendants whose every alias stays inside the configured
    write roots are allowed (cargo `link_or_copy` no longer deadlocks first
    install); out-of-bounds aliases fail closed listing every path with
    guidance.
  - First-install ACE installation is serialized process-wide, closing the
    foreground/background race that produced transient `os error 5` write
    failures during the ACL propagation window.
  - Project-local `[sandbox]` path errors now name the offending path and
    the fix (user-level config authorization vs project-local narrowing);
    subset/narrowing semantics unchanged. README + spec document the
    hard-link policy, `~/.cargo/registry` narrowing example, and the TOCTOU
    non-goal.
- **Delegate `workspace` is now required** — the tool schema marks
  `workspace` required and `execute()` errors when it is missing or blank;
  the silent parent-workspace fallback is gone, so a forgotten workspace can
  never run a subagent in the wrong tree.
- **Subagent session titles** — the delegate task-panel label is stamped as
  the subagent session's title at creation (all backends); resumes never
  overwrite it, and the label cap is aligned to the declared 40 chars.
- **CSS regressions** — restored the truncated `katex.min.css` closing
  brace and the missing `.ws-btn` closing brace that nested ~600 lines of
  styles (message bubbles and more) into it.

### Changed

- Version bumped to 0.1.1; the default session backend is now SQLite/turso
  (`<workspace>/.e-agent/sessions.db`), JSONL stays selectable, Greptime
  remains optional. No migration from existing JSONL workspaces (legacy
  files keep working via explicit `backend = "jsonl"`).

[0.1.1]: https://github.com/GreptimeTeam/e-agent/releases/tag/v0.1.1
