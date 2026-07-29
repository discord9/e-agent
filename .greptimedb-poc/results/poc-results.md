# GreptimeDB Storage PoC — Experiment Results
Date: 2026-07-29
Server: greptime v1.2.0 (nightly 2ed3c3c4), standalone, raft_engine WAL, sync_write=true
Harness: poc-harness/ (Rust, greptimedb-ingester 0.18.0 + tokio-postgres 0.7 + reqwest)

## 1. Session table round-trip (HTTP SQL)
- 11 payloads byte-identical: 1MB, CJK, emoji, multiline, SQL injection, compaction/notice entries
- Ordering: seq-based ORDER BY correct across interleaved sessions with out-of-order timestamps
- Duplicate retry: append_mode preserves duplicates (count=2 for all), ROW_NUMBER() dedup works
- Batch insert: 1000 rows via multi-VALUES in 0.02s (55k rows/s)

## 2. Crash durability (kill -9)
- 1083 rows before kill, 1083 after restart — all survived with sync_write=true

## 3. Client protocol comparison (Rust harness)

### tokio-postgres (port 15403)
- ALL payloads byte-identical including \n, \t, \r\n, backslash, quotes, 1MB
- psycopg2 \n escaping problem is Python/libpq-specific; NOT present in Rust clients
- Caveat: cannot pass i64 directly to TIMESTAMP(9) param — use string literal or chrono

### gRPC ingester (port 15401)
- ALL payloads byte-identical
- Works fine writing to existing append_mode table (schema must match)
- 9 rows incl 1MB in 70ms; batch 1000 rows in 15ms (68k rows/s)
- Write-only: no SQL query capability — read path needs separate protocol

## 4. OTLP signals (Rust harness, opentelemetry-proto 0.30)
- Traces: 200 OK → `opentelemetry_traces` table. Requires `x-greptime-pipeline-name: greptime_trace_v1` header
- Metrics: 200 OK → **Prometheus-compatible table name remapping**:
  - gauge (unit=1) → `poc_rust_gauge_ratio` (unit suffix appended)
  - counter → `poc_rust_counter_total` (always `_total` suffix)
  - histogram (unit=ms) → `poc_rust_histogram_milliseconds_{bucket,sum,count}` (unit + three tables)
  - Resource attrs promoted to columns: `service_name`, `job`, custom labels
- Logs: 200 OK → `opentelemetry_logs` table
- ExponentialHistogram: 200 OK but **silently dropped** — no table created, no error. Confirmed no-op.

## 5. Recommendation
- tokio-postgres for session read+write (single protocol, minimal deps, byte-correct)
- gRPC ingester only if Arrow Flight bulk needed later
- HTTP SQL acceptable fallback but adds JSON parse overhead
