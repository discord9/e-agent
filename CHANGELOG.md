# Changelog

All notable changes to e-agent are tracked here. Format based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); this project
does not yet follow semantic versioning strictly.

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

- Version bumped to 0.1.1; storage-backend default decision recorded in
  TODO (phased switch to SQLite, JSONL stays optional — not yet
  implemented).

[0.1.1]: https://github.com/GreptimeTeam/e-agent/releases/tag/v0.1.1
