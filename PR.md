# GreptimeDB Session Storage Backend

## Summary

Add an optional GreptimeDB-backed session storage backend alongside the existing JSONL file backend. Controlled by runtime config (`[session] backend = "greptime"`), gated by compile-time feature flag (`--features greptime`). Default behavior (JSONL) is unchanged.

**Branch**: `omos/greptimedb-storage-research` (on base `44794ce`). Cleanup commit `3789e93` removed experiment artifacts (`poc-harness/`, `.greptimedb-poc/`) and dead code before merge.

## What Changed

### New files

| File | Purpose |
|------|---------|
| `src/session_greptime.rs` | GreptimeDB session storage via tokio-postgres. connect/load/append. |
| `src/session_store.rs` | Runtime dispatch enum (`Jsonl` / `Greptime`). No trait — per AGENTS.md. |
| `GREPTIMEDB_STORAGE_MIGRATION_PITFALLS.md` | Migration pitfalls report (evidence-locked to specific commits). |

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

## Verification

- 134/134 project tests pass (default features); 138/138 with `--features greptime` (4 integration tests run against a live GreptimeDB at 127.0.0.1:15403, dbname=e_agent)
- 8601 real JSONL entries imported from 84 sessions during the PoC (incremental importer)
- EXPLAIN ANALYZE VERBOSE confirmed LastRow scan (2 rows vs 5780) and WindowedSortExec during the PoC
- kill -9 durability verified (sync_write=true, 1083 rows survived)
- tokio-postgres and gRPC ingester both byte-identical (psycopg2 \n issue is Python-only)
- clippy clean (both feature sets), fmt clean

## Design Decisions

- **No Storage trait** — `SessionStore` is an enum, not a trait. AGENTS.md: "one adapter, no seam."
- **No dual-write** — config selects one backend at a time. Default stays JSONL.
- **No rewrite for Greptime** — compaction is append-only (Compaction entry appended at end; agent's `effective_history` finds it via `rposition`). `rewrite()` is a no-op for Greptime.
- **Background tasks stay JSONL** — `record_background_start` / `clear_background_task` / `take_unfinished_background` are unchanged.
- **append_mode = true** — no DELETE/UPDATE. Retries produce duplicates, deduplicated on read (app-side HashMap, first-wins by event_time DESC).

## Non-goals

- No automatic migration of existing JSONL sessions at startup (a one-shot importer was built and used during the PoC; removed before merge as an experiment artifact)
- No background task persistence in GreptimeDB
- No distributed/cluster deployment — standalone GreptimeDB only
- No generic storage abstraction for future third backends

## Known Limitations

- GreptimeDB server must be running before e-agent starts (no auto-discovery or embedded mode)
- `tokio-postgres` needs `with-chrono-0_4` feature for TIMESTAMP params
- `event_time` is real wall-clock time but may have nanosecond collisions across processes (same-ns entries get prev+1)

## Review Notes (two rounds, all resolved)

Round 1 (self-review) — items fixed in cleanup commit `3789e93`:

1. ~~Dead code: `append_batch` / `next_seq()` unused by production.~~ **Fixed**: removed.
2. ~~Module header said "not a replacement yet".~~ **Fixed**.
3. **Accepted — rewrite() no-op for Greptime**: legacy sessions loaded from DB get `legacy=false`; unreachable in practice, behavior correct.
4. **Accepted — per-subagent DB connections**: one connection per live subagent; bounded, fine for single-user agent.
5. ~~`merge_mode = 'last_non_null'` ineffective under append_mode.~~ **Fixed**: removed from DDL.

Round 2 (oracle review) — items fixed in commits `7d7e648` and `8dc6054`:

6. ~~**Critical — subagents shared the main session's bound store**: Greptime `SessionStore` is bound to one session_id at connect; cloning it into `Delegate` wrote all subagent transcripts into the main session.~~ **Fixed**: `Delegate` now carries `SessionBackend` config and each subagent connects its own session-bound store; resume connects a temporary store bound to the resumed id.
7. ~~**High — append partial commit**: per-row INSERTs advanced `next_seq` for a committed prefix while callers treated the call as all-or-nothing; retries re-sent the slice with shifted seqs. First fix attempt used a transaction, but live-wire testing proved **GreptimeDB pg-wire has NO transaction support** (server warns "transaction is not supported"; ROLLBACK is a no-op).~~ **Fixed**: single multi-row parameterized INSERT per chunk (verified: any row failure → zero rows committed), `next_seq` advances only on success, so retries reuse the same seq range and read-side dedup handles full-batch duplicate retries.
8. ~~**Medium — µs-precision ties**: nanosecond monotonic timestamps collapsed to equal values on the µs-precision PG wire, making first-wins dedup nondeterministic on retry duplicates.~~ **Fixed**: monotonic clock now quantizes to microseconds (`next_event_time_us`).
