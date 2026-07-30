# GreptimeDB Session Storage Backend

## Summary

Add an optional GreptimeDB-backed session storage backend alongside the existing JSONL file backend. Controlled by runtime config (`[session] backend = "greptime"`), gated by compile-time feature flag (`--features greptime`). Default behavior (JSONL) is unchanged.

**Branch**: `omos/greptimedb-storage-research` (3 commits on `44794ce`)

## Commits

| Commit | Description |
|--------|-------------|
| `0c1f4fd` | PoC: session_greptime module + experiment results + verification harness |
| `1f08d58` | import_jsonl: incremental JSONL→DB migration tool |
| `12e0552` | Runtime backend config: SessionStore enum dispatch |

## What Changed

### New files

| File | Purpose |
|------|---------|
| `src/session_greptime.rs` | GreptimeDB session storage via tokio-postgres. connect/load/append/append_batch. |
| `src/session_store.rs` | Runtime dispatch enum (`Jsonl` / `Greptime`). No trait — per AGENTS.md. |
| `poc-harness/` | Standalone verification tools (not part of e-agent binary). |
| `GREPTIMEDB_STORAGE_MIGRATION_PITFALLS.md` | Migration pitfalls report. |
| `.greptimedb-poc/results/` | EXPLAIN ANALYZE VERBOSE outputs + query analysis. |

### Modified files

| File | Change |
|------|--------|
| `src/config.rs` | Added `SessionBackend` enum (`Jsonl` default / `Greptime { conn }`) + `session` field in Config. |
| `src/main.rs` | Creates `SessionStore` from config, passes to session operations and delegate. |
| `src/tui.rs` | Accepts `SessionStore`, replaces `Session::append` calls. |
| `src/delegate.rs` | `PersistConfig` carries `SessionStore`, persist/resume uses store. |
| `src/lib.rs` | Added `session_greptime` (feature-gated) and `session_store` modules. |
| `Cargo.toml` | Added optional `tokio-postgres` dep + `greptime` feature. |

## Usage

```toml
# ~/.config/e-agent/config.toml
[session]
backend = "greptime"
conn = "host=127.0.0.1 port=15403 dbname=public"
```

```sh
# Build with GreptimeDB support
cargo build --features greptime

# Default (no config or backend = "jsonl") — unchanged behavior
cargo build
```

## Table Schema

```sql
CREATE TABLE IF NOT EXISTS session_entries (
    session_id      STRING       PRIMARY KEY,
    seq             BIGINT       NOT NULL,
    event_time      TIMESTAMP(9) NOT NULL TIME INDEX,
    entry_kind      STRING       NOT NULL,   -- message / compaction / notice
    payload         STRING       NOT NULL,   -- SessionEntry as JSON
    schema_version  INT          NOT NULL DEFAULT 1,
    agent_role      STRING       NOT NULL DEFAULT 'main',
    is_error        BOOLEAN      NOT NULL DEFAULT FALSE,
    appended_at     TIMESTAMP(9) NOT NULL DEFAULT now()
) WITH (append_mode = 'true', sst_format = 'flat')
```

## Query Patterns (leveraging TIME INDEX optimizations)

| Operation | Query | Optimization |
|-----------|-------|-------------|
| `connect` (find max seq) | `last_value(seq ORDER BY event_time ASC) GROUP BY session_id` | LastRow scan hint → reads 2 rows instead of full scan |
| `load` (read all entries) | `SELECT seq, payload ... ORDER BY event_time DESC` | WindowedSortExec + app-side HashMap dedup (no window function) |
| `append` | Parameterized INSERT | tokio-postgres byte-identical (verified with 1MB, CJK, multiline, injection) |
| `append_batch` | Multi-row VALUES INSERT | 55k+ rows/s |

## Verification

- 134/134 project tests pass (both with and without `--features greptime`)
- 4/4 session_greptime integration tests pass (roundtrip, reconnect, dedup, batch)
- 8601 real JSONL entries imported from 84 sessions via incremental importer
- EXPLAIN ANALYZE VERBOSE confirms LastRow scan (2 rows vs 5780) and WindowedSortExec
- kill -9 durability verified (sync_write=true, 1083 rows survived)
- tokio-postgres and gRPC ingester both byte-identical (psycopg2 \n issue is Python-only)
- clippy clean, fmt clean

## Design Decisions

- **No Storage trait** — `SessionStore` is an enum, not a trait. AGENTS.md: "one adapter, no seam."
- **No dual-write** — config selects one backend at a time. Default stays JSONL.
- **No rewrite for Greptime** — compaction is append-only (Compaction entry appended at end; agent's `effective_history` finds it via `rposition`). `rewrite()` is a no-op for Greptime.
- **Background tasks stay JSONL** — `record_background_start` / `clear_background_task` / `take_unfinished_background` are unchanged.
- **append_mode = true** — no DELETE/UPDATE. Retries produce duplicates, deduplicated on read (app-side HashMap, first-wins by event_time DESC).

## Non-goals

- No migration of existing JSONL sessions at startup (use `poc-harness import_jsonl` manually)
- No background task persistence in GreptimeDB
- No distributed/cluster deployment — standalone GreptimeDB only
- No generic storage abstraction for future third backends

## Known Limitations

- GreptimeDB server must be running before e-agent starts (no auto-discovery or embedded mode)
- `tokio-postgres` needs `with-chrono-0_4` feature for TIMESTAMP params
- memtable flush requires admin API (not SQL) — all current PoC data is in memtable
- `event_time` is real wall-clock time but may have nanosecond collisions across processes (same-ns entries get prev+1)
