# GreptimeDB Standalone 统一存储迁移踩坑报告

**状态：** 迁移决定已定；本文不是可行性或劝退报告
**范围：** e-agent session/transcript、运行事件，以及 OpenTelemetry metrics/logs/traces 统一落到 GreptimeDB Standalone
**调查日期：** 2026-07-30
**证据基线：**

- e-agent：`44794ce4f0bc8c581f0742590330cd41579f31a1`
- GreptimeDB 源码：`ff2fa71d50af62815d23ea83ac34d0d4bf043e41`
- 本地 docs：`02a754039fc11df2241312873a83521486f3965f`
- Web：GreptimeDB v1.1 文档和截至调查日的 GitHub issue/PR

> **文档新鲜度说明：** 本地 docs checkout 在 feature branch，且用户指出它可能长期未更新。本文只把本地 docs 当作便于定位的副本；凡关键结论尽量用较新的 GreptimeDB 源码或 Web 官方文档交叉验证。实现前必须再对目标发行版的文档、配置 schema 与 release notes 做一次 version-lock 复核。不要把本文中的 `main`/nightly 行号当成任意稳定版本都成立。

## 0. 一句话结论

迁移方向没有问题，而且对 e-agent 明显优于 JSONL：可查询、可索引、有 TTL/压缩/备份工具，并能原生接收三类 OTLP 信号。真正容易踩的坑不是“GreptimeDB 能不能存 session”，而是：

1. **不能继续把 session 持久化当成本地同步文件追加。** 当前 `Session::append()` 每次 `sync_all()`；迁移后网络 ACK、超时重试和数据库宕机会引入“不知道写没写成功”的状态。
2. **session 表必须有稳定的顺序键和重试幂等键。** 仅有 timestamp 不足；同一时刻的多条事件及超时重试都会造成歧义。
3. **不要用 OTLP logs 代替权威 session transcript。** 两者可以同库，但生命周期、schema 和可靠性要求不同。
4. **OTLP 只走 HTTP/protobuf，不是标准 OTLP/gRPC。** traces 还必须带内置 pipeline header。
5. **Standalone 的默认本地 WAL 不是逐写 fsync。** 若 session ACK 意味着“断电后也不丢”，必须显式开启 `sync_write=true`，并实际测延迟。
6. **切换时保留 JSONL 作为短期 rollback journal，而不是永久双后端抽象。** 先双写、校验、切读，再停止 JSONL；删除回滚路径要最后做。

## 1. 当前 e-agent 语义：迁移必须保持什么

当前 `src/session.rs` 的核心语义是：

- 每个 session 一个 JSONL；`SessionEntry` 逐条追加；
- 每次 append 后 `file.sync_all()`，成功才把内存里的 `persisted` 游标前移；
- load 时严格按文件行序重放；第一条坏 JSON 会让整个 load 报错；
- legacy rewrite 通过临时文件 + rename 整体替换；
- main agent 与 subagent 都是“一个 turn 结束后批量持久化新增 history”；
- subagent persistence 目前是 best-effort，主 agent persistence failure 则会冒泡；
- `background.jsonl` 还有一套 start/clear/take 的小型可变状态，不等同于 transcript。

`SessionEntry` 目前只有三个持久化变体：`Message`、`Compaction`、`Notice`。`Message` 内又有 System/User/Assistant/Tool；assistant reasoning 只用于显示/审计，绝不能因迁移而开始回放到 provider wire。

因此，数据库迁移至少要保持以下不变量：

- 一个 session 内的**确定性顺序**；
- 每个 entry 的完整 serde 表示可以 round-trip（包括 future unknown fields 的迁移策略）；
- append 成功后再推进持久化游标；失败不能悄悄丢 history；
- resume 得到与 JSONL 相同的 `Vec<SessionEntry>`；
- compaction 之前的 entries 仍保留用于显示/审计；
- main/subagent 的错误语义不能被无意统一错：当前二者并不完全相同；
- reasoning 仍是 display/audit only。

### 一个现在就该纠正的理解

GreptimeDB 没有传统 `UPDATE` 或多表事务，**对 transcript 主路径不是硬伤**。e-agent 的 transcript 本来就是 append-only log；迁移不应把它改造成“session 一行的大 JSON blob”，也不需要原地更新 assistant message。

## 2. 推荐的最小 session 数据模型

### 2.1 一张权威事件表，而不是把整段 session 存成一行

建议第一版只建一张 `session_entries` 表；完整 `SessionEntry` 继续按现有 serde JSON 存入 `payload`，另抽取少数真正需要过滤/排序的列。不要一开始把每个 ToolCall/Assistant 字段拆成几十列，也不要为未来查询建立一套事件平台。

```sql
CREATE TABLE IF NOT EXISTS session_entries (
    session_id       STRING,
    seq              INT64,
    event_time       TIMESTAMP(9) TIME INDEX,
    entry_kind       STRING,
    payload          STRING,
    schema_version   INT32,
    agent_role       STRING,
    is_error         BOOLEAN,
    PRIMARY KEY (session_id)
) WITH (
    'append_mode' = 'true',
    'sst_format' = 'flat'
);
```

说明：

- `seq` 是 **session 内由 e-agent 分配的严格递增序号**，不是 GreptimeDB 的内部 sequence，也不是服务器 `CURRENT_TIMESTAMP()`。
- `event_time` 用 nanosecond precision，应用侧生成。它服务时间裁剪/TTL/诊断；**恢复顺序必须 `ORDER BY seq`**，不能靠 timestamp 或数据库返回的自然顺序。
- `payload` 存 `serde_json::to_string(SessionEntry)` 的完整文本。原生 JSON 类型在当前文档仍标为 experimental；先用 STRING 降低 schema churn。
- `entry_kind` 只取 `message` / `compaction` / `notice`；如果后续确有查询需求，再增加 `message_role`、`tool_name` 等少数字段。
- `schema_version` 明确 payload 的应用格式版本，避免未来仅靠 serde 猜版本。
- `agent_role` 可取 `main` / `subagent`；不要用 session id 前缀推断语义。
- `is_error` 只是便捷列，权威内容仍在 payload。
- `sst_format=flat` 在当前源码的默认 `MitoConfig` 和当前文档中是新表默认，但底层 `FormatType` 的 fallback default 仍是 `primary_key`，配置也可以改写默认。DDL 必须显式写出并用目标版本的 `SHOW CREATE TABLE` 验证；不要依赖隐式默认。
- **第一版不要加 TTL。** transcript 现在默认永久保存；擅自给 90d TTL 是数据语义变化。若用户明确选择 retention，再按独立 database/table 设置并做恢复测试。

### 2.2 `session_id` 高基数不是 blocker，但必须压测

官方 table-design 文档的准确说法是：高基数列**可以**进 PK，但会增加 key 长度、写入内存和 dedup 成本；若只是点查，高基数 field + skipping index 常是更低开销的默认。另一方面，GreptimeDB 的 flat SST 正是为大量 unique PK 优化。

对 e-agent，常见读路径是：

```sql
SELECT seq, payload
FROM session_entries
WHERE session_id = ?
ORDER BY seq;
```

把 `session_id` 放在唯一、首位 PK 能让同 session 数据邻近，逻辑很直接。不要因为泛化的“高基数 tag 不好”就绕成 hash bucket；也不要宣称一定最优。上线前必须用真实 transcript 分布比较：

- A：`PRIMARY KEY(session_id)`；
- B：无 PK，`session_id STRING SKIPPING INDEX`；
- 两者都测 append、单 session resume、session 列表、并发 main+subagent、压缩后查询，以及长 session 的 `ORDER BY seq` 排序成本。

先保留目标版本的默认 memtable；只有真实压测显示写入/内存瓶颈，才比较 `memtable.type=bulk`。当前源码中 bulk memtable 会强制 flat SST，不能把 memtable 与 SST format 当作完全独立的旋钮。

Skipping index（当前是 Bloom）是**无假阴性的块级预过滤**，最终 SQL 仍做精确比较；它能加速 equals，但不是传统唯一/B-tree 索引，也不提供排序或唯一性。

### 2.3 `append_mode=true` 解决重复保留，不解决重试幂等

append-only 是正确起点：同一 session 可在同一纳秒产生多条 entry，也不会被 `(PK, TIME INDEX)` dedup 掉；代价是该表不支持 `DELETE`。

但 append-only 意味着：客户端超时后不知道写是否已提交，盲目重试会插入重复 `(session_id, seq)`。**`seq` 只是检测重复，不能在 append mode 下自动去重。** 这正是迁移中最重要的应用层坑。

建议第一版的写协议：

1. 一个 turn 的新增 entries 在内存中先分配连续 `seq`；
2. 尽量作为一个批次写入；
3. 成功 ACK 后推进 `persisted` 游标；
4. 对明确“连接前失败”的错误可重试；
5. 对 timeout/断连这种 ambiguous outcome，先执行：

   ```sql
   SELECT seq FROM session_entries
   WHERE session_id = ? AND seq BETWEEN ? AND ?;
   ```

   只补缺失 seq；
6. load 时对 `(session_id, seq)` 重复行显式报数据完整性错误，或先定义可审计的 deterministic 去重规则，不能静默随便取一条。

若 PoC 证明这种 reconciliation 太重，可另测非 append 表，把 `PRIMARY KEY(session_id, seq)` 与唯一 event timestamp 组合用于 dedup；但在 GreptimeDB 中 dedup key 实际是 `(PRIMARY KEY, TIME INDEX)`，`seq` 进 PK 会加长 key，而且仍要保证 event_time 重试不变。不要未经实验就切换。另需把 `append_mode=true` 当作 schema invariant：若有人后来误关它，而 PK 仍只有 `session_id`，同 session、同 event_time 的 entries 会被静默合并。

### 2.4 不要用 `CURRENT_TIMESTAMP()` 生成权威 event_time

同一批/重试时服务器时间可能相同或变化；这会破坏幂等与历史导入。应用必须在 entry 第一次产生时生成并持有 `event_time`，重试复用同值。历史 JSONL 没有逐 entry timestamp，回填策略见迁移章节。

### 2.5 session 列表不能从文件名“免费”获得了

JSONL 的 session discovery 可以列目录；单表后若直接 `SELECT DISTINCT session_id`，数据增长后会越来越贵。不要立刻建复杂 catalog 表，但 PoC 必须覆盖 `--session` UX 和未来 session picker：

- 小规模阶段可 `GROUP BY session_id` 得到 `MIN(event_time), MAX(event_time), COUNT(*)`；
- 若实测不够，再加一张 `sessions` 元数据表或 Flow 物化摘要；
- 元数据表会引入“entry 已写但 session summary 未更新”的跨表非事务问题，所以它只能是可重建索引，不能成为 transcript 的第二权威来源。

## 3. 接入协议：最小依赖与真正的坑

### 3.1 不建议用 HTTP SQL 拼接 payload

GreptimeDB `/v1/sql` 只接收 form-urlencoded 的 SQL 字符串，没有参数绑定。session payload 包含任意引号、NUL、代码、工具输出和大文本；自行 SQL escaping 容易错，也会复制大字符串。它适合 DDL、ad-hoc 查询和初期 smoke test，不适合权威 transcript 写路径。

### 3.2 两个实际候选

1. **PostgreSQL/MySQL wire client**：可使用 prepared statements，生态成熟，但 GreptimeDB 不是完整 PostgreSQL/MySQL；ORM introspection 与部分语法不兼容。若只用显式 SQL + bind，风险可控。
2. **GreptimeDB gRPC row-insert + Flight query**：写入类型明确、支持批量，不需要 SQL escaping；但本地 `src/client` crate 名为内部 `client`，依赖大量 workspace crate，并非适合 e-agent 直接引用的轻量公开 SDK。直接复制 protobuf/协议又违背 e-agent 的最小原则。

~~因此报告不替实现阶段拍板。做一个很小的 worktree PoC，比较：~~

**PoC 已完成（2026-07-29），结果如下：**

| 路径 | 字节正确 | `\n` 多行 | 1MB payload | CJK/emoji | 注入串 |
|---|---|---|---|---|---|
| **tokio-postgres 0.7** | ✅ | ✅ | ✅ | ✅ | ✅ |
| **gRPC ingester 0.18.0** | ✅ | ✅ | ✅ | ✅ | ✅ |

关键发现：

- **psycopg2 的 `\n` → `\\n` 转义是 Python/libpq 特有的**，Rust 客户端（tokio-postgres / gRPC ingester）均无此问题。
- tokio-postgres 不能直接给 `TIMESTAMP(9)` 传 `i64`（参数类型映射拒绝），需格式化为字符串字面量或用 `with-chrono` feature。
- gRPC ingester 可以写已有的 `append_mode` 表（schema 匹配即可），但**只写不读**——读路径仍需 SQL。
- 批量吞吐：tokio-postgres 未测批量；gRPC 68k rows/s；HTTP SQL multi-VALUES 55k rows/s。

**结论：tokio-postgres 单协议读写是最小依赖方案**（一个 crate，读写皆可，字节正确已验证）。gRPC ingester 留给未来 Arrow Flight bulk 场景。

不要新增 `Storage` trait：目前只有一个决定采用的后端。一个具体的 Greptime session 模块足够；旧 JSONL 迁移器/短期 fallback 不构成长期第二实现。

## 4. OTLP 一体化：接口矩阵和版本坑

### 4.1 端点事实

GreptimeDB 原生接收的是 **OTLP/HTTP protobuf**：

| 信号 | URL（默认 HTTP 端口 4000） | 额外要求 |
| --- | --- | --- |
| metrics | `/v1/otlp/v1/metrics` | `X-Greptime-DB-Name`，可选 metrics promotion headers |
| logs | `/v1/otlp/v1/logs` | 可选 table/pipeline/extract-key headers |
| traces | `/v1/otlp/v1/traces` | **必须** `X-Greptime-Pipeline-Name: greptime_trace_v1` |

统一 SDK base endpoint 是 `http://host:4000/v1/otlp`，标准 exporter 会追加 `/v1/{signal}`，于是得到上面的双 `/v1/` 路径。这很反直觉，配置时最容易写成错误的 `/v1/metrics`。

**没有标准 OTLP/gRPC 4317 listener。** 4001 是 GreptimeDB 自有 gRPC/Flight（另有 OtelArrow metrics service），不能把 Rust OTel exporter 的 tonic OTLP endpoint 指到 4001。e-agent telemetry 必须编译/配置 `http/protobuf`；这也与现有 `reqwest`/Rustls 方向一致。

### 4.2 Metrics

- 每个 metric 映射为逻辑表；新表使用 Prometheus-compatible 模式，名称/label 会转换。
- **PoC 实测确认（2026-07-29）**：表名不是原始 metric 名，而是经过 Prometheus 兼容转换：
  - gauge (unit=1) → `name_ratio`（unit 后缀拼接）
  - counter → `name_total`（强制 `_total` 后缀）
  - histogram (unit=ms) → `name_milliseconds_bucket` / `_sum` / `_count`（三张表）
  - Resource attributes（如 `service.name`）提升为列（`service_name`, `job`）
  - 查询时需用转换后的表名，或直接用 PromQL HTTP API。
- **ExponentialHistogram 静默丢弃**：OTLP 返回 200 但不建表、不报错。源码确认是 no-op。如果 e-agent 未来用 native histogram，会丢数据。
- 默认只提升选定 resource attributes；scope attributes 默认丢弃。若需要 `service.version`、deployment、自定义 resource，先明确 promotion header，不要摄入后才发现维度没了。
- Delta Sum/Histogram 按原值保存，不替你累积；temporality 必须和预期 PromQL 对齐。
- Explicit histogram 会展开 `_bucket` / `_sum` / `_count`。
- **ExponentialHistogram 当前不支持，代码路径直接忽略。** 这不是普通告警，而是数据缺口；在 SDK 选择 explicit buckets，或 collector 中转换，并用已知样本端到端核对。
- **Exemplars 当前不落库。** Prometheus remote-write v2 代码也明确忽略 exemplars；不要把“响应 header 存在”误判为支持。
- 同名旧 OTLP metric 表可能继续走 legacy mapping；上线前检查 `SHOW CREATE TABLE`，不要让旧测试数据偷偷锁定旧模式。

### 4.3 Logs

- 默认表 `opentelemetry_logs`，默认 append-only；body 带 fulltext index；attributes 使用 GreptimeDB JSON 列。
- 这不与“session payload 暂用 STRING”矛盾：OTLP log schema 是 GreptimeDB 管理的 observability contract；session payload 是 e-agent 的长期恢复 contract。
- 默认 schema 的 `scope_name` 是 tag；可通过 `X-Greptime-Log-Extract-Keys` 提取有限的 scalar attributes 为 tag。不要把 session id、tool call id 等所有高基数字段都提升为 tag。
- OTLP exporter 重试可能产生重复 logs；append-only 不去重。日志查询/计数要接受 at-least-once，或在应用事件里带稳定 event id 供离线识别。

### 4.4 Traces

- 默认表 `opentelemetry_traces`，append-only，trace pipeline v1 会扁平化 resource/scope/span attributes；属性集合变化会演化宽表 schema。
- 部分 spans 类型冲突时可能返回 OTLP `partial_success`；只有检查 protobuf body 才知道 rejected count，不能只看 HTTP 2xx。
- 默认按 trace_id 首字符 16 分区；标准随机 hex trace id 通常均匀，但自定义 id 生成器必须验证。可用 hint 调分区数。
- traces 及其模型仍是演进较快的部分；放在独立 database（例如 `e_agent_otel`）可让重建/迁移不碰权威 sessions。
- traces/logs 的 timeout 重试都会形成重复数据。Collector 的 `batch` processor **不是去重器**；不要把“加 batch”写成去重方案。

### 4.5 建议的逻辑隔离

同一个 Standalone 实例，但至少两个 database：

- `e_agent`: 权威 session/transcript；默认不设 TTL；严格 durability。
- `e_agent_otel`: metrics/logs/traces；按信号设 TTL，可接受采样和 at-least-once 重复。

“统一后端”不等于“混在一个 schema、一个 retention policy 里”。数据库级隔离可减少 OTLP 自动建表、TTL 和 schema churn 误伤 session。

## 5. Standalone 运维：上线前必须改的默认值

### 5.1 WAL durability

本地 RaftEngine WAL 默认：

```toml
[wal]
provider = "raft_engine"
sync_write = false
```

`sync_write=false` 表示每次 WAL write 不做 fsync；进程 crash 通常可重放 WAL，但断电/内核崩溃的最后窗口不是 RPO=0。若 e-agent 收到 DB ACK 后就删除本地 rollback journal，session 可能比当前每 turn `sync_all()` 更弱。

上线 session 前：

- 设置 `sync_write=true`；
- 禁止 session 表 `skip_wal=true`，也不要通过 table/region `wal_options` 选择 noop WAL；建表后审计 `SHOW CREATE TABLE`；
- 在目标硬盘实测每 turn 与批量 append 的 p50/p99；
- 给 WAL 独立可靠卷是性能优化，不替代备份；
- 做 kill -9、容器重启和断电等价测试，核对已 ACK seq。

### 5.2 单点不是劝退理由，但恢复必须可演练

Standalone 无副本/自动 failover；这是已选择的部署边界。需要接受的是 downtime，而不是没有 runbook：

- 固定 GreptimeDB 版本和镜像 digest；
- 持久化 `data_home`、WAL、metadata，不能把任一目录留在 ephemeral filesystem；
- 定期 `greptime cli data export`；恢复时使用官方 import；
- metadata snapshot 对 RaftEngine 需要停 Standalone；
- 当前没有 PITR；恢复粒度是最近一次可验证备份 + 仍存活的本地/WAL 数据；
- Standalone DR 文档页在调查基线中是空的，必须自己写并演练 restore SOP；
- 不要把运行中的 `data_home` 直接 rsync 当一致性备份，除非使用经过验证的 crash-consistent volume snapshot 流程。

备份使用 GreptimeDB 的 typed export/Parquet 和 metadata snapshot；不要永久保留一套“应用层 JSON 全库备份”。迁移窗口里的原 JSONL 是 rollback source，角色不同。

### 5.3 资源和磁盘默认值

默认 `max_concurrent_queries=0`、`scan_memory_limit=unlimited`、standalone `max_in_flight_write_bytes=0`。统一承载 OTLP 后，metrics burst 或 Grafana 大查询可能挤死 session resume/write。

上线前至少：

- 设置容器/系统 memory limit；
- 给 query concurrency、scan memory、in-flight writes 设**非零有限值**；这些值必须按机器内存、最大 session payload 和 OTLP burst 压测得出，报告不臆造一个通用数字；
- 监控 write buffer reject、flush/compaction failure、WAL/disk/inode、RSS、HTTP 429/5xx、查询延迟；
- `/metrics` 接入**外部** Prometheus/agent，避免唯一告警也只存回这台 GreptimeDB；
- 磁盘 70–75% 预警、80–85% 严重告警，保留 compaction/headroom；
- 使用对象存储时部署后立即写入 + `ADMIN flush_table` 验证，不能以“进程能启动”证明凭据和 bucket 可用。

### 5.4 HTTP TLS

GreptimeDB 的 HTTP server 没有原生 TLS 配置，而 sessions 与 OTLP 都会走含敏感内容的 HTTP。若不是严格 localhost/同机 Unix 网络边界，必须用 Caddy/nginx/mesh 终止 TLS，并验证：

- body size（tool output/session payload 可能很大）；
- request timeout；
- streaming/keepalive；
- Basic auth header 不被日志记录；
- OTLP protobuf content type 不被代理改写。

## 6. 历史 JSONL 迁移与切换顺序

### 6.1 历史数据最麻烦的是没有逐 entry 时间

session id 包含创建秒，但现有 JSONL 行没有 timestamp。不能假装能恢复真实事件时间。建议：

- `seq` 按文件有效行从 0/1 递增，完全保留历史顺序；
- `event_time` 优先取可证明的来源；没有则以文件 mtime/会话 id 时间为 anchor，加 `seq` 纳秒形成稳定单调值；
- 增加迁移审计记录，标记 `time_source = synthetic`（可放 payload envelope 或迁移日志，不一定永久加列）；
- 同一文件重跑必须生成完全相同的 `(session_id, seq,event_time,payload)`。

### 6.2 推荐 rollout（不是长期双后端）

1. **Inventory**：枚举 `.e-agent/sessions/*.jsonl`，记录文件 hash、有效行数、解析结果；`background.jsonl` 单独处理。
2. **建库建表**：显式 DDL；`SHOW CREATE TABLE` 存档；开启 auth 和 `sync_write=true`。
3. **离线导入历史**：按 session 批量写，逐 session 校验 count、min/max seq、payload hash。
4. **Shadow read**：同一 session 同时从 JSONL 与 GreptimeDB load，比较反序列化后的 `Vec<SessionEntry>`，不是只比行数。
5. **短期双写**：先写 GreptimeDB，成功后仍写 JSONL rollback journal；任一失败都显式报错并保留未持久化内存状态。需要明确定义“DB 成功、JSONL 失败”如何告警。
6. **切读 GreptimeDB**：JSONL 保持只写/只读 fallback 一小段观察期；恢复/重启/compaction/subagent resume 全部实测。
7. **停止 JSONL 写**：保留只读 migration command 和 immutable archive；不要立刻删除。
8. **最后移除 fallback**：完成一次从官方 backup 的空机恢复演练后再做。

### 6.3 校验不能只用 COUNT

每个 session 至少验证：

- seq 连续、无重复；
- `COUNT(*)`；
- `MIN/MAX(seq)`；
- 每条 payload 可 deserialize；
- 按 seq 拼接的 canonical hash；
- `Agent::messages()` 在 JSONL 与 DB load 后等价；
- reasoning 字段存在但 wire serialization 仍排除；
- compaction + retained messages、Notice、Tool error、Unicode/大 stdout 都有 fixture。

## 7. `background.jsonl` 不要硬塞进 transcript 表

它表示“进程是否在任务运行期间死亡”的可变 registry：start append、completion rewrite/remove、下次启动 consume。若迁移它：

- 不应写到 append-only `session_entries` 后假装清理；
- 可以先继续保留本地小文件，避免把 session migration 膨胀成所有状态一次重写；或者
- 建一个小的 deduplicating `running_tasks` 表，以 stable process-instance id + task id + timestamp 做键，并通过 insert/upsert 表示 started/completed/consumed。

为控制首个迁移切片，建议第一版只迁权威 session transcript；`background.jsonl` 保留，除非“一体化”明确要求连这个 crash marker 都迁。它只有几行，不是 JSONL session 后端的主要问题。

## 8. 后续实施前必做的 PoC/故障注入清单

以下验证不属于本次只读调研；进入实现阶段后，所有实验必须在独立 worktree/独立测试 data_home 进行：

### Session correctness

- 同一 session 同一 timestamp 连续写 100 条，按 seq 完整恢复；
- payload 含单/双引号、NUL、emoji、中文、换行、1MB/10MB tool output；
- 一个 turn 多 entries 单批写；
- server 在 request 后、response 前被 kill，客户端 reconcile 后无缺 seq/未处理重复；
- 两个 e-agent 进程错误地同时 resume 同一 session，确认是拒绝、冲突告警还是明确支持（当前 JSONL 也没有锁，这个坑不要扩大）；
- compaction、cancelled turn、provider error、未配对 tool call repair 的 round-trip。

### Durability/recovery

- `sync_write=false` 与 `true` 的 kill/restart、延迟和吞吐对比；
- WAL replay 后逐 seq/hash 校验；
- disk-full/readonly/object-store outage；
- backup → 全新 data_home → restore → session/OTLP 查询 smoke test；
- 版本升级前后同一 fixture 查询与 restore。

### OTLP

- exporter endpoint 确认是 `/v1/otlp` base + `http/protobuf`；
- trace 缺 pipeline header 必须在测试中失败，带 header 成功；
- resource/scope attribute promotion 前后 schema；
- explicit histogram 数值与 PromQL；exponential histogram 明确失败/不产生数据，不能静默通过测试；
- exemplar 明确不在 backend；
- trace partial_success body 被监控；
- retry 后 logs/traces duplicate rate；
- TTL 到期只承诺 compaction 后清理，不测试“到秒立刻消失”。

## 9. 上线 gate

满足以下条件再把 GreptimeDB 设为唯一 session read/write backend：

- [ ] 目标 GreptimeDB 发行版锁定；本文所有配置键在该版本复核
- [ ] session schema 经真实数据 A/B benchmark 确认
- [ ] 写入使用 bind/typed protocol，不拼接任意 payload SQL
- [ ] ambiguous write outcome 有 seq reconciliation
- [ ] `sync_write=true` 且延迟可接受
- [ ] data_home/WAL/metadata 都在持久盘
- [ ] 内存/并发/磁盘 limits 与告警已设
- [ ] HTTP 仅 localhost，或代理 TLS/auth 已验证
- [ ] 历史迁移 canonical hash 100% 一致
- [ ] JSONL shadow read 观察期无 divergence
- [ ] 官方 data + metadata backup 的空机 restore 演练成功
- [ ] OTLP exporter 是 HTTP/protobuf；trace pipeline header 固化
- [ ] retention 分离：session 无默认 TTL，OTLP 每类显式 TTL
- [ ] exponential histogram/exemplar 缺口已由 SDK 配置或明确接受

## 10. 明确非目标

- 不重新讨论是否迁移 GreptimeDB。
- 不因短期双写新增通用 `Storage` trait/plugin framework。
- 不把 session transcript 改造成 OTLP logs，也不把 OTLP 当恢复协议。
- 不在第一版拆出复杂多表 event schema、全文搜索平台或 session analytics。
- 不为了 Standalone 自建 scheduler、replication、PITR 或 HA 层。
- 不默认采集 prompt、reasoning、tool args/results 到 OTel；session 权威存储与 telemetry privacy policy 分开。
- 不在本次调研中实现代码或运行 GreptimeDB 实验。

## 11. 来源

### e-agent 本地源码

- `src/session.rs` — JSONL load/append/rewrite、`sync_all()`、legacy migration、background task registry
- `src/agent.rs` — `SessionEntry`、history/context/compaction、reasoning replay boundary
- `src/main.rs`, `src/tui.rs`, `src/delegate.rs` — turn-boundary persistence 与 main/subagent error semantics

### GreptimeDB 官方源码（本地 `ff2fa71d50af`）

- `src/servers/src/http.rs`, `src/servers/src/http/otlp.rs` — OTLP HTTP routes/response/partial success
- `src/servers/src/otlp/metrics.rs` — metric translation、histogram、unsupported exponential histogram
- `src/servers/src/otlp/logs.rs` — default OTLP log schema/fulltext/JSON columns
- `src/servers/src/otlp/trace/v1.rs` — trace mapping
- `src/servers/src/prom_remote_write/v2.rs` — exemplars ignored
- `src/common/wal/src/config/raft_engine.rs` — local WAL defaults
- `src/mito2/src/region/options.rs`, `src/mito2/src/worker/handle_write.rs` — append/dedup/delete semantics
- `src/client/Cargo.toml` — internal client dependency footprint
- `src/cli/src/data/{export_v2,import_v2,snapshot_storage}.rs` — newer snapshot/import-export implementation

### GreptimeDB 官方文档

- Data model: <https://docs.greptime.com/user-guide/concepts/data-model>
- Table design: <https://docs.greptime.com/user-guide/deployments-administration/performance-tuning/design-table>
- CREATE TABLE/options: <https://docs.greptime.com/reference/sql/create>
- Data indexes: <https://docs.greptime.com/user-guide/manage-data/data-index>
- OTLP: <https://docs.greptime.com/user-guide/ingest-data/for-observability/opentelemetry>
- HTTP protocol: <https://docs.greptime.com/user-guide/protocols/http>
- Local WAL: <https://docs.greptime.com/user-guide/deployments-administration/wal/local-wal>
- Data backup/restore: <https://docs.greptime.com/user-guide/deployments-administration/disaster-recovery/back-up-&-restore-data>
- Metadata backup/restore: <https://docs.greptime.com/user-guide/deployments-administration/disaster-recovery/back-up-&-restore-meta-data>
- Upgrade: <https://docs.greptime.com/user-guide/deployments-administration/upgrade>
- Standalone monitoring: <https://docs.greptime.com/user-guide/deployments-administration/monitoring/standalone-monitoring>

### 需要在目标版本逐项复核的公开 issues

- Sparse partition-tree PK insertion: <https://github.com/GreptimeTeam/greptimedb/issues/8217>
- High-series-cardinality concurrent Flight queries: <https://github.com/GreptimeTeam/greptimedb/issues/7939>
- Standalone empty-region recovery workflow: <https://github.com/GreptimeTeam/greptimedb/issues/8374>
- Object-storage import tracking（旧 CLI；新 v2 源码已有更多能力，勿只看旧 issue 下结论）: <https://github.com/GreptimeTeam/greptimedb/issues/7756>
- Memory limiter tracking: <https://github.com/GreptimeTeam/greptimedb/issues/7094>
- Parquet row-group/compaction limit: <https://github.com/GreptimeTeam/greptimedb/issues/7940>
