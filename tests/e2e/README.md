# e-agent Web 端回归测试套件

把散落在 `.e-agent/` 下的临时 playwright 验证脚本（verify_sidebar_active_subs.py、
verify_sidebar_persist.py、verify_btw_frontend.py、verify_msg_title.py、
web_pin_check.py、verify_conflict.py …）整理成**可重复跑**的回归套件，覆盖核心前端
功能，合入前一键回归。

## 目录结构

```
tests/e2e/
├── regression.py        # 主入口：--list / --all / --case NAME；汇总 PASS/FAIL，exit 0/1
├── common.py            # 共享：HTML 拼装、浏览器启动、路由拦截、Case 上下文、DOM 快照
├── cases/
│   ├── __init__.py      # 用例发现（扫描各模块的 CASES 列表）
│   ├── sidebar.py       # 唯一会话导航：开合、树、筛选、恢复、删除、置顶、归档、8 条限制、
│   │                    #               桌面挤压布局、持续打开 + localStorage 持久化
│   ├── chat.py          # 聊天：从侧边栏打开、状态保留（草稿/滚动）、SSE、错误渲染、
│   │                    #       并发写冲突现状（Failed->失败）
│   ├── commands.py      # 命令：/compact、/rename、/btw 斜杠拦截（mock 端点）
│   ├── mobile.py        # 手机视口 390px 聊天空状态/聊天/侧边栏不横向溢出
│   ├── skipped.py       # TODO 用例登记（--list 可见，不运行）；当前为空，供未来未合入功能登记
│   └── smoke_real.py    # 真实 server 冒烟（可选，--real 启用，只读）
└── README.md
```

## GreptimeDB background restart recovery

`greptime_background_restart_recovery.sh` is a real process-boundary E2E for
GreptimeDB-backed background-task recovery. It starts an isolated GreptimeDB,
mock OpenAI SSE provider, and server A/B with dynamically allocated listeners,
then creates the task through the public prompt API, kills server A by its saved
PID, resumes the same session in server B, and checks history/SSE plus the
running-task API. It requires explicit `GREPTIMEDB_BIN` and `psql`; no existing
GreptimeDB, provider, config, state, workspace, or secret is read. A failed
run deliberately retains its temporary directory and reports the exact HTTP
body; a successful run removes all child processes and temporary data.

```sh
GREPTIMEDB_BIN=/path/to/greptime bash tests/e2e/greptime_background_restart_recovery.sh
```

This test is an acceptance test, not a compatibility shim: the recovery notice
assertion is not weakened when production behavior fails.

## 运行

```sh
cd tests/e2e
uv run --with playwright python regression.py --list     # 列出用例
uv run --with playwright python regression.py --all      # 跑全部就绪用例
uv run --with playwright python regression.py --case sidebar   # 按名称子串跑
uv run --with playwright python regression.py --all --real     # 含真实 server 冒烟
```

退出码：0 = 全部 PASS（含 SKIPPED 不计失败）；1 = 存在 FAIL。

## 依赖

- `uv`（Python 环境管理器）
- playwright（`uv run --with playwright` 自动提供；已缓存时秒级就绪）
- chromium headless shell（默认路径
  `/mnt/nvme_rust/cargo-home/playwright-browsers/chromium_headless_shell-1228/chrome-headless-shell-linux64/chrome-headless-shell`，
  可用环境变量 `EAGENT_CHROME` 覆盖）

## 设计约定（稳定性）

1. **核心用例全 mock，不依赖真实 server**。主页 HTML 由 `common.assemble_html()`
   从当前 checkout 的 `src/ui/`（index.html 骨架 + style.css + app.js + vendor）
   拼装，`page.route` 拦截 `/api/sessions`、`/history`、`/events`、`/prompt`、
   `/title`、`/btw`、`/compact` 等注入测试数据。server 没起也能跑。
2. **每个用例独立启动一个浏览器**，状态完全隔离；失败互不影响。
3. **超时控制**：单用例默认 30s（`--timeout` 可调），超时记为 FAIL。
4. **失败自动打印关键 DOM 快照**（banner / chatEmpty / sidebarTree / messages 的
   html 片段 + state 摘要），便于直接定位。
5. **0 JS 错误检查**：每个用例末尾自动断言全程无 pageerror / 非资源类 console.error。
6. 可选冒烟 `smoke_real_api`：连宿主 server（默认 `http://127.0.0.1:18766`，
   可用 `EAGENT_BASE` 覆盖）走真实 API，只读；需要 token 文件
   （默认 `~/.local/state/e-agent/server.token`，可用 `EAGENT_TOKEN_FILE` 覆盖）。
   server 不可达或 token 缺失时该用例 **SKIP**，不影响 exit code。

## 环境变量

| 变量 | 默认值 | 说明 |
|---|---|---|
| `EAGENT_CHROME` | chromium headless shell 路径 | 浏览器可执行文件 |
| `EAGENT_BASE` | `http://127.0.0.1:18766` | 冒烟用例连接的 server |
| `EAGENT_TOKEN_FILE` | `~/.local/state/e-agent/server.token` | 冒烟用例的 token 文件 |
| `EAGENT_CASE_TIMEOUT` | `30` | 单用例超时秒数 |

## 用例清单与覆盖范围

| 用例 | 覆盖 |
|---|---|
| `sidebar_tree` | 开合、主会话根、subagent 活跃/历史分组、label 回退、busy 与孤儿组 |
| `sidebar_filter` | 标题与完整 ID 搜索、子会话随父展开、孤儿组、无匹配提示、清空恢复 |
| `sidebar_limit` | 每 workspace 默认 8 个主会话 + 展开全部 |
| `sidebar_squeeze` | 桌面挤压布局：聊天空状态与聊天内容右移 280px，遮罩不拦截 |
| `sidebar_persist` | 切会话保持侧边栏打开 + localStorage 跨刷新恢复 |
| `sidebar_actions` | 唯一导航中的 inactive 恢复、删除、pin、archive、归档组及旧 server 404 降级 |
| `chat_open_sse` | 从侧边栏打开、history 与 SSE 全事件流、Busy 状态 |
| `chat_state_preserved` | 侧边栏切会话时草稿/滚动/消息缓存恢复 |
| `chat_error_render` | Failed→失败、错误行、Finished 禁输入（主/子会话提示） |
| `conflict_card` | Failed 状态与并发写冲突错误消息现状 |
| `commands` | /compact、/rename、/btw 与未知命令；会话从侧边栏打开 |
| `mobile_390` | 390px 聊天空状态/聊天/覆盖式侧边栏无横向溢出，遮罩关闭 |
| `smoke_real_api`（可选） | 真实 server 侧边栏树渲染 + 打开会话（只读） |

## GreptimeDB History physical paging E2E

`greptime_history_physical_paging.sh` 是独立实例的端到端回归测试：它启动本次测试专用
GreptimeDB（独立 `--data-home`，HTTP/gRPC/MySQL/PG 均为动态 `127.0.0.1` 端口）和
本次测试专用的 token-authenticated e-agent HTTP server。fixture 只通过 `psql` 写入该
实例，行为验收只通过 `/api/sessions/{id}/history` 的 HTTP 响应完成，并覆盖
`ORDER BY seq DESC,event_time DESC` 的 physical-row LIMIT、最新同 seq 副本优先、页内
best-effort dedup，以及 compaction segment 边界。

调用者必须显式传入绝对路径且可执行的 `GREPTIMEDB_BIN`；脚本不会自动发现或 fallback
到任何本机路径。可选的 `EAGENT_BIN` 也必须是绝对路径可执行文件，否则脚本使用
`CARGO_TARGET_DIR`（或临时 target）构建。运行示例：

```sh
GREPTIMEDB_BIN=/absolute/path/to/greptime \
CARGO_TARGET_DIR=/path/to/target \
tests/e2e/greptime_history_physical_paging.sh
```

端口选择是一次性 `bind(0)` 后关闭 socket，再立即启动两个进程；这是一个很短但不可
消除的 TOCTOU 窗口。启动和 readiness 任一 bind 冲突都会 fail-closed，绝不重试到默认
端口或连接已有实例。Greptime readiness 同时要求 PID、`/health` 和 `PG SELECT 1`；
server readiness 要求 PID、临时 token 和授权 `/api/sessions`。fixture 写入后会重启同一
个独立 e-agent，因此历史 fixture 的查询确实经过未注册 live registry 的 historical
fallback；验证 unknown initial/older 为 404、known historical initial 和 terminal empty
page 为 200。cleanup 按 e-agent → GreptimeDB 顺序逐 PID 有界 TERM、必要时 KILL，只有两个进程都退出才删除临时根目录；
失败时保留路径以便诊断。

```sh
bash -n tests/e2e/greptime_history_physical_paging.sh
```

## 注意事项

- **只测当前 checkout 的前端**：HTML 从 `src/ui/` 实时拼装，不依赖 server 内置页面
  或 `.e-agent/assembled.html`（那份是历史快照，可能过期）。
- **TODO 登记**（`cases/skipped.py`）：当前为空。曾登记的 4 个 TODO 已全部合入 main
  并转正（sidebar_squeeze / sidebar_persist / pin_button / conflict_card——pin 已迁入 sidebar_actions；conflict_card 按
  现状登记：仅 statusLabel Failed→失败 合入，.msg-error.conflict 友好卡片未合入）。
  未来发现「功能未合入、写用例必然 FAIL」的新功能时，在此登记并写 TODO 说明。
- 用例不触碰真实数据（核心用例全 mock；冒烟只读）。
- 若浏览器二进制路径变了，设 `EAGENT_CHROME` 即可，无需改代码。

## Nested subagent restart E2E

`greptime_nested_background_restart.sh` runs an isolated GreptimeDB plus server A/B and
stdlib mock provider on dynamically allocated ports (never `15403`). It creates a parent
delegate and a child-owned background bash task, verifies both `/api/tasks` ownership and
both durable `running_tasks` scopes, then SIGKILLs A. The child command has a unique
per-run argv marker; the test fails before starting B if that host process survives.

After containment, parent resume must consume and report only the parent delegate row.
The child session is then resumed explicitly and must receive exactly one child-owned
killed notice and consume its own row. No lifecycle SQL writes are used. Failures preserve
the isolated temp root and clean up only the exact marker process/group and test services;
success removes the root. Run with `GREPTIMEDB_BIN=/home/discord9/.local/share/e-agent/greptimedb/greptime`
and the explicit isolated E2E binaries/target described by the script.
