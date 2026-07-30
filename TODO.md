# Temporary TODO

Temporary session tracker until the persisted Todo tool and `session_state` storage exist.

## In progress

- [ ] **Greptime latest-write dedup — final cleanup**
  - Same `seq`: latest `event_time` wins.
  - Max-time tie: equal deserialized `SessionEntry` folds; divergent entry errors.
  - Output stays `seq ASC`; importer requires strict `0..N` continuity.
  - Remaining review fixes: monotonic/checked next-seq refresh, accurate inspection SQL labels/raw payload diagnostics, remove stale README/PR semantics.
  - Commit only after final read-only review.

- [ ] **Incremental session persistence — final review**
  - Worktree: `.e-agent/worktrees/incremental-persistence`
  - User/Assistant/Tool/BackgroundCompletion/Compaction persist immediately; streaming/reasoning deltas do not.
  - Latest fixes: JSONL torn-tail repair, idempotent completion retry, idle REPL completion, resume-claim RAII, recovery-record reconciliation.
  - Awaiting final oracle verdict; then rebase/merge into main and run full gate.

- [ ] **External file capabilities**
  - Worktree: `.e-agent/worktrees/external-file-capabilities`
  - `read_file`: workspace-relative plus absolute paths under `readable_paths` or `writable_paths`.
  - `write_file` / `edit_file`: workspace-relative plus absolute paths under `writable_paths` only.
  - Preserve cap-std roots and symlink/`..` escape protection.

## Next

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

- [ ] **Undo**
  - Port old prototype after incremental persistence lands; do not merge old baseline directly.
  - Validate marker bounds, checked ordinal conversion, orphan tool results, compaction/external facts, overlap, Greptime roundtrip, README non-goals.

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

- [ ] Review `current-pr-html-preview` global skill after readable sandbox mounting is confirmed.
- [ ] Decide whether archived subagent browsing is still needed after immediate session IDs land.
- [ ] Session lazy paging remains deferred; current Greptime full-load performance is acceptable.

## Explicit non-goals

- No task scheduler, priorities, deadlines, dependency graph, worker pool, or event-sourced Todo projection.
- No per-feature state tables yet; use one typed `session_state` snapshot.
- No cross-storage transactions or multi-writer correctness claims.
