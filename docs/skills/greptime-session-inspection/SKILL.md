---
name: greptime-session-inspection
purpose: Inspect, diagnose, and reconstruct e-agent session data stored in GreptimeDB.
status: standalone-document
framework: not implemented
---

# GreptimeDB Session Inspection

> **Framework**: none (standalone runbook). The skills/ directory exists for future
> framework integration; this file is a self-contained skill/runbook document.

## When to use

- A session seems missing or truncated in the TUI/REPL.
- You need to cross-reference DB state against JSONL transcript files.
- Investigating duplicate, retry, or compaction artifacts.
- Validating that a newly configured GreptimeDB backend is writing correctly.
- Recovering session history from the database (read-only).

## Non-goals / safety

- **READ-ONLY by default.** Append mode (`append_mode='true'`) means UPDATE,
  DELETE, DROP, TRUNCATE, and "repair" operations are invalid or destructive.
  Never run them during diagnosis.
- This is **not** a migration tool. Existing JSONL sessions are not migrated to
  GreptimeDB by this runbook.
- Background task bookkeeping remains JSONL (`*.background.jsonl`) and is not
  stored in the database. See [Background task JSONL](#background-task-jsonl-separate).
- This runbook does **not** cover GreptimeDB cluster operations, replication,
  or backup/restore.
- Do **not** paste full connection strings containing credentials into logs
  or shared output (see [Inspect config](#2-inspect-config-backend-and-conn)).

## Investigation workflow

### 1. Identify exact session ID and workspace root

The session ID is the file stem used in the JSONL path and the `session_id`
column. In the TUI it appears in the status bar or can be read from the
`.e-agent/sessions/` directory:

```bash
WORKSPACE="$(pwd -P)"
ls -1 "$WORKSPACE/.e-agent/sessions/"*.jsonl 2>/dev/null | head -20
```

The **workspace root** is the canonical directory that contains `.e-agent/`.
e-agent canonicalizes `--workspace` when it opens the workspace, then stores
that path as `workspace_id`. Use `pwd -P` from the same workspace to avoid
symlink or `..` mismatches.

Set shell variables for the rest of the runbook:

```bash
SESSION='example-session-abc123'
WORKSPACE="$(pwd -P)"
# Copy the real connection string from the protected config into your shell.
# Do not echo it or paste it into shared output.
CONN='host=127.0.0.1 port=4002 dbname=public'
```

> **Heads-up – psql variables vs `-c`**: In this environment psql `-v` variable
> interpolation (`:'varname'`) works when SQL is supplied via standard input
> (heredoc or pipe) but is **not** expanded for queries passed with `-c`.
> Commands below that reference session or workspace values therefore use a
> quoted heredoc (`<<'SQL'`) and `psql -v` flags; commands without those values
> keep `-c` with `-P pager=off` for diagnostic friendliness.
> Shell variables (`$SESSION`, `$WORKSPACE`) are still set above and passed to
> psql via `-v`; they never appear inside SQL string bodies.

### 2. Inspect config, backend, and conn

Read the session configuration — confirm `[session] backend = "greptime"`:

```bash
awk '
  $0 == "[session]" { in_session=1; next }
  /^\[/ { in_session=0 }
  in_session && $1 == "backend" { print; exit }
' "$HOME/.config/e-agent/config.toml"
```

Expected backend:

```toml
[session]
backend = "greptime"
```

**WARNING**: The `conn` value may contain a password. Never print, grep, log,
or paste the raw connection string into shared output. Open the protected
config privately and copy the value directly into a local shell variable.
Documentation and reports must use a credential-free placeholder only:

```bash
CONN='host=127.0.0.1 port=4002 dbname=public'
```

### 3. Verify connectivity and table DDL

Test the connection and confirm the table schema matches expectations:

```bash
psql "$CONN" -P pager=off -c "SELECT version();"
psql "$CONN" -P pager=off -c "SHOW CREATE TABLE session_entries;"
```

Expected DDL:

```sql
CREATE TABLE IF NOT EXISTS "session_entries" (
   "workspace_id" STRING NOT NULL,
   "session_id" STRING NOT NULL,
   "seq" BIGINT NOT NULL,
   "event_time" TIMESTAMP(9) NOT NULL,
   "entry_kind" STRING NOT NULL,
   "payload" STRING NOT NULL,
   "schema_version" INT NOT NULL DEFAULT 1,
   "is_error" BOOLEAN NOT NULL DEFAULT false,
   "appended_at" TIMESTAMP(9) NOT NULL DEFAULT now(),
   TIME INDEX ("event_time"),
   PRIMARY KEY ("workspace_id", "session_id")
)
ENGINE=mito
WITH(
   append_mode = 'true',
   sst_format = 'flat'
)
```

Key things to verify:

- **database name**: query the database selected by the protected `conn`
  setting. If it is wrong, correct the local configuration rather than probing
  unrelated databases.

- **`append_mode='true'`**: confirms the table is append-only.

### 4. Prove DB vs JSONL persistence

Check whether data is coming from the DB or from JSONL files:

```bash
# Does the JSONL file exist?
ls -la "$WORKSPACE/.e-agent/sessions/${SESSION}.jsonl" 2>&1

# Are there DB rows?
psql "$CONN" -v sid="$SESSION" -v wid="$WORKSPACE" -P pager=off <<'SQL'
SELECT count(*) AS rows_in_db
FROM session_entries
WHERE workspace_id=:'wid' AND session_id=:'sid';
SQL
```

**Interpretation**:

| DB has rows | JSONL exists | Meaning |
|---|---|---|
| Yes | Yes | JSONL may be an **imported source** or a **legacy file**. Check `appended_at` range vs file mtime. |
| Yes | No | Strong proof of DB-only persistence. |
| No | Yes | Config uses JSONL backend, or this is an old session before the switch. |
| No | No | Session ID is wrong, workspace path differs, or data is in a different database. |

**Nuance**: File existence alone does **not** prove current JSONL writes. The
agent writes to the **configured** backend. If `backend = "greptime"`, writes
go to the DB, even if a stale `.jsonl` file remains. Fresh DB rows plus a
stale-or-absent JSONL is strong proof of active DB writes.

Also check `appended_at` vs file mtime:

```bash
echo "--- DB write range ---"
psql "$CONN" -v sid="$SESSION" -v wid="$WORKSPACE" -P pager=off <<'SQL'
SELECT min(appended_at) AS earliest, max(appended_at) AS latest
FROM session_entries
WHERE workspace_id=:'wid' AND session_id=:'sid';
SQL
echo "--- JSONL file mtime ---"
ls -l --time-style=full-iso "$WORKSPACE/.e-agent/sessions/${SESSION}.jsonl" 2>&1
```

**Important**: DB timestamps are timezone‑less (UTC). File mtime is typically
local time. Do not casually compare them without accounting for timezone.

### 5. Summarize physical / logical rows, seq range, duplicates, gaps

```bash
psql "$CONN" -v sid="$SESSION" -v wid="$WORKSPACE" -P pager=off <<'SQL'
SELECT
  count(*)                                          AS physical_rows,
  count(DISTINCT seq)                               AS logical_entries,
  min(seq)                                          AS min_seq,
  max(seq)                                          AS max_seq,
  COALESCE((max(seq)-min(seq)+1) - count(DISTINCT seq), 0)
                                                    AS gap_slots,
  count(*) - count(DISTINCT seq)                    AS duplicate_rows
FROM session_entries
WHERE workspace_id=:'wid' AND session_id=:'sid';
SQL
```

- **physical_rows** — every row in the table (may include retry duplicates).
- **logical_entries** — distinct `seq` values. The model context is built from
  logical entries, not physical rows. Duplicates do not inflate context.
- **duplicate_rows** = physical - logical. Expected to be 0 in normal
  operation; >0 indicates retries after partial write failures.
- **gap_slots** = `(max-min+1) - count(distinct seq)`. **Assumes** seq starts
  at 0 and increments contiguously. A value >0 means seq values are missing,
  which should not happen in normal operation.

### 6. Inspect latest rows safely with payload previews

View the most recent physical rows with a truncated payload preview:

```bash
psql "$CONN" -v sid="$SESSION" -v wid="$WORKSPACE" -P pager=off <<'SQL'
SELECT seq, entry_kind, is_error,
       left(replace(payload, E'\n', ' '), 120) AS preview
FROM session_entries
WHERE workspace_id=:'wid' AND session_id=:'sid'
ORDER BY seq DESC
LIMIT 10;
SQL
```

The `left(replace(...), 120)` strips newlines and truncates to 120 characters
so the output fits in a terminal without scrolling. The payload is a
`SessionEntry` JSON blob (see `SessionEntry` enum in the Rust source).

### 7. Reconstruct logical transcript (deduplicated, seq-ordered)

Use the production dedup strategy: scan newest-first by `event_time`, keep the
first row per `seq` (which is the latest write), then sort by `seq` ASC.

For ad hoc inspection, a `ROW_NUMBER()` window query is clearer:

```sql
WITH dedup AS (
  SELECT *,
    ROW_NUMBER() OVER (
      PARTITION BY seq
      ORDER BY event_time DESC
    ) AS rn
  FROM session_entries
  WHERE workspace_id=:'wid' AND session_id=:'sid'
)
SELECT seq, entry_kind, is_error,
       left(replace(payload, E'\n', ' '), 120) AS preview
FROM dedup
WHERE rn = 1
ORDER BY seq ASC;
```

> **Performance note**: The window function scans all rows for this session.
> The production code avoids it by scanning `ORDER BY event_time DESC` and
> deduplicating in the Rust application (first occurrence wins). For ad hoc
> investigation of sessions with <10K rows, the window is fine.

### 8. Inspect entry kinds, errors, notices, compactions

Grouped counts — logical-dedup aware:

```sql
WITH dedup AS (
  SELECT *,
    ROW_NUMBER() OVER (
      PARTITION BY seq
      ORDER BY event_time DESC
    ) AS rn
  FROM session_entries
  WHERE workspace_id=:'wid' AND session_id=:'sid'
)
SELECT entry_kind, is_error, count(*) AS cnt
FROM dedup
WHERE rn = 1
GROUP BY entry_kind, is_error
ORDER BY entry_kind, is_error;
```

Or a simpler physical count (usually identical when duplicate_rows=0):

```bash
psql "$CONN" -v sid="$SESSION" -v wid="$WORKSPACE" -P pager=off <<'SQL'
SELECT entry_kind, is_error, count(*) AS cnt
FROM session_entries
WHERE workspace_id=:'wid' AND session_id=:'sid'
GROUP BY entry_kind, is_error
ORDER BY entry_kind, is_error;
SQL
```

**entry_kind meaning**:

| entry_kind | Description |
|---|---|
| `message` | A `SessionEntry::Message` — user, assistant, tool, or system message. `is_error=true` only for failed Tool messages. |
| `compaction` | A `SessionEntry::Compaction` — a summary of earlier history produced by the compaction feature. |
| `notice` | A `SessionEntry::Notice` — system notices such as background task completion. |

### 9. Inspect last compaction and understand effective history

List all compaction entries (logical-dedup aware, ordered by seq):

```bash
psql "$CONN" -v sid="$SESSION" -v wid="$WORKSPACE" -P pager=off <<'SQL'
WITH dedup AS (
  SELECT *,
    ROW_NUMBER() OVER (
      PARTITION BY seq
      ORDER BY event_time DESC
    ) AS rn
  FROM session_entries
  WHERE workspace_id=:'wid' AND session_id=:'sid'
  AND entry_kind = 'compaction'
)
SELECT seq, left(replace(payload, E'\n', ' '), 200) AS preview
FROM dedup
WHERE rn = 1
ORDER BY seq ASC;
SQL
```

**How compaction works — effective history**:

- Compaction is **append-only**: old rows remain in the table. Effective
  history is reconstructed by taking the **last** `Compaction` entry (the
  one with the highest `seq`, found via `rposition` in Rust) and appending
  all entries with a higher seq.
- Physical row count is **not** model context size. The model sees only
  logical entries after dedup and compaction resolution.
- Each compaction entry stores a `summary` and a `retained` array of messages
  that survived the compaction.

### 10. Verify subagent sessions and workspace isolation

Subagent sessions are prefixed with `sub-` (from `new_id_prefixed("sub-")`).
They use the same DB table with their own `(workspace_id, session_id)` key:

```bash
psql "$CONN" -v wid="$WORKSPACE" -P pager=off <<'SQL'
SELECT session_id, count(*) AS rows, min(seq) AS min_seq, max(seq) AS max_seq
FROM session_entries
WHERE workspace_id=:'wid' AND session_id LIKE 'sub-%'
GROUP BY workspace_id, session_id
ORDER BY max(seq) DESC
LIMIT 20;
SQL
```

**Workspace isolation** is enforced by the primary key `(workspace_id,
session_id)`. Two workspaces can use the same `session_id` without conflict:

```sql
-- Query sessions across workspaces sharing the same name
SELECT workspace_id, session_id, count(*) AS rows
FROM session_entries
WHERE session_id = 'shared-session-name-xxx'
GROUP BY workspace_id, session_id;
```

The production code binds `workspace_id` at connect time, so the backend never
sees entries from other workspaces.

**Database check**: Always query the database selected by the protected
configuration. `database` and `schema` are distinct concepts in GreptimeDB;
`SHOW CREATE TABLE` confirms the table in the active connection. Do not probe
other databases unless the user explicitly identifies one as relevant.

### 11. Diagnose common failure modes

| Symptom | Check |
|---|---|
| **Session shows 0 rows in TUI** | 1. Verify the canonical workspace path with `pwd -P`.<br>2. Check the configured `dbname`.<br>3. Is the session newer than the last completed turn? Let the turn finish, then check again. |
| **Rows exist but TUI shows old state** | Persistence occurs at **turn boundaries**. The active turn's newest events are not flushed until the turn completes. Run `/compact` or send a new prompt to trigger a write. |
| **duplicate_rows > 0** | Expected after a write retry (e.g., connection drop after INSERT succeeded but before the client received acknowledgment). The dedup logic in `load()` handles this: first row per seq (latest write) wins. |
| **gap_slots > 0** | Seq values are missing. This should not happen in normal operation — seq is assigned atomically in `append()`. If it occurs, check for concurrent writers or manual interference. |
| **JSONL file is absent but DB has rows** | Config is using GreptimeDB backend. This is the correct state. |
| **JSONL file is present and recent but DB has no rows** | Config may have switched back to JSONL, or the session started on JSONL before the switch. |
| **Cannot find a session** | Verify the exact session ID, canonical workspace path, and configured database. |
| **Timestamps look wrong** | `event_time` and `appended_at` are **DB write times**, not original conversation message times. For imported JSONL history, they reflect the import time. Timestamps are timezone-less (UTC). |
| **Background tasks seem missing** | Background task bookkeeping is intentionally **separate** JSONL (`*.background.jsonl`). It is not a transcript backend failure. |

## Synthetic example

> **Synthetic data — not from any real session.** This example uses
> deliberately fictional values to illustrate the query output shape.

### Session `example-session-abc123`

Assuming a workspace at `$WORKSPACE` in the configured database, a typical small
session might look like:

| Metric | Value |
|---|---|
| Physical rows | 42 |
| Logical entries | 42 |
| Seq range | 0 … 41 |
| duplicate_rows | 0 |
| gap_slots | 0 |
| Compaction entries | 1 |
| Non-error messages | 35 |
| Error messages (is_error=true) | 5 |
| Notices | 2 |

**Latest entries** (seq 38–41):
- seq 38: user message
- seq 39: assistant response
- seq 40: tool call
- seq 41: tool result

**Compaction** at seq 20 — entries after seq 20 form the active model context.

**JSONL file** may or may not exist depending on backend configuration; see
the decision tree above.

## Useful SQL snippets (copy/paste)

Set shell variables per [step 1](#1-identify-exact-session-id-and-workspace-root),
then run any snippet below.

```bash
# 1. Exact summary
psql "$CONN" -v sid="$SESSION" -v wid="$WORKSPACE" -P pager=off <<'SQL'
SELECT
  count(*)                                          AS physical_rows,
  count(DISTINCT seq)                               AS logical_entries,
  min(seq)                                          AS min_seq,
  max(seq)                                          AS max_seq,
  COALESCE((max(seq)-min(seq)+1) - count(DISTINCT seq), 0) AS gap_slots,
  count(*) - count(DISTINCT seq)                    AS duplicate_rows
FROM session_entries
WHERE workspace_id=:'wid' AND session_id=:'sid';
SQL

# 2. Latest physical rows with payload preview
psql "$CONN" -v sid="$SESSION" -v wid="$WORKSPACE" -P pager=off <<'SQL'
SELECT seq, entry_kind, is_error,
       left(replace(payload, E'\n', ' '), 120) AS preview
FROM session_entries
WHERE workspace_id=:'wid' AND session_id=:'sid'
ORDER BY seq DESC
LIMIT 10;
SQL

# 3. Logical deduped transcript (ROW_NUMBER)
psql "$CONN" -v sid="$SESSION" -v wid="$WORKSPACE" -P pager=off <<'SQL'
WITH dedup AS (
  SELECT *,
    ROW_NUMBER() OVER (PARTITION BY seq ORDER BY event_time DESC) AS rn
  FROM session_entries
  WHERE workspace_id=:'wid' AND session_id=:'sid'
)
SELECT seq, entry_kind, is_error,
       left(replace(payload, E'\n', ' '), 120) AS preview
FROM dedup WHERE rn = 1
ORDER BY seq ASC;
SQL

# 4. Entry kind / error counts (logical-dedup aware)
psql "$CONN" -v sid="$SESSION" -v wid="$WORKSPACE" -P pager=off <<'SQL'
WITH dedup AS (
  SELECT *,
    ROW_NUMBER() OVER (PARTITION BY seq ORDER BY event_time DESC) AS rn
  FROM session_entries
  WHERE workspace_id=:'wid' AND session_id=:'sid'
)
SELECT entry_kind, is_error, count(*) AS cnt
FROM dedup WHERE rn = 1
GROUP BY entry_kind, is_error
ORDER BY entry_kind, is_error;
SQL

# 5. Last compaction (logical-dedup aware)
psql "$CONN" -v sid="$SESSION" -v wid="$WORKSPACE" -P pager=off <<'SQL'
WITH dedup AS (
  SELECT *,
    ROW_NUMBER() OVER (PARTITION BY seq ORDER BY event_time DESC) AS rn
  FROM session_entries
  WHERE workspace_id=:'wid' AND session_id=:'sid'
    AND entry_kind = 'compaction'
)
SELECT seq, left(replace(payload, E'\n', ' '), 200) AS preview
FROM dedup WHERE rn = 1
ORDER BY seq DESC
LIMIT 1;
SQL

# 6. Latest sessions for a workspace
psql "$CONN" -v wid="$WORKSPACE" -P pager=off <<'SQL'
SELECT session_id, count(*) AS rows, min(seq) AS min_seq, max(seq) AS max_seq
FROM session_entries
WHERE workspace_id=:'wid'
GROUP BY workspace_id, session_id
ORDER BY max(seq) DESC
LIMIT 20;
SQL
```

## How to tell DB vs JSONL — quick decision tree

```
┌──────────────────────────────────────────────────────┐
│ 1. Read ~/.config/e-agent/config.toml [session]      │
│    ┌──────────────────┐         ┌──────────────┐     │
│    │ backend=greptime │         │ backend=jsonl│     │
│    └────────┬─────────┘         └──────┬───────┘     │
│        ┌────┴────┐                ┌────┴────┐       │
│        │ DB rows │                │ JSONL   │       │
│        │ present?│                │ exists? │       │
│        │  │  │   │                │  │  │   │       │
│        │ Y │  N  │                │ Y │  N  │       │
│        │  │  │   │                │  │  │   │       │
│        │  │  ┌───┴──┐            │  │  └─┐  │       │
│        │  │  │ Wrong│            │  │    │  │       │
│        │  │  │ db?  │            │  │    │  │       │
│        │  │  │ path?│            │  │    │  │       │
│        │  │  │ sess?│            │  │    │  │       │
│        │  │  └──────┘            │  │    │  │       │
│        │   ┌────┴──┴────┐        │ ┌─────┴──┴────┐ │
│        │   │ DB active  │        │ │ JSONL active│ │
│        │   └────────────┘        │ └─────────────┘ │
│        │   JSONL may also exist ── stale/imported   │
│        └────────────────────────────────────────────┘│
```

## Important nuances

- **Persistence happens at turn boundaries.** During an active multi-step turn,
  the newest entries are still in memory. They appear in the DB only after the
  turn completes (or after `/compact` in the REPL). If you query mid-turn,
  allow the turn to finish first.
- **`event_time` ≠ original message time.** Both `event_time` and
  `appended_at` are DB write timestamps. For imported JSONL history, they
  reflect import time, not the original conversation time.
- **Timestamps are timezone‑less.** GreptimeDB returns them without timezone
  info. File system mtimes are typically local time. Account for UTC vs local
  offset when comparing.
- **Workspace IDs use the canonical workspace root.** e-agent canonicalizes
  the workspace before deriving `workspace_id`. Run `pwd -P` from the same
  workspace when constructing diagnostic queries.
- **Physical row count ≠ model context size.** The Rust `load()` function
  deduplicates by `seq` and resolves compaction. Model context is built from
  logical entries, not physical rows.
- **Background task JSONL is separate.** `*.background.jsonl` files in the
  `.e-agent/sessions/` directory are never stored in the DB. Their absence
  does not indicate a backend failure.

## Checklist

- [ ] Config `[session] backend` matches expectations (`greptime` vs `jsonl`).
- [ ] Connection string `conn` spells the correct database name.
- [ ] Target session ID is known and exact.
- [ ] Workspace path matches the canonical root reported by `pwd -P`.
- [ ] `SHOW CREATE TABLE session_entries` confirms the schema (append-only, matches expected DDL).
- [ ] DB connectivity verified with `psql "$CONN" -P pager=off -c "SELECT version();"`.
- [ ] Physical vs logical row count checked; duplicate_rows understood.
- [ ] Last compaction seq identified; effective history understood.
- [ ] JSONL file existence and mtime cross-referenced against DB `appended_at`.
- [ ] Subagent sessions (`sub-*` prefix) checked if relevant.
- [ ] Active turn is complete before drawing conclusions about "missing" data.
- [ ] No DROP, DELETE, UPDATE, or destructive operations performed.
