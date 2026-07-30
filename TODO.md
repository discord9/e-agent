# Temporary TODO

Temporary session tracker until the persisted Todo tool and `session_state` storage exist.

## In progress

- [ ] **Greptime latest-write ordering correction**
  - `event_time` is the canonical conversation/display/paging order; `seq` is only a tie-breaker plus auxiliary identity/dedup/integrity metadata.
  - Same `seq`: resolve the canonical record consistently; equal winning `(event_time, seq)` folds identical deserialized `SessionEntry` values and rejects conflicts.
  - After repeated-seq resolution, output `event_time ASC, seq ASC`; importer may still use `seq` continuity as a structural validation without redefining display order.
  - Correct the stale final `seq ASC` behavior in commit `7c29ba5` and update inspection/README/PR semantics.

- [ ] **Minimal incremental session persistence — final review**
  - Worktree: `.e-agent/worktrees/minimal-entry-persistence`; the broad `.e-agent/worktrees/incremental-persistence` implementation is abandoned and must not be merged.
  - Complete User/Assistant/Tool/BackgroundCompletion/Compaction entries await the existing backend append before entering history and advancing entry-associated state; streaming/reasoning deltas do not persist.
  - No new JSONL durability, background recovery, resume claims, task-ID reconciliation, provider notices, or REPL lifecycle machinery.
  - Awaiting final narrow oracle verdict; then run the normal four-command gate and merge.

- [ ] **External file capabilities**
  - Worktree: `.e-agent/worktrees/external-file-capabilities`
  - `read_file`: workspace-relative plus absolute paths under `readable_paths` or `writable_paths`.
  - `write_file` / `edit_file`: workspace-relative plus absolute paths under `writable_paths` only.
  - Preserve cap-std roots and symlink/`..` escape protection.

## Next

- [ ] **Cancel background task tool**
  - Expose the existing `BackgroundTasks::cancel(id)` registry operation as a builtin tool so the model can stop runaway bash/delegate tasks without requiring F2 keyboard input.
  - Input is only `{ "id": <task-id> }`; return found/cancel-requested vs not-found.
  - Reuse current cancellation/recovery cleanup semantics; no scheduler, priorities, bulk cancel, or worker pool.

- [ ] **Runtime build/version visibility**
  - Make the running TUI/REPL report its actual executable/build identity, not merely the currently installed `command -v e-agent` target.
  - Prefer a small `/version` or status field containing package version, build commit when available, and `current_exe`.

- [ ] **Enforce explorer read-only access at the tool layer**
  - Do not register `write_file` or `edit_file` for explorer sessions; prompt instructions are not a security boundary.
  - Explorer `bash` must run with the workspace and Git metadata mounted read-only. Shell redirection, `sed -i`, `rm`/`mv`, Git writes, and subprocesses must be unable to mutate the workspace.
  - Fail closed: if the read-only bash sandbox cannot be established, do not expose `bash` to the explorer instead of falling back to an ambient shell. Temporary computation may write only to an isolated scratch directory.
  - Apply the same enforcement to any role declared read-only, including Oracle, while keeping fixer/designer write capabilities explicit.
  - Add regression tests for direct file tools, `> file`, `tee`, `sed -i`, rename/delete, Git metadata writes, symlink escapes, background bash, and subprocess inheritance.

- [ ] **Optional OTLP metrics export**
  - Default off; enable only through explicit config and use standard OTLP endpoint/resource attributes.
  - Measure model request/first-token/total latency, token usage, tool latency/error, persistence append/load latency/error, compaction trigger/outcome, background task start/finish/cancel, and TUI event-loop/redraw lag.
  - Include low-cardinality attributes only: provider/model, tool name, backend, outcome, main/subagent role, and build identity.
  - Never export prompts, assistant/tool content, file paths, commands, session payloads, API keys, or raw session IDs. Avoid user/workspace identifiers by default.
  - Bound metric cardinality; no per-task/session labels, tracing/log export, remote control, dashboards, collector management, or always-on telemetry in v1.
  - Instrument the existing call sites directly; do not add a general event bus or telemetry trait without a second real exporter.

- [ ] **Bound long-session TUI redraw cost**
  - Keep exact relative visual-row scrolling without computing a global visual-row offset: maintain a bounded rendered window of `DisplayLine`s plus the viewport's local visual-row position.
  - Up/Down move one visual row and PageUp/PageDown keep their visual-row step. When the viewport reaches a local window edge, extend the source range backward/forward and correct only the local position.
  - At the bottom, follow newly appended/streamed output. When scrolled up, keep the local viewport fixed while output continues below; `End` explicitly resumes following.
  - Start with a conservative source range and expand only when it cannot cover the viewport. No full-history render, global visual-row offset/index, generalized virtual-list framework, or exact global content height.
  - Preserve the current simple Markdown behavior with bounded look-behind; no full CommonMark engine or table support. Cover streaming at bottom, frozen scrolled-up viewport, Up/Down/PageUp/PageDown/End, resize, attached sessions, wrapping/CJK, and fenced code within look-behind.

- [ ] **Termux touch and terminal mouse controls**
  - Support Termux phone touch through terminal mouse events, with a visible tappable background-task status/button that opens and closes the tasks panel.
  - Allow tapping a task row to select it and attach/open it; support touch/mouse scrolling in the main scrollback, attached session, and task list. Keep keyboard controls unchanged.
  - Preserve access to native terminal text selection (for example through an explicit mouse-mode toggle or modifier) instead of permanently capturing every pointer event.
  - Reuse existing panel, selection, attach, and scroll actions. No gesture framework, drag-and-drop, hover-only controls, custom touch protocol, or desktop widget abstraction in v1.
  - Test coordinate hit regions across narrow Termux layouts, resized terminals, empty/long task lists, and panel open/close behavior.

- [ ] **Verify resize behavior after local-window scrolling**
  - Re-test on a binary containing `0a9ff99` and `381c13a`: replacing the old full-history absolute visual-row offset with a local scroll window may already eliminate the severe viewport drift.
  - If a scrolled-up view still reproducibly jumps after narrower/wider resize, preserve a small local content anchor across rerender; otherwise close this item without adding code.
  - Following-bottom and attached-session resize should remain stable. No global visual-row index, full-history measurement, resize cache, or exact pixel/column preservation.

- [ ] **Paged Greptime session reads**
  - Resume/open a session by loading only its latest entry block; fetch earlier/later blocks on demand when the TUI's local visual window reaches a source boundary.
  - Treat `event_time` as the canonical conversation order. Use a stable `(event_time, seq)` cursor, with `seq` only as the tie-breaker and as auxiliary identity/dedup/integrity metadata; never order or page by `seq` alone and never use database OFFSET.
  - Resolve repeated logical records without allowing versions of one `seq` to be deduplicated only page-locally. For equal `event_time` and `seq`, fold identical deserialized `SessionEntry` values and reject conflicting values.
  - Do not require an exact total entry/visual-row count. New appends extend the time-ordered tail; already loaded blocks remain usable while the user inspects history.
  - Keep model-context reconstruction correct: compaction/context loading and transcript/tool pairing may request additional entry blocks independently of what is currently visible. Use `seq` to validate those structural relationships, not to redefine chronological display order.
  - GreptimeDB only in v1; no new JSONL paging machinery, generic storage trait, cache service, prefetch scheduler, or cross-block transaction protocol.

- [ ] **Compaction streaming TUI fix**
  - Send compact deltas to `event_handler + per-turn subscriber`, never observers/session log.
  - Manual `/compact` subscribes explicitly; auto-compaction reuses current subscriber.
  - Remove asynchronous forwarding race; drain direct queue before deciding summary fallback.
  - Successful log contains one complete Compaction; failure/cancel contains none.

- [ ] **Delegate returns subagent session ID immediately**
  - Migrate only after incremental persistence lands because both edit `src/delegate.rs`.
  - Background result: started-task line plus `subagent session: ...`.
  - Sync result: session line plus successful answer.
  - Preserve sync failure as `Err`; release resume claim on spawn failure; normalize multiline labels.

- [ ] **Oracle review: attached subagent steering UX**
  - Review the end-to-end experience of attaching to a subagent and sending it guidance while it is working; aim for the same clarity and responsiveness as interacting with the main agent.
  - Make it obvious whether input was accepted, queued for the next turn, or used to redirect current work. Show queued prompts and allow correcting/removing them when practical instead of silently stashing text.
  - Review cancel-versus-steer behavior: the user should be able to redirect execution without accidentally terminating the session or waiting through a long irrelevant turn, while completed rounds remain visible.
  - Check input focus, busy/idle state, streaming, scroll freeze, task completion, detach/reattach, and mobile/Termux touch interaction together rather than reviewing only the channel API.
  - Preserve the existing semantic contract (`Prompt` and `Cancel`) unless a concrete missing operation is proven. Do not add a third steering transport, agent-to-agent messaging, scheduler, or remote-control framework.

- [ ] **First-class main and subagent session runners**
  - Refactor main agents and subagents to use one session-runner lifecycle and the same frontend contract for events, prompt batching/steering, cancellation, status, persistence, attach/detach, and completion.
  - The runtime audit confirmed `Agent`, Model, Tool, Workspace/cap-std handles, and the `Agent::run` future are `Send`; an owned Agent can run in a `tokio::spawn(async move { ... })` session task. Top-level vs delegated child, model/role, and parent-task metadata should be configuration, not separate execution semantics.
  - Before removing the dedicated subagent thread/runtime, contain synchronous ReadFile/WriteFile/EditFile filesystem work so it cannot block shared Tokio workers; do not add a worker pool or generic executor abstraction.
  - The main TUI should interact through the same `SessionHandle` semantics as an attached subagent instead of holding a privileged second control path; remove the old transport only when parity is demonstrated.
  - Preserve isolated Agent history/state and the shared running-task registry. A future subprocess boundary may implement the same SessionHandle semantics; no nested delegate, agent-to-agent messaging, scheduler, worker pool, remote protocol, or process isolation in this refactor.
  - Do this only after current persistence, steering UX, and cancellation behavior are stable; require a narrow Oracle review of lifecycle/cancellation during implementation, not another speculative architecture phase.

- [ ] **Undo**
  - Port old prototype after incremental persistence lands; do not merge old baseline directly.
  - Validate marker bounds, checked ordinal conversion, orphan tool results, compaction/external facts, overlap, Greptime roundtrip, README non-goals.

- [ ] **Fork session**
  - Implement only after incremental persistence and Undo semantics stabilize.
  - Do not copy parent history. The child session's first entry is an immutable `Fork { source_session_id, source_seq, source_event_time }` parent pointer.
  - Resolve the parent snapshot using `seq <= source_seq` and, per seq, the latest row whose `event_time <= source_event_time`; the timestamp is both the target-row identity and snapshot cutoff.
  - JSONL uses its stable entry ordinal/seq prefix as the cutoff; cross-backend fork resolution/migration is not automatic in v1.
  - New fork gets its own session ID and independent mutable `session_state`; never inherit Todo/background recovery state.
  - Reject or repair fork points with incomplete tool-call/tool-result pairing; preserve compaction semantics through the inherited prefix.
  - A session has one immutable direct parent; recursive parents require cycle/corruption detection. No merge, rebase, live synchronization, or shared writer in v1.

- [ ] **Persisted Todo tool + mutable session state**
  - One `session_state` row per workspace/session; JSONL parity via `<session>.state.json` atomic replacement.
  - Initial real fields: `todos` and `unfinished_background_tasks`.
  - Greptime: fixed TIME INDEX plus non-append merge semantics; verify on live DB first.
  - `last_non_null`: omitted field means unchanged; clearing writes `[]`, never `NULL`.
  - Transcript/state/background operations are not cross-table transactional; single writer only.
  - Replace this file with the real Todo tool once installed and the agent is restarted.

## Ready / isolated

- [ ] **Designer role**
  - Prompt: `agents/designer.md`, based on OMO Slim UI/UX designer.
  - Model route already set globally: `[roles] designer = "chatgpt/sol"`.
  - Copy prompt to `~/.config/e-agent/agents/designer.md` for global availability, then restart.
  - Commit separately from Greptime work.

## Deferred

- [ ] **Very long-term mobile/web frontends**
  - Revisit only after the Termux TUI and touch controls are stable and there is concrete demand.
  - Reuse the existing session event and steering semantics when the frontend actually exists; do not add a frontend protocol, HTTP service, shared UI framework, or mobile/web abstraction in advance.

- [ ] Review `current-pr-html-preview` global skill after readable sandbox mounting is confirmed.
- [ ] Decide whether archived subagent browsing is still needed after immediate session IDs land.
- [ ] Session lazy paging remains deferred; current Greptime full-load performance is acceptable.

## Explicit non-goals

- No task scheduler, priorities, deadlines, dependency graph, worker pool, or event-sourced Todo projection.
- No per-feature state tables yet; use one typed `session_state` snapshot.
- No cross-storage transactions or multi-writer correctness claims.
