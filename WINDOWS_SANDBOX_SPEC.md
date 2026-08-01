# Windows 沙盒实现方案（Job Object + AppContainer）

> 状态：**设计文档**（2026-08-01）。Windows 移植 C 方案已合入 main（bash 走 PATH、文件写入降级、进程 kill 降级、HOME/XDG、`\\?\` normalize），但**沙盒未实现**。当前 Windows 上配置 `[sandbox] enabled = true` 会 fail-closed 报错 `bwrap not found`（bwrap 是 Linux 专用）。本文档是 Windows 沙盒的完整实现方案。
>
> 参考实现：微软 `mxc`、OpenAI `codex-rs`（`windows-sandbox-rs`：受限 token + Job Object + ConPTY）、`rappct` crate（AppContainer/LPAC）。

---

## 1. 目标与范围

### 1.1 目标

让 Linux 上 `[sandbox]` 配置的语义在 Windows 上**等价成立**：

| Linux（bwrap） | Windows（本方案） | 对标点 |
|---|---|---|
| 进程树隔离（--die-with-parent / 进程组 kill） | Job Object（TerminateJobObject / KILL_ON_JOB_CLOSE） | 进程 |
| 文件系统隔离（--ro-bind / --bind / 只读系统目录） | AppContainer / 受限 token 文件权限 | 文件 |
| 网络隔离（--unshare-net） | AppContainer 网络能力（InternetClient 等） | 网络 |
| workspace 可写 / 只读 | AppContainer 对 workspace 目录的 ACL（写/读） | 工作区 |

### 1.2 非目标（明确不做）

- 不做 Linux bwrap 的完整语义复刻（如 mount namespace、tmpfs $HOME 隐藏——Windows 无对应物）。
- 不做 AppContainer 之外的完整安全边界（如完整性级别以外的防御纵深）。
- 不做沙盒内图形/交互（Windows 沙盒内跑 GUI 程序是后续话题）。
- 第一版不做 per-path 细粒度 ACL 动态调整（`writable_paths`/`readable_paths` 的运行时映射，见 §4.3 取舍）。

### 1.3 验收标准

- `[sandbox] enabled = true` 时 bash 命令在 Job Object 内运行，`network = false` 时无网络，`workspace_writable = false` 时 workspace 只读。
- 取消/超时 → 整个进程树被终止（Job Object `TerminateJobObject`），无孤儿进程。
- `enabled = false`（默认）→ 行为与现在完全一致（裸 bash）。
- Linux 构建/测试零回归（全部 `cfg(windows)` 门控）。

---

## 2. 现状与集成点

### 2.1 相关代码

- `src/tools/bash.rs`：
  - `bash_executable()`（Windows 搜 PATH / Git for Windows）——已实现。
  - `ProcessGroupGuard`（`cfg(windows)` 降级为顶层 `TerminateProcess`）——**Job Object 方案要替换/升级这里**。
  - `run_bash()` 的 `Some(sandbox) =>` 分支（约 266 行）构造 bwrap 参数——**Windows 上此分支要换成 Job Object + AppContainer 启动路径**。
- `src/tools/background.rs`：`BackgroundRegistry::Drop` 的 `cfg(windows)` 顶层 kill——**同样升级为 Job Object kill**。
- `src/config.rs`：`Sandbox` 结构（enabled/network/workspace_writable/writable_paths/readable_paths）——**跨平台共用，不改**。
- `Cargo.toml`：`[target.'cfg(windows)'.dependencies] windows-sys`（已就位，需加 feature）。

### 2.2 设计原则

1. **全部 `cfg(windows)` 门控**：Linux 路径逐字节不动（AGENTS.md：Linux 是主平台，Windows 是 C 方案降级）。
2. **沙盒可选**：`enabled = false` 时走现有裸 bash；`enabled = true` 才创建 Job Object + AppContainer。
3. **失败即报错**（fail-closed）：Job Object / AppContainer 创建失败 → 返回清晰错误，不静默降级为裸跑。

---

## 3. 组件设计

### 3.1 Job Object（进程树隔离 + kill）

```rust
/// Windows 进程容器：AssignProcessToJobObject 后后代自动继承，
/// TerminateJobObject 杀全树。对标 Linux 进程组 / --die-with-parent。
#[cfg(windows)]
pub struct JobObject {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl JobObject {
    /// 创建 Job Object（可命名，便于诊断），配置：
    /// - JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE：句柄全关 → 杀全树（对标 --die-with-parent）
    /// - JOB_OBJECT_LIMIT_ACTIVE_PROCESS：可选，限制进程数（防 fork 炸弹）
    /// - JOB_OBJECT_LIMIT_JOB_MEMORY / JOB_OBJECT_LIMIT_PROCESS_MEMORY：可选，限内存
    /// - JOB_OBJECT_LIMIT_JOB_TIME：可选，限总 CPU 时间（对标 timeout）
    pub fn new() -> std::io::Result<Self> { /* CreateJobObjectW */ }

    /// 把进程（及其后代）放进 Job Object。spawn 前必须设置
    /// CREATE_SUSPENDED + 先 Assign 再 Resume，避免竞态（进程在 Assign
    /// 前已退出/逃逸）。或 spawn 后立即 Assign（竞态窗口小，可接受，
    /// 但严格做法是 CREATE_SUSPENDED）。
    pub fn assign(&self, process_handle: HANDLE) -> std::io::Result<()> { /* AssignProcessToJobObject */ }

    /// 杀全树（对标 kill 进程组）。
    pub fn terminate(&self, exit_code: u32) { /* TerminateJobObject */ }

    /// 可选：限制（内存/CPU/进程数）——通过 SetInformationJobObject。
    pub fn set_limits(&self, limits: JobLimits) -> std::io::Result<()> { /* ... */ }
}

/// 挂到 Drop：Job Object 句柄关闭时 KILL_ON_JOB_CLOSE 自动杀全树，
/// 覆盖「bash 命令超时/取消/进程崩溃」所有路径。
#[cfg(windows)]
impl Drop for JobObject {
    fn drop(&mut self) {
        // CloseHandle 触发 KILL_ON_JOB_CLOSE（若已设置）
    }
}
```

**集成到 `run_bash`**：
- `Some(sandbox)` + Windows：创建 `JobObject`，spawn bash 后 `assign`，`cancel_guard` 从 `ProcessGroupGuard`（顶层 TerminateProcess）换成 `JobObject`（terminate 全树）。
- `BackgroundRegistry::Drop` 的 Windows 分支：从 `OpenProcess+TerminateProcess`（顶层）换成**按 task 存的 JobObject handle**（或存 job name，Drop 时 OpenJobObject + TerminateJobObject）。

### 3.2 AppContainer（文件/网络隔离）

```rust
/// Windows AppContainer：低权限容器，对标 bwrap 的 mount/网络隔离。
/// 通过受限 token + 容器 SID 控制文件 ACL 与网络能力。
#[cfg(windows)]
pub struct AppContainer {
    // 容器 SID + 受限 token（CreateAppContainerToken）
}

#[cfg(windows)]
impl AppContainer {
    /// 创建容器（DeriveAppContainerSidFromAppContainerName）+ 受限 token
    /// （CreateAppContainerToken），配置：
    /// - 默认拒绝所有文件（容器 SID 无任何 ACL）
    /// - 显式授予：workspace（可写/只读由 workspace_writable 决定）、
    ///   writable_paths（写）、readable_paths（读）、系统运行所需的最小
    ///   目录（如 Git Bash 自身安装目录、%WINDIR% 只读、%TEMP% 写）
    pub fn new(name: &str) -> std::io::Result<Self> { /* ... */ }

    /// 授予某路径读/写权限（SetEntriesInAcl / SetNamedSecurityInfo 给容器 SID 加 ACE）。
    pub fn grant_path(&self, path: &Path, write: bool) -> std::io::Result<()> { /* ... */ }

    /// 网络能力：network=true → 加 InternetClient / InternetClientServer；
    /// network=false → 不加（无网络）。
    pub fn set_network(&self, enabled: bool) { /* ... */ }

    /// 用受限 token 启动进程（CreateProcessAsUser / CreateProcessWithToken）。
    pub fn spawn(&self, cmd: &Command) -> std::io::Result<Child> { /* ... */ }
}
```

**集成到 `run_bash`**：
- `Some(sandbox)` + Windows：先建 `JobObject`，再建 `AppContainer`，`AppContainer::spawn` 启动 bash（token 受限），`JobObject::assign` 把进程放进容器。两者叠加 = 进程树隔离 + 文件/网络隔离。

### 3.3 路径/权限映射（对标 bwrap 参数）

| Linux bwrap 参数 | Windows AppContainer 等价 |
|---|---|
| `--ro-bind /usr /usr` 等系统目录只读 | AppContainer 默认对系统目录无写权限（天然只读）；需显式授予读（%WINDIR%、Git 安装目录）|
| `--bind <workspace> <workspace>`（可写） | `grant_path(workspace, write=true)` |
| `--ro-bind <workspace> <workspace>`（只读） | `grant_path(workspace, write=false)`（只给读）|
| `--ro-bind <readable_paths> ...` | 逐个 `grant_path(p, false)` |
| `--bind <writable_paths> ...` | 逐个 `grant_path(p, true)` |
| `--unshare-net` | 不授予网络能力（无 InternetClient）|
| `--die-with-parent` | `KILL_ON_JOB_CLOSE` |
| `/tmp` 临时目录 | 授予 `%TEMP%` 写 |

### 3.4 配置兼容

- **复用现有 `Sandbox` 结构**，不改 `config.rs` 的字段/语义。
- 项目级覆盖（multi-workspace 已支持 `[sandbox]` 项目覆盖）天然兼容。
- 第一版限制（明确写进 README 已知限制）：
  - `writable_paths`/`readable_paths` 的**路径通配/嵌套**语义按「精确路径 + 目录继承」处理（AppContainer ACL 是目录级），不做 bwrap 的复杂 bind 语义。
  - `$HOME` 隐藏：Windows 上不隐藏（AppContainer 无 tmpfs 概念），但**不授予 HOME 写权限**（默认只读/拒绝），等效于「看不见也写不了」（除了显式 grant 的路径）。

---

## 4. 实现步骤（建议顺序）

### 阶段 A：Job Object（进程树 kill，最小可用）
1. `Cargo.toml` windows-sys 加 feature：`Win32_System_JobObjects`、`Win32_System_Threading`（已有）、`Win32_Foundation`（已有）。
2. 新建 `src/tools/job_object.rs`（`cfg(windows)`），实现 `JobObject`（new/assign/terminate/limits/Drop）。
3. 替换 `bash.rs` 的 `ProcessGroupGuard`（Windows 分支）与 `background.rs` 的 Drop kill 为 Job Object。
4. 验证：`enabled = true` 时取消/超时 → 整棵进程树被杀（`taskkill` 后无残留子进程）。

### 阶段 B：AppContainer（文件/网络隔离）
1. `Cargo.toml` 加 feature：`Win32_Security`、`Win32_System_Token`、`Win32_NetworkManagement`（若需要）。
2. 新建 `src/tools/app_container.rs`（`cfg(windows)`）：容器 SID + 受限 token + `grant_path` + `set_network` + `spawn`。
3. `run_bash` 的 `Some(sandbox)` Windows 分支：JobObject + AppContainer 组合启动。
4. 验证：`workspace_writable=false` 时 bash 写 workspace 失败；`network=false` 时 `ping`/`curl` 失败；`network=true` 正常。

### 阶段 C：打磨
- 超时（`[bash] timeout`）在 Windows 上通过 Job Object 的 `JOB_OBJECT_LIMIT_JOB_TIME` 或现有 tokio timeout + terminate 实现。
- 错误消息友好化（"HOME is not set" → 提及 USERPROFILE）。
- 单元测试（纯逻辑部分，如路径→ACL 映射，抽成跨平台可测函数）。
- CI：`cargo check --target x86_64-pc-windows-msvc`（交叉检查编译）。

---

## 5. 风险与注意

1. **CREATE_SUSPENDED 竞态**：spawn 后 Assign 有极小竞态窗口（进程可能先退出或逃逸）。严格做法：`CREATE_SUSPENDED` spawn → Assign → `ResumeThread`。建议第一版就做对。
2. **Git Bash 依赖**：AppContainer 下 Git Bash 需要能读自己的安装目录 + %WINDIR% 系统 DLL；漏授会导致 bash 起不来——**这是最常见的坑**，阶段 B 验证重点。
3. **ACL 与继承**：`grant_path` 用目录继承（`CONTAINER_INHERIT_ACE`），子目录/文件自动继承；但已存在的文件若无继承位会漏——建议 grant 时对目标目录递归处理或接受"新建文件继承、存量文件需显式"的取舍。
4. **Job Object 嵌套**：进程可能已在别的 Job（如父进程创建的）→ `AssignProcessToJobObject` 失败（除非嵌套 Job 开启）。bash 直接 spawn 自 e-agent 主进程，通常无此问题；若遇 `ERROR_ACCESS_DENIED`，提示用户。
5. **32/64 位**：Windows 上 Git Bash 可能是 32 位而 e-agent 是 64 位，Job Object 无位宽限制，AppContainer token 也兼容——但测试时留意。
6. **ConPTY**：目前 bash 是管道（Stdio::piped）非 ConPTY；若后续要交互式终端，ConPTY + Job Object 的组合参考 codex-rs（`windows-sandbox-rs`）。

---

## 6. 参考

- OpenAI codex-rs `windows-sandbox-rs`：受限 token + Job Object + ConPTY 的完整实现（最佳参考）。
- `rappct` crate：AppContainer/LPAC 的 Rust 封装。
- 微软文档：`CreateJobObjectW`、`AssignProcessToJobObject`、`TerminateJobObject`、`SetInformationJobObject`、`CreateAppContainerToken`、`DeriveAppContainerSidFromAppContainerName`、`SetNamedSecurityInfo`。
- 本项目记忆 #7893（Windows 移植调查报告）、#7896（oracle review 结论：Job Object 里程碑 + pid 复用 TOCTOU 用 GetProcessTimes 校验）。
