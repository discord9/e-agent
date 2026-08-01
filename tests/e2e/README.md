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
│   ├── sidebar.py       # 侧边栏：开合、会话树（活跃/历史分组 + label 优先）、筛选、15 条限制+展开
│   ├── chat.py          # 聊天：openSession、状态保留（草稿/滚动）、SSE 基本流、错误渲染
│   ├── commands.py      # 命令：/compact、/rename、/btw 斜杠拦截（mock 端点）
│   ├── listview.py      # 列表：标题两行、搜索、旧 server 降级、inactive 恢复、删除
│   ├── mobile.py        # 手机视口 390px 不横向溢出
│   ├── skipped.py       # 未合入功能的 TODO 登记（--list 可见，不运行）
│   └── smoke_real.py    # 真实 server 冒烟（可选，--real 启用，只读）
└── README.md
```

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
4. **失败自动打印关键 DOM 快照**（banner / sessionList / sidebarTree / messages 的
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
| `sidebar_tree` | 开合（按钮/×/Escape/遮罩）、会话树渲染（主会话根 + subagent 活跃/历史分组 + label 优先 + 旧 server 兼容 + busy 提示 + 孤儿组） |
| `sidebar_filter` | 筛选：主会话按 title/id 匹配、子会话随父展开、孤儿组、无匹配提示、清空恢复 |
| `sidebar_limit` | 15 条限制 + 「+N 个更早的会话」展开全部 |
| `chat_open_sse` | openSession、history 渲染、SSE mock 全事件流（status/UserPrompt/Delta/Reasoning/Tool/Text/Notice/Usage/Error）、Busy 状态 |
| `chat_state_preserved` | 切会话草稿/滚动位置/消息缓存恢复，不重新加载历史 |
| `chat_error_render` | Failed→失败（label/chip）、错误行前缀、Finished 禁输入（主/子会话提示） |
| `commands` | /compact、/rename（新标题/空清除/裸用法）、/btw（成功/404/500 降级/树出现新 subagent）、未知 /foo 走 /prompt |
| `list_two_line` | 标题两行显示（title + 完整 id）、无标题单行 id |
| `list_search` | 按 title/id 子串搜索、无匹配提示、清空恢复 |
| `list_legacy` | 旧 server 无 title/label/active/pinned 字段降级、✎ PUT 404 提示不崩 |
| `list_resume` | inactive 历史会话点击 → resumeSession（POST /api/sessions {id}） |
| `list_delete` | 删除会话（confirm + DELETE + 轮询重绘后行消失） |
| `mobile_390` | 390px 列表/聊天/侧边栏不横向溢出、覆盖式遮罩、点遮罩关闭 |
| `smoke_real_api`（可选） | 真实 server 列表渲染 + 打开会话（只读） |

## 注意事项

- **只测当前 checkout 的前端**：HTML 从 `src/ui/` 实时拼装，不依赖 server 内置页面
  或 `.e-agent/assembled.html`（那份是历史快照，可能过期）。
- **未合入功能**（`cases/skipped.py`，TODO 登记，不运行）：侧边栏挤压布局/持续打开
  （feat/sidebar-persist）、pin 置顶按钮（feat/pin-frontend）、并发写冲突友好卡片
  （feat/conflict-friendly-web）。对应分支合入 main 后，把 TODO 置空并按登记说明补用例。
- 用例不触碰真实数据（核心用例全 mock；冒烟只读）。
- 若浏览器二进制路径变了，设 `EAGENT_CHROME` 即可，无需改代码。
