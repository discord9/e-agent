# 冒烟测试清单

每个功能合并后，按此清单验证真实用户路径。标记：✅ 单测覆盖 / 🖐 需手动 / 🤖 可脚本化（scripts/smoke.sh）

> 背景：本项目吃过两次「单测全过、真实环境挂」的亏 —— btw fork 单测 5 个全过、真实环境 HTTP 500；read_image 在非视觉模型下把图片塞进历史、会话永久锁死。此清单的价值就是把这些真实路径变成每次合并后的例行检查。

## 如何用

- 合并功能后：先 `cargo test`，再跑本节相关条目（`scripts/smoke.sh` 自动跑 🤖 条目）
- 后端 Rust 改动：**重启 server 再测**（进程内状态如 undo 栈、token、registry 都是内存态）
- 前端改动：刷新页面即生效（`src/ui/` 是 `include_str!` 编译进二进制的，改完要重新 `cargo build` 再重启）
- 需真实模型/浏览器/数据库的条目标 🖐，只能手动验证

## 会话生命周期

| 功能 | 路径 | 预期 | 标记 |
|------|------|------|------|
| 创建会话 | POST /api/sessions | 201 + id（默认 `web-…` 前缀） | 🤖 |
| 发消息 | POST /api/sessions/{id}/prompt | 202 Accepted；随后 SSE 有事件（`text` 或 `prompt` 字段皆可） | 🖐（需模型） |
| 取消 turn | POST /api/sessions/{id}/cancel | 202 | 🖐 |
| compact | POST /api/sessions/{id}/compact | 202；会话可继续（压缩 delta 只进 handler，不刷滚动条） | 🖐 |
| 删除会话 | DELETE /api/sessions/{id} | 204（NO_CONTENT）；列表消失、transcript 保留 | 🤖 |
| fork | CLI `--fork <id> --at N` | 新 `fork-…` 会话（N 为 1-based turn 边界） | 🖐 |
| btw | POST /api/sessions/{id}/btw | 201 + `btw-…` id；F2/任务面板可见；可 attach 继续对话 | 🖐（**曾 500，回归重点**）|
| undo | POST /api/sessions/{id}/undo + TUI `/undo` | 200 文件回滚（`{"ok":true,"message":"已撤销…"}`）；空栈 409 中文报错 | 🤖/🖐 |
| read_image | 非视觉模型 | 不附加图片、不锁死会话（**回归重点**） | 🖐 |
| read_image | 视觉模型（vision=true） | 图片附加，模型能看图 | 🖐 |
| delegate | 后台子代理 | 完成自动注入 `[background task N completed]` | 🖐 |
| 背景 bash | GET /api/tasks + DELETE /api/sessions/{id}/tasks/{task_id} | 任务列表（跨会话扁平快照）/取消 | 🤖 |

## 已知缺陷与回归重点

| Bug | 现象 | 根因 | 现状态 | 回归验证 |
|-----|------|------|--------|----------|
| btw fork 500 | POST /api/sessions/{id}/btw 真实环境 HTTP 500（单测 5 个全过） | BackgroundTasks 的 completion sender clone 不共享，btw 子代理完成事件无处投递 | ✅ 已修（`fix(btw): share the background completion sender`） | 真实模型下 btw 一次成功：201 + `btw-…` id，任务面板可见，可 attach |
| read_image 锁死会话 | 非视觉模型下 read_image 把图片塞进历史，之后模型调用 gate 拦截所有后续请求，会话永久锁死 | 图片附加未按模型 vision 能力 gate，污染了 replay 历史 | 已修/在修 | 非视觉模型 read_image 后：图片**不**附加，会话仍能正常继续对话 |
| banner 关不掉 | 校验类 warn 与普通提示共用同一个 banner 槽位，出现后关不掉/关掉又刷回来 | banner 由「校验提示占用」与「普通提示计时器」两条路径写，状态互不清理（`app.js` validateBannerUp/bannerTimer） | 有独立关闭按钮 + 计时器清理逻辑 | 触发一次 warn（如 session 字段缺失）→ 点 × 能关；普通提示自动消失；两者不互相覆盖 |

## 桌宠（前端）

全部手动验证（🖐），需要真实模型产出一轮 turn：

- **状态总结**：每个 turn（Busy→Idle）结束时生成一句话中文总结并缓存（`GET /api/sessions/{id}/summary`）；气泡要显示**完整句子**，不截断
- **拖拽**：桌宠可自由拖拽，不遮挡输入框/侧边栏；位置刷新后合理
- **梗台词**：点击桌宠随机弹出 DeepSeek 鲸鱼梗台词
- **会话状态 badge**：侧边栏会话树中 live / busy / idle / subagent 状态 badge 正确（桌宠与列表状态一致）
- 回归：无活动 turn 的会话不显示过期总结

## 会话后端

- **JSONL 默认**：`[session]` 不配置时走 jsonl 文件后端，transcript 落盘 `<workspace>/.e-agent/sessions/…`（或等价 root）
- **Greptime**：`config.toml` 写
  ```toml
  [session]
  backend = "greptime"
  conn = "host=127.0.0.1 port=4002 dbname=public"
  ```
  需启用 `greptime` feature、GreptimeDB 先启动；元数据入审计表，`GET /api/sessions` 合并历史会话
- **CLI `--fork` 在 Greptime 后端同样工作**：fork 在新库写 `fork-…` 前缀的新会话（`session_factory` 统一 `new_id_prefixed("fork-")`，与后端无关）
- 切换后端后冒烟：创建 → 列表 → 删除 一轮走通；历史会话在列表中可见、可 resume

## 自动冒烟（scripts/smoke.sh）

```sh
scripts/smoke.sh [--port PORT] [--token TOKEN]
```

覆盖所有不依赖真实模型的端点（列表/创建/undo 空栈/history/删除/btw 空 prompt 400/prompt 空 text 400/未知 id 404/tasks）；需要模型的端点（prompt 实际效果、btw 实际 fork、SSE、compact 实际效果）在本文件手动条目中。详见脚本头部注释。
