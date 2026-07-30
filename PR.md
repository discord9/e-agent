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
    workspace_id    STRING       NOT NULL,
    session_id      STRING       NOT NULL,
    seq             BIGINT       NOT NULL,
    event_time      TIMESTAMP(9) NOT NULL TIME INDEX,
    entry_kind      STRING       NOT NULL,   -- message / compaction / notice / background_completion
    payload         STRING       NOT NULL,   -- SessionEntry as JSON
    schema_version  INT          NOT NULL DEFAULT 1,
    is_error        BOOLEAN      NOT NULL DEFAULT FALSE,
    appended_at     TIMESTAMP(9) NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, session_id)
) WITH (append_mode = 'true', sst_format = 'flat')
```

## Query Patterns

| Operation | Query | Optimization |
|-----------|-------|-------------|
| `connect` (find max seq) | `SELECT COALESCE(MAX(seq), -1)::BIGINT ... WHERE workspace_id=$1 AND session_id=$2` | Full partition scan (acceptable, sessions are bounded) |
| `load` (read all entries) | `SELECT seq, event_time, payload ... ORDER BY seq ASC` | App-side dedup (`dedup_raw_entries`) — group by seq, keep only rows with the latest `event_time` per seq, sort output by seq ASC |
| `append` | Parameterized multi-row INSERT | Single statement (all-or-nothing per chunk of ≤9000 rows); `next_seq` advances only after all chunks succeed |

### Dedup semantics (load_with_seq → dedup_raw_entries)

1. Among all physical rows for a given seq, only the row(s) with the **latest**
   `event_time` are retained. Older rows are silently discarded (they are
   considered overwritten by the newer write).
2. If the latest event_time has **multiple** physical rows (same seq, same
   max event_time), their payloads are deserialised and compared. If all are
   identical (idempotent retry at the same microsecond), they are folded into
   one logical entry. If **any** differ, an error is returned with the seq,
   event_time, session id, and manual-inspection guidance.
3. Output is ordered by seq ASC.

## Verification

- All project tests pass (both default and `--features greptime`; integration tests require a live GreptimeDB)
- 8601 real JSONL entries imported from 84 sessions during the PoC (incremental importer)
- kill -9 durability verified (sync_write=true, 1083 rows survived)
- tokio-postgres and gRPC ingester both byte-identical (psycopg2 \n issue is Python-only)
- clippy clean (both feature sets), fmt clean

## Design Decisions

- **No Storage trait** — `SessionStore` is an enum, not a trait. AGENTS.md: "one adapter, no seam."
- **No dual-write** — config selects one backend at a time. Default stays JSONL.
- **No rewrite for Greptime** — compaction is append-only (Compaction entry appended at end; agent's `effective_history` finds it via `rposition`). `rewrite()` is a no-op for Greptime.
- **Background tasks stay JSONL** — `record_background_start` / `clear_background_task` / `take_unfinished_background` are unchanged.
- **append_mode = true** — no DELETE/UPDATE. Retries produce duplicates, deduplicated on read (`dedup_raw_entries` — latest event_time per seq wins, same-seq ties checked by deserialised equality).

## Non-goals

- No automatic migration of existing JSONL sessions at startup (manual import via `e-agent-import-jsonl`)
- No background task persistence in GreptimeDB
- No distributed/cluster deployment — standalone GreptimeDB only
- No generic storage abstraction for future third backends

## Known Limitations

- GreptimeDB server must be running before e-agent starts (no auto-discovery or embedded mode)
- `tokio-postgres` needs `with-chrono-0_4` feature for TIMESTAMP params
- `event_time` is wall-clock time with microsecond precision.  Within a single
  process, the monotonic `next_event_time_us` atomic guarantees strict ordering
  (same-µs or backward clock → prev+1).  Dedup: per seq, only rows with the
  latest `event_time` are retained; at the same max `event_time`, identical
  deserialised `SessionEntry` values are folded and divergent ones cause a
  hard error.

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
8. ~~**Medium — µs-precision ties**: nanosecond monotonic timestamps collapsed to equal values on the µs-precision PG wire, making retry duplicates indistinguishable at the same microsecond.~~ **Fixed**: monotonic clock now quantizes to microseconds (`next_event_time_us`); duplicate detection uses deserialised `SessionEntry` equality at the same max event_time.
