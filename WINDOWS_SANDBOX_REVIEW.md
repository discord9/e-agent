# Windows 写沙箱 MVP — 审查交接报告

> 审查对象：分支 `feat/windows-write-sandbox-mvp`
> 提交：`bf46b26`（MVP）+ `1d995dd`（硬链接修复），基于 `6fb118d`
> 审查轮次：第 1 轮（`22fd6cd`，verdict **REJECT**）→ 第 2 轮复查（`1d995dd`，verdict **REJECT**）
> 日期：2026-08-02
> 方式：静态代码审查（Linux 主机，Windows 专属测试未实际执行；分支所有者的真机测试结果以其自身报告为准）

## Verdict：REJECT — 暂不合并

实现能执行真实 Windows 访问检查（restricted token + capability ACE）且整体 fail-closed（任何 token/ACL/spawn 步骤失败都不回退裸 shell），但当前仍有 **2 个可信逃逸路径**（硬链接竞态未闭环、控制台/桌面通道），作为"强制写边界"尚不安全。修复硬门槛通过后即可重新审查。

## 修复状态总表

| # | 级别 | Finding | 状态 | 证据（分支 tip `1d995dd`） |
|---|---|---|---|---|
| 1 | 🔴 Blocker | 硬链接把 capability ACE 传播到根外文件 | **部分修复**：静态场景已关闭，扫描→ACL 之间竞态未闭环 | `windows_sandbox.rs:276-317, 444, 638-646`；竞态见 `:246-273, :502-513` |
| 2 | 🔴 Blocker | 子进程继承父真实控制台 stdin + `Winsta0\Default` | **未修** | `:770-790`（复制真实 stdin）、`:818-865`（作为 hStdInput）、`:858-877`（Default 桌面、无 CREATE_NO_WINDOW） |
| 3 | 🟠 Major | 句柄检查 vs 路径名应用 TOCTOU | **未修** | `RootAcl` 只存路径+裸 ACL `:240-244, :404-474`；`SetNamedSecurityInfoW(root.path)` `:502-513` |
| 4 | 🟠 Major | capability ACE 缺 `DELETE` / `FILE_DELETE_CHILD` | **未修** | 仅 `FILE_GENERIC_READ\|WRITE\|EXECUTE` `:486-488` |
| 5 | 🟠 Major | 无 Job Object，超时/取消只杀顶层进程 | **未修** | `ProcessGuard` `:66-105`；timeout 路径 `:978-1015`；README 确认 `README.md:344-350` |
| 6 | 🟠 Major | 安全/生命周期测试矩阵不足 | **部分修复**：+1 静态硬链接回归测试 | `windows_sandbox_tests.rs:96-116`；全量清单见下 |
| 7 | 🟠 Major | **新增**：每次 shell 调用无界同步树遍历 | **未修** | `scan_descendants` `:276-317`；每命令全根重扫 `:625-642`；同步阻塞 tokio worker、不落在 timeout 内 `:974, :1003-1017`；违背 SPEC 自身约束 `WINDOWS_SANDBOX_SPEC.md:150-151` |
| 8 | 🟠 Major | **新增**：duplicate-handle 失败时命令仍在跑却报启动失败 | **未修** | `:66-89`：`TerminateProcess` 结果被忽略，关闭进程句柄不终止进程 |
| 9 | 🟡 Minor | Windows 环境变量透传警告写在 Linux 段落 | **未修** | 透传除 6 个凭证外的全部父变量 `:740-767`；警告仅 `README.md:352-364` |
| 10 | 🟡 Minor | SPEC 头部"设计/调研，尚未实现"与正文矛盾 + Phase-A 旧文案 | **未修** | `WINDOWS_SANDBOX_SPEC.md:3-5, :56, :73, :192` |

## Blocker 详情

### Blocker 1 — 硬链接越权（部分修复，竞态未闭环）

**原问题**：`set_path_ace` 以可继承 capability ACE 递归传播到现有子项（`SetNamedSecurityInfoW`），NTFS 硬链接的 ACL 属于底层文件而非路径名——工作区内指向工作区外用户可写文件的硬链接，会让 capability SID 传播到该外部文件，受限 token 即可经任意别名写入。

**修复内容（1d995dd，静态场景有效）**：
- `scan_descendants`：`symlink_metadata`（不跟随）+ `FILE_ATTRIBUTE_REPARSE_POINT` 位检查，拒绝一切 reparse descendant（junction/mount point 全覆盖）；`GetFileInformationByHandle` 的 `nNumberOfLinks > 1` 拒绝硬链接文件；任何扫描错误 `?` 传播（fail-closed）。
- 时序正确：所有写根先完成 `preflight_root`（含扫描）才开 source token / 改 ACL（`:638-646` vs `:701-707`）。
- 回归测试真实：同卷外部硬链接 → 断言报错含路径 + 外部文件内容 `"unchanged"`（`:96-116`）。

**已确认无问题**：目录硬链接（Windows 不支持，目录别名走 reparse 拒绝）；非 NTFS 卷在扫描前拒绝（`:355-401`）；`file_link_count` 的 `Handle` RAII 无泄漏（含 `GetFileInformationByHandle` 失败路径）。

**未闭环竞态**：所有检查句柄在 ACL 应用前已丢弃（`:246-273` 的 `Handle` 随函数返回 drop；root 检查句柄也不保留在 `RootAcl`），ACL 仍按路径名 `SetNamedSecurityInfoW` 应用。**扫描后、传播前**，descendant 被替换成硬链接 / root 路径被替换 → 原越权可复现。README 的 TOCTOU 承认（`:335-338`）不构成闭环。

**修复方向**：让身份稳定、排除变更的句柄从校验贯穿到 ACL 应用（句柄版 `SetSecurityInfo`），保证被授予继承权限的对象就是被扫描的对象；若无法竞态安全，不得把路径名传播设计表述为"强制写边界"。

### Blocker 2 — 子进程获得交互式主机控制台通道与默认桌面（未修）

**证据**：`duplicate_stdin` 复制真实 `STD_INPUT_HANDLE`（`DUPLICATE_SAME_ACCESS` + 可继承，`:770-790`）并作为 `hStdInput` 装入显式继承句柄列表（`:818-865`）；显式挂到 `Winsta0\Default`，创建标志无 `CREATE_NO_WINDOW`（`:858-877`）。

**风险**：复制句柄保留父进程已获授权——受限 token 不会追溯收窄既有句柄；子进程可消费/注入共享控制台输入缓冲，默认桌面保留不必要的宿主交互面（这正是微软建议 restricted-token 应用使用独立桌面的原因）。可借此操纵无限制的 agent 本身，而非仅通过受限 token 写文件。

**修复方向**：stdin 改传 EOF 匿名管道或只读 `NUL` 句柄，绝不复制父控制台；`CREATE_NO_WINDOW` 非交互启动；如需 GUI 兼容，用私有 window station/desktop 而非 `Winsta0\Default`。

## Major 详情

### Major 3 — 授权句柄检查、路径名消费的 TOCTOU（未修）
`preflight_root` 临时开句柄检查后即弃，ACL 应用与进程 cwd 都重新按路径解析。持久对象 ACE 使路径替换尤其危险。方向：保留 reparse-safe 句柄、经句柄校验卷/文件身份、`SetSecurityInfo` 用句柄、启动前复核 cwd/root 身份；或并发启动串行化并证明 fail-closed。

### Major 4 — 删除/重命名权限缺失（未修）
capability ACE 无 `DELETE` 与目录 `FILE_DELETE_CHILD`。Git checkout、编译器清理、原子保存（临时文件替换）、`Remove-Item`/`Move-Item` 都会在"可写"工作区失败。方向：文件与目录分别授予正确掩码（可用两条不同继承的 ACE），并补 create/overwrite/atomic-replace/rename/delete 测试（含根外拒绝）。

### Major 5 — 超时/取消后子孙进程仍持能力（未修，SPEC 已推后但属安全缺口）
无 `CREATE_SUSPENDED` + Job Object 原子分配；timeout 丢弃 guard 仅 `TerminateProcess` 顶层。脱离的编译器/脚本子进程仍能继续改所有允许根；子孙持有 stdout/stderr 句柄还会让阻塞的管道读取任务悬挂。方向：suspended 创建 → Job Object（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`）→ resume；timeout/取消/registry teardown 都关/杀 job；补子孙存活回归测试。

### Major 6 — 测试矩阵不足（部分修复）
当前 5 个测试：
1. `restricted_token_enforces_configured_write_roots` `:37-94`（新建/单 extra root/兄弟拒绝/`workspace_writable=false`）
2. `hard_linked_descendant_is_rejected_before_external_file_can_change` `:96-116`（静态硬链接）
3. `restricted_token_preserves_output_and_nonzero_exit` `:118-133`
4. `restricted_token_rejects_network_false_at_execution` `:135-151`
5. `protected_git_is_rejected_before_acl_preflight_or_process_start` `:153-174`

**缺口**：junction/mount-point/symlink descendant；不可读目录/枚举失败/瞬态消失项；多根场景（最后一个根失败时前序根未被动过）；删除/目录删除/重命名/原子替换；stdin EOF 且无继承真实控制台句柄；超时/取消后存活子孙；ACL 幂等性与受保护子 DACL；根/descendant 替换竞态或证明句柄策略可防。需真实 Windows CI runner 执行，不能只靠交叉编译。

### Major 7 — 新增：无界同步树遍历阻塞运行时（未修）
每次 shell 调用都对全部写根做同步递归 `read_dir`/`symlink_metadata`/`CreateFileW`，发生在 `spawn` 返回前、timeout 开始前（`:974, :1003-1017`）。大游戏工作区可无界阻塞 tokio worker 且不受 timeout 保护；任何不可读/瞬态消失的 descendant 还会让所有 shell 命令失效。与 SPEC 自己写的"启动路径不得无界递归"（`WINDOWS_SANDBOX_SPEC.md:150-151`）直接冲突。**注意**：不要用缓存扫描来"解决"——那会失效安全检查；ACL 策略本身需要有界或竞态安全设计。最低限度：`spawn_blocking` + 显式时间/条目限额，超限按"不支持的根"fail-closed 报错。

### Major 8 — 新增：duplicate 失败路径报错但命令仍在跑（未修）
进程创建成功后 `ProcessGuard::duplicate` 失败 → 调 `TerminateProcess` 但忽略结果 → 立即返回 duplicate 错误；unwind 只关进程句柄（不终止进程）。资源耗尽下工具报"启动失败"但受限命令还在执行。方向：检查 `TerminateProcess` 结果、确认终止后才报错；更优：进程先进入已构造好的 kill guard / Job Object，再做任何可失败的后续操作。

## Minor 详情

- **Minor 9**：Windows 环境块转发除 6 个 API key 外的全部父变量（`:740-767`），含 `GITHUB_TOKEN`/AWS/Azure 凭证/代理/数据库 URL；对应警告只在 `README.md:352-364` 的 Linux/macOS 段落。方向：Windows 段同样明示 + 精确列出剥离名单；如需更强，改为 allowlist/可配置 denylist。
- **Minor 10**：`WINDOWS_SANDBOX_SPEC.md:3-5` 状态头仍写"设计 / 调研，尚未实现"，紧接正文"当前已实现"；`:56, :73, :192` 的 Phase-A 旧文案仍把 Windows `enabled = true` 描述为需要"尚未实现"的 fail-closed 错误。方向：更新状态头，把过时 Phase-A 文案标记为历史或改写为描述当前 MVP。

## 已确认的正常行为（无需改动）

- 非 no-op：Windows 启用时 shell 真正分支进 `windows_sandbox::run`（`bash.rs:359-372`），失败不回退裸 shell。
- `network=false` 与 `protect_git=true` 在 token/ACL 准备前拒绝（`:890-897`），fail-closed。
- 非 Windows 平台保持 bwrap 门控，不静默变 no-op（`session_factory.rs:599-611`）。
- `windows-sys` 依赖与模块 cfg 门控正确（`Cargo.toml:43-55`、`tools.rs:13-15`）。
- 子代理一律 `protect_git=true`（`tools.rs:131-138`）→ Windows 沙箱下 delegate 当前全是拒绝执行的壳（fail-closed，但功能受限，README 已披露 `README.md:336-340`）。

## 下一轮硬门槛（按优先级）

1. **Blocker 2**：stdin 改 EOF 管道/只读 NUL；私有非交互 window station/desktop；`CREATE_NO_WINDOW`（不视为桌面隔离）。
2. **Blocker 1 竞态闭环**：句柄从校验贯穿到 `SetSecurityInfo`；或明确放弃"路径名传播 = 强制边界"的表述。
3. **Major 7**：树遍历移入 `spawn_blocking` + 时间/条目限额。
4. **Major 8**：确认终止后才报启动失败；进程先入 kill guard/Job 再做可失败操作。
5. **Major 4**：补 DELETE / FILE_DELETE_CHILD 及相应操作测试。
6. **Major 5**：Job Object 生命周期（可与 8 合并设计）。
7. **Major 6**：补全测试矩阵并在真 Windows CI 跑。
8. **Minor 9/10**：文档修订（可随时顺手做）。

## 备注

- 本报告为静态审查；Windows 专属 enforce 测试未在 Linux 执行，最终验收以真机测试为准。
- 审查期间远端另出现 `fix/windows-tui-paste` 分支（未审查，不在本报告范围）。
