# Windows sandbox 调研与分阶段实现方案

> 状态：**设计 / 调研，尚未实现**。
>
> 本文记录 Windows 进程树治理与 sandbox 的技术结论、阻断问题和分阶段实施门槛。当前已实现 restricted-token **写限制 MVP**，首要场景是原生 Windows 游戏开发（PowerShell/MSVC/Git/Godot/Unity/Unreal 工具链）。它不是读隔离、网络隔离或 Linux bubblewrap（bwrap）等价物；任一 token、ACL 或 native spawn 步骤失败都 **fail-closed**，不得回退为裸 shell。

## 1. 定位、威胁模型与保证分层

Windows 实现必须把“进程生命周期”与“安全隔离”分开描述：

| 层级 | 机制 | 可以保证 | 不可以保证 |
|---|---|---|---|
| 已实现 MVP：写限制 | restricted primary token（`DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED`）+ 稳定 capability SID/ACL | workspace（按 `workspace_writable`）及显式 `writable_paths` 写入；原生工具链兼容 | 不限制读取/网络；Everyone/logon-SID 本来可写的公共位置可能仍可写 |
| 后续生命周期增强 | Job Object，`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`，原子入 job | 取消、墙钟超时或后台 registry 销毁时终止该命令的子/孙进程 | 不阻塞写限制 MVP；本身不提供文件或网络隔离 |
| Phase B：隔离技术原型 | AppContainer profile/SID、security capabilities、ACL 实验 | 验证文件访问和网络 capability 的可行边界与兼容性 | 尚不承诺正式配置语义、可靠清理或与 bwrap 等价 |
| Phase C：正式 Windows sandbox | 原子创建时同时应用 Job Object 与 AppContainer；经审定的 ACL 生命周期 | 在明确支持范围内实现 workspace/path 权限、网络能力和进程树清理；任一步失败都关闭执行 | 无 mount namespace、tmpfs HOME 或 bind mount 覆盖；不承诺任意 Windows 环境均兼容 |

威胁模型针对由 shell 启动的不受信命令及其后代：

- Phase A 只防止取消/超时后残留进程继续运行，以及 PID 驱动清理造成的错误目标风险；它不改变命令对宿主资源的权限。
- 完整 sandbox 才讨论 workspace 外文件访问、写权限和网络访问。其安全边界依赖 AppContainer、访问令牌、宿主 DACL、capability 以及 Windows 本身的访问检查共同成立。
- 任何 AppContainer 创建、profile/SID 解析、属性列表构造、ACL 准备、管道准备或进程创建失败，都不得改为普通用户权限启动。
- 在 Phase C 完成前，即使 Phase A 已上线，Windows `[sandbox] enabled = true` 仍须 fail-closed；Job Object 不能被当成 enabled 模式的降级 sandbox。

## 2. 已确认的设计决策

| 主题 | 决策 | 理由 / 约束 |
|---|---|---|
| Job Object 启用条件 | 与 `[sandbox]` 开关无关；Windows 所有前台和后台 shell（包括 `enabled = false`）都使用 | 生命周期治理不是文件/网络 sandbox；禁用 sandbox 也不应留下进程树 |
| Job 限制 | 设置 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`；不配置/允许 breakaway，也不向子进程传 `CREATE_BREAKAWAY_FROM_JOB` | 所有后代留在同一生命周期边界内 |
| 超时 | 保留 `tokio::time::timeout` 作为墙钟 timeout，超时时终止 job 并等待进程结束 | `JOB_OBJECT_LIMIT_JOB_TIME` 是 CPU 时间语义，不能替代墙钟 timeout |
| 后台状态 | Windows 状态持有 owned/shared Job handle；不以 PID 或 `AtomicI32` 作为清理权威 | PID 可复用；稍后按 PID `OpenProcess` 可能误杀无关进程，且只能可靠指向顶层进程 |
| 原子创建 | Windows 10+ 优先使用 `STARTUPINFOEX` + `PROC_THREAD_ATTRIBUTE_JOB_LIST` | 让进程从创建时即属于 job，避免 spawn 后 assign 的逃逸窗口 |
| 原生启动层 | 允许增加一个小型 `cfg(windows)` native spawn helper，同时承载属性列表、security capabilities 和 stdio 管道 | Tokio `Command` 不足以配置这里需要的完整 `STARTUPINFOEX` 属性列表；不引入通用进程框架或 trait |
| AppContainer | Phase B 仅做独立技术原型，不接正式 `[sandbox]` 配置 | ACL 生命周期与工具链兼容性尚属阻断问题 |
| 网络 | `network = false` 不授予网络 capabilities；`network = true` 第一版只授予出站客户端能力（例如 `internetClient`） | 不承诺服务端监听、入站连接或内网访问；不默认添加 `internetClientServer` / `privateNetworkClientServer` |
| 备选方案 | restricted token 只作为可能的 MVP 备选，且必须显式标注读隔离缺失 | `WRITE_RESTRICTED` 可约束写访问检查，但不能提供“workspace 外权威 deny-read” |
| 正式接入 | 仅在 Phase B 原型门槛和 ACL 方案评审通过后进入 Phase C | 避免把实验性 ACL 修改直接绑定用户配置 |

不推荐 `CREATE_SUSPENDED -> 按 PID OpenThread -> ResumeThread`。按 PID 找线程存在标识复用、选错线程和额外权限问题，也没有必要。若某个受控 fallback 必须先 suspended 再 assign，应直接保留 `CreateProcess` 返回的 `PROCESS_INFORMATION.hThread` 并使用该 handle；但正式推荐路线仍是创建时的 `PROC_THREAD_ATTRIBUTE_JOB_LIST`。

## 3. 当前代码与实际集成点

以下行号是本文修订时的定位锚点；实现时以符号搜索为准，不依赖旧文档中的文件长度或“约多少行”。

| 路径 | 当前位置 | 需要的改动方向 |
|---|---|---|
| `src/tools/bash.rs:58-100` | shell 工具描述当前直接描述 bubblewrap | Phase C 按平台生成准确描述；Windows 未接入前不能宣称处于 sandbox |
| `src/tools/bash.rs:157-232` | Unix `ProcessGroupGuard` 与 Windows 顶层 `TerminateProcess` 降级 | Phase A 将 Windows guard 改为拥有/共享 Job handle；正常完成后 disarm，取消/drop 时终止 job |
| `src/tools/bash.rs:234-300` | `Shell` 与 Windows PowerShell/Git Bash 检测 | 原生 spawn helper 必须保留现有 shell 选择、参数和 `PowerShell -NoProfile` 行为 |
| `src/tools/bash.rs:322-500` | `run_bash`、bwrap 参数构造与 spawn | 平台分流：Unix 保持 bwrap；Windows enabled=false 走带 Job 的普通 shell；Phase C enabled=true 走 Job + AppContainer 原子创建 |
| `src/tools/bash.rs:503-574` | PID/process-group slot、取消 guard、Tokio timeout | Windows 用 Job ownership 替代 PID slot；取消和墙钟 timeout 均终止 job并 wait |
| `src/tools/background.rs:274-300` | `RunningTask.process_group: Arc<AtomicI32>` | Windows 需要独立的 shared owned Job 状态，不能把 HANDLE 塞入整数 PID slot |
| `src/tools/background.rs:455-493` | 后台 shell 建立 PID slot并调用 `run_bash` | 启动后把 job ownership 发布给 registry；处理“任务已取消但 job 尚未发布”的竞态 |
| `src/tools/background.rs:677-711` | registry drop：Unix 杀进程组，Windows 按 PID `OpenProcess` | Windows 改为终止/关闭每个 task 自己持有的 job；不得重新按 PID 打开进程 |
| `src/session_factory.rs:194-211` | 解析 sandbox 后统一执行 bwrap preflight | Phase A 按平台分流：Linux 保持 bwrap preflight；Windows `enabled = true` 由 `cfg(windows)` 明确返回“Windows sandbox backend 尚未实现”类错误，不依赖 PATH 中缺少 bwrap；Phase C 再替换为真实 Windows 后端 preflight |
| `src/tools.rs:41-48` | `bwrap_available()` | 保持为 Unix/bwrap 能力，不作为 Windows sandbox 判定依据 |
| `src/config.rs:68-105` | `Sandbox`：`enabled/network/workspace_writable/writable_paths/readable_paths` | Phase B 不接入；Phase C 才映射完整 Windows 语义并验证所有字段 |
| `Cargo.toml:43-47` | target-gated `windows-sys = 0.61` | 增加 Job/AppContainer/ACL 所需 features，依赖仍只在 Windows target 下 |

Phase A 不能改变 `enabled = false` 命令本身的 shell、参数、当前目录、环境、stdio 或退出输出；唯一新增行为是可靠的进程树生命周期清理。

## 4. Phase A：所有 Windows shell 的 Job Object 生命周期

### 4.1 必需行为

1. 为每次 Windows shell 执行创建独立 Job Object。
2. 使用 `SetInformationJobObject(JobObjectExtendedLimitInformation, ...)` 设置 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`。
3. 不设置 `JOB_OBJECT_LIMIT_BREAKAWAY_OK` 或 `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK`，创建参数也不使用 `CREATE_BREAKAWAY_FROM_JOB`。
4. 通过 `STARTUPINFOEXW` 的 `PROC_THREAD_ATTRIBUTE_JOB_LIST` 在 `CreateProcessW` 创建时将新进程放入 job。
5. 前台 guard 和后台 registry 持有 owned/shared Job handle。取消、`tokio::time::timeout`、后台取消、最后一个 registry owner drop 都应显式 `TerminateJobObject`；最后 handle 关闭时 `KILL_ON_JOB_CLOSE` 是兜底。
6. 正常等待到 shell 与管道读取完成后解除 guard/释放 registry 状态，不将正常退出报告为取消，也不影响下一次命令。
7. `enabled = false` 也执行以上步骤。Phase A 合入时，`SessionFactory` 必须按 `cfg(windows)` 对 `enabled = true` 明确返回“Windows sandbox backend 尚未实现”类错误，不得以 PATH 中恰好没有 `bwrap` 作为拒绝依据；Linux 的 bwrap preflight 保持不变。Phase C 再以真实 Windows backend preflight 替换该显式拒绝。

shared Job 状态需要表达“尚未创建 / 已发布 job / 已正常完成或已取消”。若取消先于 native spawn 完成，spawn 路径在发布 handle 时必须观察取消状态并立即终止 job，不能留下发布竞态。具体可用 `Arc<Mutex<State>>` 或等价的小型状态，但不得退回 PID + `AtomicI32`。

### 4.2 原子 native spawn helper

Tokio `Command` 可继续作为 Unix 路径，Windows Phase A 使用一个窄范围 helper：

- 接收已解析的 executable、参数、cwd、环境修改和 stdio 要求；正确构造 Windows command line。
- 接受明确的 stdin/stdout/stderr 配置。Phase A 保持当前行为：stdin 继承父进程，stdout/stderr 使用 pipe；不得在没有调用方写入的情况下擅自把 stdin 改成 pipe 或 null。只让为子进程选定或安全复制的 stdin handle 与 stdout/stderr 子端可继承，pipe 父端及所有无关 handle 均不可继承；返回可被 Tokio 异步读取/等待的拥有型对象。
- 用 `InitializeProcThreadAttributeList` / `UpdateProcThreadAttribute` / `DeleteProcThreadAttributeList` 管理属性列表。
- Phase A 在同一属性列表设置 `PROC_THREAD_ATTRIBUTE_JOB_LIST` 与 `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`。handle list 必须只列出为子进程选定或安全复制的 stdin handle 与 stdout/stderr 子端；`CreateProcessW` 的 `bInheritHandles` 必须与该白名单配套设为 `TRUE`，不得仅依赖全局 inheritable 标志。Phase B/C 再追加 `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`，而不是再造第二套 spawn 路径。
- 调用 `CreateProcessW` 时使用 `EXTENDED_STARTUPINFO_PRESENT`，并从返回的 `PROCESS_INFORMATION` 直接拥有 process/thread handles。
- 所有部分成功状态均采用严格回滚：关闭 pipe、process/thread/job handles，销毁属性列表，并保证已创建进程不会在错误返回后存活。

helper 不是通用 runner，不加入 trait，不负责 ConPTY、GUI、elevation、broker、IPC 或资源调度。

### 4.3 timeout 与结束顺序

墙钟 timeout 继续由 `tokio::time::timeout` 决定。命中后：

1. `TerminateJobObject`；
2. 等待顶层 process 收割并让 stdout/stderr reader 结束；
3. 清空后台 task 的 owned Job 状态；
4. 返回现有 timeout 形态的错误。

不使用 `JOB_OBJECT_LIMIT_JOB_TIME` 替代上述逻辑；它累计的是 job CPU 时间，睡眠或等待 I/O 的命令可能永不触发，语义与配置中的墙钟秒数不同。第一阶段也不提前加入 CPU、内存、进程数等资源限制。

## 5. Phase B：AppContainer 独立技术原型

Phase B 是 Windows-only 的独立测试程序或严格门控测试，不读取、不改变正式 `[sandbox]` 配置，也不进入 `run_bash`。原型应使用临时 profile、临时目录和可审计清理，先回答可行性与 ACL 生命周期问题。

### 5.1 创建模型

原型采用：

1. 用 `CreateAppContainerProfile` 创建 profile，或用 `DeriveAppContainerSidFromAppContainerName` 获取已存在 profile 的 SID；明确 profile 名称、复用和删除策略。
2. 构造 `SECURITY_CAPABILITIES`，设置 AppContainer SID 和最小 capability SID 集。
3. 在同一个 `STARTUPINFOEXW` 属性列表中同时放入：
   - `PROC_THREAD_ATTRIBUTE_JOB_LIST`；
   - `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`。
4. 用支持 extended startup info 的原生创建路径启动进程；Job Object 仍只承担生命周期。

不要使用或声称 `windows-sys` 提供 `CreateAppContainerToken`。底层 `NtCreateLowBoxToken` 可以作为调研事实记录，但它属于低层、稳定性和支持性负担更高的路线，本方案不选择它。

### 5.2 普通 AppContainer、LPAC 与访问检查

AppContainer 不等价于“只有一个低权限用户”：

- 普通 AppContainer 与 LPAC（Less Privileged AppContainer）的默认资源可见性不同。普通 AppContainer 可能受益于授予 `ALL APPLICATION PACKAGES` 等包主体的宿主权限；LPAC 更严格，兼容成本更高。原型必须明确测的是哪一种，不能把普通 AppContainer 结果外推到 LPAC。
- 文件访问不是只给 package SID 加 ACE 就结束。有效访问取决于普通用户 DACL 权限与 package/capability 权限检查的交集，并受 deny ACE、完整性级别、继承和对象类型影响。
- AppContainer 没有 bwrap 的 mount namespace、tmpfs HOME、bind/ro-bind 覆盖或独立文件系统视图。路径仍是宿主路径，ACL 修改也是真实宿主状态。
- 必须验证 shell/工具链能读取安装目录和系统 DLL，并考虑用户 profile、注册表、证书存储、代理配置、`TEMP`/`TMP` 和动态加载依赖。能启动一个简单 exe 不代表 PowerShell、Git Bash、Git、curl 或 TLS 可用。

### 5.3 网络语义

正式语义若可行，应保持最小化：

- `network = false`：不给任何网络 capability。
- `network = true`：第一版只给出站客户端能力，例如 `internetClient`；不承诺服务端监听、入站连接、局域网/企业内网、loopback 特例或发现协议。
- 验收必须分别覆盖 DNS 解析、TCP 出站与 HTTPS/TLS；`ping` 依赖 ICMP 与环境策略，不能作为唯一网络判据。

## 6. 阻断性设计问题：ACL 是有状态的宿主修改

`SetNamedSecurityInfo` 不是无状态的 sandbox 映射。它会真实、持久地修改宿主对象 DACL；对目录设置可继承 ACE 时，权限会按 Windows 继承规则传播到符合条件的存量子项。进程结束或 AppContainer profile 删除都不会自动撤销这些 ACE。

因此不能设计成简单的 `grant_path(path, write)`：该抽象隐藏了持久副作用、原始 ACL、传播进度、引用者和回滚责任。Phase C 前必须形成并验证以下决策：

| 阻断项 | 必须回答的问题 |
|---|---|
| SID 身份 | 使用跨运行稳定 SID 还是每次唯一 SID？稳定 SID 便于复用但权限长期存在；唯一 SID 会制造孤儿 ACE 和清理压力 |
| ACE 生命周期 | 哪些原有 ACE 必须原样保留；新增 ACE 如何精确标识、撤销，何时允许保留 |
| 崩溃恢复 | 进程在递归传播或撤销中崩溃后，如何发现未完成事务并恢复；恢复元数据放在哪里 |
| 并发引用 | 两个 session/profile 同时依赖同一 ACE 时如何引用计数，避免一个结束后撤销另一个仍需的权限 |
| 权限变化 | 授权期间用户/管理员修改 ACL 时如何避免回滚覆盖其变化；不能用旧 DACL 整体回写 |
| protected DACL | 遇到 `SE_DACL_PROTECTED`、禁用继承或权限不足时是拒绝、逐项显式授权还是判为不支持 |
| 显式 ACE | 存量子项已有显式 allow/deny ACE 时，授权与撤销如何计算，顺序如何保持 |
| 传播成本 | 大型 workspace 的存量树传播耗时、部分失败、中断与进度如何处理；不能在启动路径无限递归 |
| 文件系统/路径类型 | 非 NTFS、UNC、只读卷、云占位文件等如何检测并 fail-closed；第一版可明确不支持 |
| reparse points | junction/symlink/mount point 是否跟随，如何阻止授权越出允许根，如何防止检查后替换（TOCTOU） |
| 审计与残留 | 如何记录变更、验证撤销完成，并测试失败回滚后没有 AppContainer ACE 残留 |

在这些问题有可执行方案前，Phase B 只能对受控临时目录实验，不能对真实 workspace 做不可恢复的递归授权，也不能进入正式配置。

## 7. restricted-token 备选 MVP

若 AppContainer/ACL 路线无法满足兼容性或可恢复性，可另行评估 restricted token：

- `WRITE_RESTRICTED` 等机制可让写访问接受额外 restricted SID 检查，从而约束写入范围。
- 它不能提供权威的 deny-read 边界；普通用户原本可读的 workspace 外内容仍可能可读。
- 若选择该 MVP，产品、工具描述、错误信息和 README 都必须明确：**不保证 workspace 外不可读**，不得称其与 bwrap 或完整 Windows sandbox 等价。
- 只复用本项目现有 pipe shell 模型；不复制 Codex 的 elevated runner、broker/IPC、ConPTY 或整套 Windows 执行框架。

restricted-token MVP 已实现失败关闭和真实 Windows native spawn；Job Object 改为后续生命周期增强，不阻塞 MVP。它只是防止事故性误写的机制，不能称为安全 sandbox 或文件系统隔离。写根仅为按配置启用的 workspace 和全部 `writable_paths`；`workspace_writable = false` 不添加 workspace 写 ACE，`readable_paths` 不授权写入。无写根时只构造 inert capability SID，不设置文件 ACE。capability SID 由 canonical path 原始 UTF-16 code units 的长度分隔字节与 class 经 SHA-256 稳定派生，保留 canonical path 的原始拼写。只接受已存在、canonical、fixed local NTFS 的目录根；拒绝 UNC/device path、非目录根、根自身为 symlink/reparse point、NULL DACL 及 case-sensitive 根，不声称支持 case-sensitive directory。所有写根会在任何 token 或 ACL 修改前以 `symlink_metadata`/`read_dir` 递归扫描且不跟随链接；hard-linked 文件 descendants 与嵌套 symlink/reparse descendants 不受支持并 fail-closed。TokenDefaultDacl 保留 source token 的现有 DACL，只合并 capability SID 的 `GENERIC_ALL` ACE；不会向 Everyone 或 logon SID 新增该权限。`SeChangeNotifyPrivilege` 无法确认已分配时失败关闭。Windows `protect_git = true` shell 不受支持，在任何 token/ACL 变更前失败关闭；不实现 `.git` deny ACE 或 carve-out。成功添加的 synthetic SID ACE 持久保留；若多根 ACL 添加中途失败则不启动进程，先前 ACE 可能持久留下，但对不含 synthetic SID 的普通 token inert，不做危险的整 DACL rollback。不默认放行 TEMP/TMP、HOME、Cargo/NuGet 或引擎缓存。Everyone/logon-SID 原本可写的公共位置仍可能可写，workspace 外仍可读且网络不隔离。检查后并发路径替换（TOCTOU）仍是 remaining risk。policy anchor 没有在 MVP 中猜测性加 deny；现有 `.e-agent/config.toml` 若位于可写根内仍可能被修改。

## 8. Phase C：门槛通过后的正式接入

只有 Phase B 验证矩阵通过、ACL 生命周期方案完成评审并有失败回滚测试后，才实施正式接入：

1. `SessionFactory` sandbox preflight 按平台分流：Linux 保持现有 bwrap probe；Windows 用真实后端对文件系统支持、profile/SID、ACL 准备与 native spawn 能力做检查，并替换 Phase A 的“Windows sandbox backend 尚未实现”显式拒绝。
2. Windows `enabled = false` 继续采用 Phase A Job Object + 普通 shell；命令行为保持不变。
3. Windows `enabled = true` 在一次原子创建中应用 Job list 与 security capabilities。ACL、profile、属性列表、pipe、process 创建中任一步失败都 fail-closed，并执行已审定回滚。
4. 将 `workspace_writable`、`writable_paths`、`readable_paths`、`.git`/policy anchor 保护和 `network` 逐项定义为 Windows 可验证语义；无法安全映射的组合应在 preflight 拒绝，而不是忽略。
5. 工具描述按平台和实际状态生成：Linux 可以描述 bwrap；Windows 只能描述已经生效且测试覆盖的保证。Phase A 不使用 “sandboxed” 字样。
6. 增加 Windows CI 编译检查，并在真实 Windows runner/机器运行功能与安全回归测试；仅交叉 `cargo check` 不足以验收 ACL、Job 与网络行为。
7. README 已同步当前 Windows sandbox 事实与非目标；后续实现时再按实际测试覆盖更新已兑现的平台保证及 restricted-token 限制。

## 9. 验收矩阵

### 9.1 Phase A

在 PowerShell 与 Git Bash（可用时）分别覆盖：

- shell 启动子进程、孙进程并记录其 PID；前台取消后全部退出且无残留。
- 墙钟 timeout 后子/孙进程全部退出；`Start-Sleep`/`sleep` 等低 CPU 命令也必须按墙钟触发。
- 后台 `cancel_background_task` 后全部退出。
- 最后一个后台 registry owner drop 后全部退出，包括 job 尚在发布过程中的竞态用例。
- 正常命令完成后不被误报为取消，不影响已完成输出，也不误杀后续新命令。
- `enabled = false` 时 shell 选择、参数、cwd、环境、管道输出、退出码和前后台行为与改动前一致；只新增进程树清理保证。
- Windows `enabled = true` 由 `cfg(windows)` 明确报“Windows sandbox backend 尚未实现”类错误并 fail-closed，不能依赖 PATH 中没有 bwrap，也不能仅因 Job Object 可用而执行；Linux bwrap preflight 保持原行为。
- 并发启动多个前台/后台命令，各自只进入自己的 job，取消或完成互不干扰。
- 启动前预置无关的 inheritable handle，子进程只能继承 `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 白名单中为其选定或安全复制的 inherited stdin handle 与 stdout/stderr 子端；pipe 父端及预置无关 handle 均不可继承。另验证前台和后台命令仍使用改动前的 inherited stdin 语义。
- 分别对 Job 创建、`SetInformationJobObject`、pipe 创建、attribute list 初始化/更新及 `CreateProcessW` 注入故障；每个部分成功路径均证明无存活进程、无 handle 泄漏。
- Linux 执行 `cargo fmt --all`、`cargo clippy --all-targets -- -D warnings`、`cargo test`、`cargo build`，全部通过。

### 9.2 Phase B 原型

至少验证并记录普通 AppContainer/LPAC 类型、Windows 版本、文件系统类型与 shell 安装来源：

- PowerShell 使用 `-NoProfile` 启动；Git Bash 可用时也启动。
- stdout/stderr pipe、大输出、退出、取消和 handle 清理正常。
- `TEMP`/`TMP` 创建、读取、删除临时文件。
- PowerShell/Git Bash 安装目录、系统 DLL、必要注册表和证书/TLS 依赖。
- workspace 只读与可写两种模式，以及额外 readable/writable path；拒绝项不能因用户原权限而意外放行。
- `network=false` 下 DNS/TCP/HTTPS 均失败；`network=true` 下 DNS/TCP/HTTPS 出站成功，但不把 ping 成败当作唯一结论。
- reparse point 指向 workspace 外、运行中替换目标、嵌套 junction 等逃逸用例。
- ACL 的继承传播、protected DACL、显式 deny/allow、部分失败、崩溃恢复、并发引用和撤销。
- 每个成功、失败及强制中断用例结束后检查 ACL/profile 残留；失败回滚不得扩大宿主权限。
- 非 NTFS/UNC 明确拒绝或有单独验证，不得静默按 NTFS 语义继续。

### 9.3 Phase C

- `Sandbox` 全部字段在 Windows 有文档化、平台化测试覆盖的语义；不支持的输入在启动前报错。
- `enabled=false` 为 Phase A 生命周期治理但无文件/网络隔离；`enabled=true` 才表示已生效的完整 Windows 后端。
- `network=false/true`、workspace RO/RW、额外路径、前后台、取消、timeout、registry drop 均组合测试。
- 并发启动多个完整 sandbox 命令，各自的 Job、profile/capabilities 与 ACL 生命周期互不串扰；取消或失败一个命令不影响其他命令。
- 启动前预置无关的 inheritable handle，完整 sandbox 子进程仍只能继承 `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 白名单中为其选定或安全复制的 stdin handle 与 stdout/stderr 子端，pipe 父端及无关 handle 均不可继承；若 Phase C 改变 stdin 策略，必须明确记录并分别验证前台、后台与控制台场景。
- 在 Phase A 通用 spawn 故障注入基础上，仅追加 profile 创建/复用、SID 派生、ACL 准备/回滚与 `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` 更新故障；证明不裸跑、无存活进程、handle、ACL 或 profile 残留，不在 Phase C 重复或后移 Phase A 的 Job、pipe、attribute list 基础设施及 `CreateProcessW` 故障责任。
- Linux bwrap 路径与全套 fmt/clippy/test/build 无回归；Windows 真实测试通过。

## 10. 非目标

为符合项目“先做最小工作闭环、避免过度设计”的原则，本文明确不做：

- 不复刻 bwrap，也不承诺 Windows 与 Linux sandbox 等价。
- 不提供 mount namespace、tmpfs HOME、bind 覆盖或任意宿主路径的虚拟文件系统视图。
- 不做 GUI、交互终端或 ConPTY。
- 不做 broker、elevated runner、agent IPC 或另一个调度框架。
- Phase A 不先做 CPU、内存、进程数等资源限制。
- 第一版不承诺任意 PowerShell 模块、Git/语言工具链、注册表依赖、证书环境都可用。
- 第一版不承诺非 NTFS、UNC 或所有 reparse-point 场景；未验证场景必须拒绝而非降级。
- 网络开启不承诺服务端监听、入站或内网访问。
- 如果最终采用 restricted-token MVP，不保证 workspace 外读取隔离。

## 11. Windows API 与依赖清单

核心 API/类型：

- Job：`CreateJobObjectW`、`SetInformationJobObject`、`TerminateJobObject`、`JOB_OBJECT_EXTENDED_LIMIT_INFORMATION`、`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`。
- 原子创建：`STARTUPINFOEXW`、`InitializeProcThreadAttributeList`、`UpdateProcThreadAttribute`、`DeleteProcThreadAttributeList`、`PROC_THREAD_ATTRIBUTE_JOB_LIST`、`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`、`CreateProcessW`、`EXTENDED_STARTUPINFO_PRESENT`。
- AppContainer：`CreateAppContainerProfile`、`DeriveAppContainerSidFromAppContainerName`、`DeleteAppContainerProfile`、`SECURITY_CAPABILITIES`、`PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`。
- ACL 调研：`SetEntriesInAclW`、`GetNamedSecurityInfoW`、`SetNamedSecurityInfoW` 及 SID/ACL/authorization 辅助 API。

`windows-sys 0.61` 的 Windows target 依赖应保留现有 `Win32_Foundation`、`Win32_System_Threading`，并按实际使用的 Job 与 AppContainer/ACL 符号补齐 `Win32_System_JobObjects`、`Win32_Security`、`Win32_Security_Isolation`、`Win32_Security_Authorization` 等安全/启动核心 features。除此之外，I/O feature 取决于最终 pipe 方案：匿名 `CreatePipe` 方案可能需要 `Win32_System_Pipes`；若为 Tokio/IOCP 采用 overlapped named pipe，还需相应的 `Win32_Storage_FileSystem` 等 feature。不得在设计阶段武断固定一组可能错误的 I/O features，最终必须以实际实现的最小化 Windows 编译确认。

不使用不存在/不适用的 `Win32_System_Token` 或 `Win32_NetworkManagement` feature，也不据此假定有 `CreateAppContainerToken` 封装。
