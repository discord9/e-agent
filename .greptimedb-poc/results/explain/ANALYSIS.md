# Query Analysis — EXPLAIN ANALYZE VERBOSE
Data: 8601 real entries from 84 JSONL sessions, largest session 5780 entries.

## Summary

| # | Query | Time | Rows | Notes |
|---|-------|------|------|-------|
| 01 | MAX(seq) — connect() | 6.1ms | 5780 scanned | Scans all rows for the session |
| 02 | load() with ROW_NUMBER dedup | 3.9ms | 5780 scanned | WindowAggExec 34ms compute (dominated by sort) |
| 03 | load() simple ORDER BY | 7.3ms | 5780 scanned | Just SortExec, no window function |
| 04 | load() dedup — medium (238) | 1.6ms | 238 scanned | Proportional to session size |
| 05 | COUNT + MAX(seq) combined | 2.7ms | 5780 scanned | Single pass, good for fast-path check |
| 06 | time-range (1h) | 11ms | 8849 scanned | Full table scan — TIME INDEX doesn't prune much |
| 07 | cross-session (entry_kind) | 12ms | 29 scanned | Small result, but scans everything |

## Key Findings

### 1. Dedup overhead is real but small
The ROW_NUMBER dedup (query 02) adds a `BoundedWindowAggExec` (34ms compute) plus
an extra `SortExec` for the window partition key. For a 5780-row session this means
~4ms vs ~3ms for the simple path — about 30% overhead. For typical sessions (< 500
rows), the difference is sub-millisecond and irrelevant.

### 2. MAX(seq) does a full scan of the session's rows
Query 01 scans all 5780 rows just to find MAX(seq). This is because `seq` is a
regular field column, not part of the primary key or time index. GreptimeDB can't
use any index to shortcut the MAX aggregation. For connect-time this is fine (once
per session), but worth noting.

### 3. Time-range queries don't benefit from TIME INDEX here
Query 06 scans 8849 rows (nearly the full table) for a 1-hour range. This is because
the imported data was all written in the last hour (import batch used `now()` as
base_ts). With real-time data spread over days, TIME INDEX pruning would be much
more effective. Not a real problem.

### 4. Cross-session queries (entry_kind filter) work but scan everything
Query 07 scans the full table to find compaction entries. Since `entry_kind` is not
a tag/PK column, there's no index. This is by design — cross-session queries are
ad-hoc analytics, not the hot path.

### 5. Sort cost is the dominant factor for load()
The final `SortPreservingMergeExec` (ORDER BY seq) takes 2.2ms for 5780 rows —
nearly as much as the scan itself. This is because `seq` is not the TIME INDEX
column, so the storage engine can't return rows pre-sorted by seq. If `seq` were
part of the sort key (e.g. composite TIME INDEX), this could be eliminated.

## Optimization Recommendations

1. **Merge connect() + load()**: Skip the separate MAX(seq) query. If you load the
   session anyway, `entries.len()` gives you next_seq. Saves one round trip.

2. **Fast-path for dedup**: Use query 05 (COUNT + MAX in one pass) to check if
   `COUNT(*) == MAX(seq)+1`. If true, no duplicates — use the simple ORDER BY path
   (query 03) which is ~30% faster. Only fall back to ROW_NUMBER when duplicates
   are detected.

3. **Consider composite TIME INDEX**: If `(event_time, seq)` or just `seq` could be
   the sort key, the final SortExec could be eliminated entirely. This would require
   schema changes and testing whether GreptimeDB supports composite sort keys in
   append_mode.

4. **Batch append**: The current per-entry INSERT is fine for local (sub-ms), but
   batching per turn would reduce round trips. Not a query issue, but worth noting.

5. **No action needed on cross-session queries**: They're analytics, not hot path.
   If needed later, a materialized view or flow task could pre-aggregate.

## Update: Query Rewrites Using TIME INDEX (2026-07-29)

### MAX(seq) → last_value + LastRow hint

```sql
-- Before (full scan):
SELECT COALESCE(MAX(seq), -1) FROM session_entries WHERE session_id = $1
-- 5780 rows scanned, 6.1ms cold / 12ms warm

-- After (LastRow scan mode):
SELECT last_value(seq ORDER BY event_time ASC) FROM session_entries
WHERE session_id = $1 GROUP BY session_id
-- 2 rows scanned, 55ms cold / 3.5ms warm
```

The `LastRow` selector is triggered when ALL of:
1. Every aggregate is `last_value(col ORDER BY time_index ASC)`
2. GROUP BY columns are all PK (tag) columns
3. ORDER BY column is the TIME INDEX

Cold-start overhead (55ms) is from pruner cache misses across 20 partitions
(~1.3ms per partition prepare). Warm runs hit the cache and drop to 3.5ms.
For large sessions the tradeoff is clear: O(1) scan vs O(n) full scan.

Source: `src/query/src/optimizer/scan_hint.rs:224` sets
`TimeSeriesRowSelector::LastRow` on the scan, which activates
`LastRowReader` in `src/mito2/src/read/last_row.rs`.

### load() → simple ORDER BY event_time, dedup in application

```sql
-- Before (window function in query engine):
SELECT seq, payload FROM (
    SELECT ..., ROW_NUMBER() OVER (PARTITION BY session_id, seq ORDER BY event_time DESC) AS rn
    FROM session_entries WHERE session_id = $1
) t WHERE rn = 1 ORDER BY seq
-- WindowedSortExec + BoundedWindowAggExec, ~4ms for 5780 rows

-- After (plain timestamp sort, dedup in Rust):
SELECT seq, payload FROM session_entries
WHERE session_id = $1 ORDER BY event_time DESC
-- WindowedSortExec + PartSortExec (TIME INDEX optimization), ~17ms cold
```

Window functions (ROW_NUMBER) are computed entirely in DataFusion with no
storage pushdown. Moving dedup to the application layer (HashMap keyed by seq,
first-wins on DESC order) eliminates the window overhead. The SQL side benefits
from WindowedSortExec's TIME INDEX sort optimization.

### SortExec elimination via TIME INDEX

Query 09 (`ORDER BY event_time DESC`) uses `WindowedSortExec + PartSortExec`
instead of `SortPreservingMergeExec + SortExec`. The storage engine returns
rows pre-sorted by TIME INDEX within each partition, so PartSortExec is nearly
free (20ns compute). This optimization only works when sorting by the TIME INDEX
column — sorting by `seq` requires a full SortExec.

Source: `src/query/src/optimizer/windowed_sort.rs:74-140` recognizes when
the sort column is the TIME INDEX and replaces SortExec with the windowed
sort path.
