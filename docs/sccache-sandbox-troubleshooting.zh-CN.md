# sccache + bubblewrap + 多 worktree 排查指南

本文面向在 e-agent/Fixer 的 bubblewrap sandbox 中并行使用 Cargo、sccache 和 Git worktree 的开发与运维人员。目标是用可复现、可对照的证据定位 `ENOENT`，而不是先入为主地重启服务或清空目录。

## 症状、已知事实与当前判断

### 观察到的症状

- sccache 报 `No such file or directory`，失败路径类似：
  `WORKTREE/target/debug/deps/.tmpXXXXXX`。
- 偶尔还会看到 build script 的 `out` 目录或其中的文件报 `ENOENT`。
- 多个 fixer worktree 曾同时冷编译；部分同一 worktree 内还并发启动了多套 Cargo gate。

### 已知事实

- Cargo wrapper 是 `/home/discord9/.local/bin/sccache-pr2678`，固定使用
  `SCCACHE_SERVER_PORT=4227`。
- sccache cache 位于 `/mnt/nvme_rust/sccache-pr2678`。
- sandbox 共享挂载 `/mnt/nvme_rust/rust-targets`、cargo-home 和 sccache cache；默认每个
  Fixer 只 bind 自己的当前 workspace。
- 改用每任务唯一的
  `/mnt/nvme_rust/rust-targets/<task>` 后，Cargo gate 已成功。
- sccache cache hit 时，会在最终 output directory 内调用
  `NamedTempFile::new_in(dir)` 创建 `.tmp*`，然后 persist 到最终文件名。因此报错中的
  `.tmp*` 不是只存在于 cache 目录里的临时文件。
- sccache 是 client/server 架构，server 是独立进程。它可能是宿主机外部预启动的 daemon，
  也可能由某次 sandbox 内调用启动；必须用进程和 namespace 证据区分。

### 当前判断等级

| 判断 | 等级 | 说明 |
| --- | --- | --- |
| 每任务唯一、各 namespace 共同可见的 target 是有效缓解措施 | 高 | 已有成功 gate 的直接观察，但这不能单独证明根因 |
| 同一 target 上的并发 Cargo、clean/rm 或目录替换可能制造竞态 | 中高 | 与 `ENOENT` 形态一致，需结合进程时间线和复现确认 |
| daemon 看不到某个 worktree/output dir | 待验证假设 | 必须检查 daemon PID 的 mount namespace；不能因使用 sandbox 就直接下结论 |
| sccache 在异常/竞态后的恢复缺陷 | 待验证假设 | 需在排除路径不可见、目录删除和并发后，用冷/热 cache A/B 证明 |
| 根因已经坐实 | **否** | 目前只能给出稳定配置和候选原因，仍需 daemon namespace 与 A/B 复现证据 |

> **重要：Git worktree common metadata 不可见是独立问题。** worktree 中的 `.git`
> 通常是一个指向主仓库 common metadata 的文件。如果 sandbox 只 bind 当前 worktree，目标
> metadata 路径可能不可见，从而导致 Git 命令失败。它需要额外 bind 或调整 workspace
> 布局，但这本身不能解释 sccache 在 Cargo output directory 内创建 `.tmp*` 时的
> `ENOENT`。不要把两类错误合并成一个根因。

## 先理解四个不同对象

1. **sccache client**：Cargo 通过 `RUSTC_WRAPPER` 启动的前端进程。它解析 rustc 调用并与
   server 通信。
2. **sccache server/daemon**：监听端口（当前约定为 4227）的独立进程，执行 cache
   查询、读取或写入，并参与把命中结果恢复到编译输出位置。它有自己的 PID、用户身份和
   mount namespace。
3. **cache directory**：当前为 `/mnt/nvme_rust/sccache-pr2678`，存放可复用缓存对象。
4. **Cargo output directory**：通常在 `$CARGO_TARGET_DIR/debug/deps`、
   `$CARGO_TARGET_DIR/debug/build/.../out` 等位置；未设置 `CARGO_TARGET_DIR` 时通常是
   当前 worktree 的 `target`。

**cache 目录可写，不代表 output directory 对 daemon 可见或可写。** 两者是不同路径；
cache hit 的 `.tmp*` 会在最终 output directory 内创建。如果 daemon 的 mount namespace
只能看到共享 cache，却看不到某个 fixer 的私有 worktree target，仍可能在恢复阶段得到
`ENOENT`。

反过来，namespace 不同也不自动等于路径不可见：同一 host path 可以被分别 bind 到两个
namespace。必须在 daemon 所在 mount namespace 中实际检查目标路径。如果 daemon 确实由
宿主机预启动，且能看到所有相关 host paths，则“daemon 在某个 sandbox namespace 内启动、
看不到其他 worktree”这一假设应被排除。

## 排查前约定

- 以下命令兼容 Bash。
- 不要执行 `cargo clean`，不要 `rm -rf target`，也不要删除 sccache cache；删除会破坏现场，
  还可能伤及其他任务。
- 将任务名改成唯一、无空格的值。所有新 target 都放在共享挂载上：

```bash
TASK_ID="replace-with-unique-task-name"
TARGET_ROOT="/mnt/nvme_rust/rust-targets"
TARGET_DIR="$TARGET_ROOT/$TASK_ID"
CACHE_DIR="/mnt/nvme_rust/sccache-pr2678"
```

- `lsns`、`namei`、`ss`、`findmnt` 可能未安装；各步骤给出了替代方法。
- 宿主机和 sandbox 可能使用不同 PID namespace。在 sandbox 中看不到宿主 daemon PID，
  不能证明 daemon 不存在。

## 从低风险到高风险逐步排查

### 1. 采集环境，不改动服务

**执行位置：宿主机普通 shell、发生错误的 e-agent sandbox/Fixer，分别各执行一次。**

```bash
env | grep -E '^(RUSTC_WRAPPER|CARGO_TARGET_DIR|CARGO_HOME|RUSTUP_HOME|SCCACHE_)=' | sort
pwd
printf 'shell PID=%s\n' "$$"
readlink /proc/$$/ns/pid
readlink /proc/$$/ns/mnt
```

比较两边的 wrapper、target、端口和 namespace inode。环境变量为空也要记录。若 `/proc` 不可
用，至少记录 `pwd`、`ps -o pid,ppid,user,args -p $$` 和后续的路径检查结果。

### 2. 核实 wrapper、真实二进制和版本

**执行位置：发生错误的 sandbox/Fixer。宿主机普通 shell 也执行一遍用于对照。**

```bash
type -a sccache
printf 'RUSTC_WRAPPER=%q\n' "${RUSTC_WRAPPER-}"
readlink -f "$RUSTC_WRAPPER"
file "$RUSTC_WRAPPER"
sed -n '1,160p' "$RUSTC_WRAPPER"
$RUSTC_WRAPPER --version
$RUSTC_WRAPPER --show-stats
```

`sed` 只适用于 wrapper 是可读脚本的情况；若 `file` 显示 ELF 二进制，不要用 `cat` 打印
它。也可用下面的只读分支自动判断：

```bash
WRAPPER="${RUSTC_WRAPPER:-$(command -v sccache)}"
REAL_WRAPPER="$(readlink -f "$WRAPPER")"
printf 'wrapper=%s\nresolved=%s\n' "$WRAPPER" "$REAL_WRAPPER"
if file "$REAL_WRAPPER" | grep -qiE 'script|text'; then
  sed -n '1,160p' "$REAL_WRAPPER"
fi
"$WRAPPER" --version
"$WRAPPER" --show-stats
```

确认脚本是否固定覆盖 `SCCACHE_SERVER_PORT=4227`，而不是仅在变量未设置时给默认值。若固定
覆盖，后面的独立端口实验不能继续使用该 wrapper，必须直接指定真实 sccache 二进制。

### 3. 查 server、端口和进程归属

**执行位置：先在宿主机普通 shell 执行。** 宿主机视角最适合确定监听者。

```bash
ss -ltnp | grep 4227
pgrep -af sccache
```

`ss` 不存在时任选其一：

```bash
lsof -nP -iTCP:4227 -sTCP:LISTEN
netstat -ltnp 2>/dev/null | grep 4227
```

`pgrep` 不存在时：

```bash
ps -ef | grep '[s]ccache'
```

先根据端口、命令行、进程用户和启动时间确认 daemon PID，**不要只取第一个 sccache
进程**。设置确认后的 PID：

```bash
PID="replace-with-confirmed-daemon-pid"
ps -o pid,ppid,lstart,user,args -p "$PID"
grep -E '^(Name|Pid|PPid|Uid|Gid|NSpid):' "/proc/$PID/status"
readlink "/proc/$PID/ns/pid"
readlink "/proc/$PID/ns/mnt"
readlink /proc/$$/ns/pid
readlink /proc/$$/ns/mnt
lsns -t mnt -p "$PID"
```

若没有 `lsns`，`readlink /proc/$PID/ns/mnt` 的 `mnt:[inode]` 已可用于初步比较。读取其他
用户进程的 `/proc` 或使用 `lsns` 可能需要宿主机管理员权限。

**执行位置：两个不同的 sandbox/Fixer，各执行一次。**

```bash
printf 'workspace=%s shell PID=%s\n' "$PWD" "$$"
readlink /proc/$$/ns/pid
readlink /proc/$$/ns/mnt
pgrep -af sccache || true
ss -ltnp 2>/dev/null | grep 4227 || true
```

记录两个 sandbox 的 mount namespace inode。由于 PID namespace 隔离，`pgrep` 或 `ss -p`
可能看不到宿主 PID/进程名；但 client 仍可能通过端口连接外部 daemon。这不是矛盾。

如要证明 daemon 实际能否看到某个 target，应从**宿主机普通 shell**进入 daemon 的 mount
namespace 做只读检查（通常需要 root 或相应 capability）：

```bash
TARGET_TO_CHECK="/mnt/nvme_rust/rust-targets/replace-with-existing-task"
sudo nsenter -t "$PID" -m -- sh -c '
  echo "mount namespace: $(readlink /proc/self/ns/mnt)"
  stat -- "$1"
  if command -v namei >/dev/null 2>&1; then namei -l -- "$1"; fi
  if command -v findmnt >/dev/null 2>&1; then findmnt -T "$1"; fi
' sh "$TARGET_TO_CHECK"
```

`nsenter` 失败时不要据此断言路径不可见；记录权限错误，请有权限的宿主机管理员执行。若
外部 daemon 的 namespace 内 `stat` 成功，并且其运行用户有适当权限，则应排除“daemon
局限在某个 fixer sandbox，因而看不到该共享 target”的假设。

### 4. 检查路径逐级可见性、挂载和权限

**执行位置：发生错误的 sandbox/Fixer。对 cache、target、实际报错的 output dir 分别执行。**

```bash
OUTPUT_DIR="$TARGET_DIR/debug/deps"
namei -l "$CACHE_DIR"
namei -l "$TARGET_DIR"
namei -l "$OUTPUT_DIR"
stat -c '%n type=%F mode=%a owner=%U:%G dev=%D inode=%i' \
  "$CACHE_DIR" "$TARGET_DIR" "$OUTPUT_DIR"
findmnt -T "$CACHE_DIR"
findmnt -T "$TARGET_DIR"
findmnt -T "$OUTPUT_DIR"
test -w "$CACHE_DIR" && echo 'cache: writable' || echo 'cache: NOT writable'
test -w "$OUTPUT_DIR" && echo 'output: writable' || echo 'output: NOT writable'
```

目录不存在时，`stat`/`namei` 的失败本身就是证据；不要为了“修复”现场先手工创建深层
`debug/deps` 或 build-script `out`。`namei` 不存在时，用 `stat`/`ls -ld` 检查已知父目录；
`findmnt` 不存在时可查看：

```bash
cat /proc/self/mountinfo
ls -ld -- "$TARGET_ROOT" "$TARGET_DIR" "$OUTPUT_DIR"
```

对一个**已经存在且确认属于当前任务**的 output dir，可执行无覆盖的临时文件创建与 rename
探针。它只创建随机名称并立即删除，不触碰已有文件：

```bash
probe_dir="$OUTPUT_DIR"
if [ ! -d "$probe_dir" ]; then
  printf 'skip: directory does not exist: %s\n' "$probe_dir" >&2
else
  src="$(mktemp -- "$probe_dir/.e-agent-sccache-probe.XXXXXX")" || exit 1
  dst="${src}.renamed"
  trap 'rm -f -- "${src:-}" "${dst:-}"' EXIT
  printf 'probe\n' >"$src"
  stat -- "$src"
  mv -- "$src" "$dst"
  stat -- "$dst"
  rm -- "$dst"
  trap - EXIT
  echo 'create + rename: OK'
fi
```

该探针只证明**当前 shell 用户和当前 mount namespace**可操作此目录；它不能替代 daemon
namespace 中的检查。若需要 daemon 身份测试，应由管理员在确认用户和 namespace 后进行，
不要通过放宽为 `chmod 777` 来试错。

### 5. 排除同一 target 并发和目录生命周期竞态

**执行位置：宿主机普通 shell和发生错误的 sandbox/Fixer。**

```bash
pgrep -af 'cargo|rustc|sccache'
```

替代命令：

```bash
ps -ef | grep -E '[c]argo|[r]ustc|[s]ccache'
```

检查这些进程的 `CARGO_TARGET_DIR`、工作目录和启动时间。若权限允许，可在宿主机查看：

```bash
for p in $(pgrep -f 'cargo|rustc|sccache'); do
  printf '\nPID %s cwd=' "$p"
  readlink "/proc/$p/cwd" 2>/dev/null || true
  tr '\0' '\n' <"/proc/$p/environ" 2>/dev/null \
    | grep -E '^(CARGO_TARGET_DIR|RUSTC_WRAPPER|SCCACHE_SERVER_PORT)=' || true
done
```

Cargo 输出 `Blocking waiting for file lock on build directory` 是共享 target 的直接提示。Cargo
锁能协调一部分写入，但不能保护来自外部脚本的 `clean`/`rm`，也不应据此认为在同一 target
并发跑多套 `clippy`、`test`、`build` gate 是稳定配置。

排障期间遵守：

- 一个 target 同一时间只运行一套 Cargo gate；
- 不在另一个终端、CI hook 或 fixer 中清理这个 target；
- 每个任务使用不同的 `$CARGO_TARGET_DIR`；
- 保留失败现场，记录报错路径是否在编译过程中消失或 inode 改变。

### 6. 安全采集 sccache debug 日志

**执行位置：发生错误的 sandbox/Fixer；确认该 target 当前没有其他 Cargo 后执行。**

```bash
LOG_DIR="${TMPDIR:-/tmp}/e-agent-sccache-$TASK_ID"
umask 077
mkdir -p -- "$LOG_DIR"
SCCACHE_LOG=debug \
SCCACHE_ERROR_LOG="$LOG_DIR/sccache-error.log" \
CARGO_TARGET_DIR="$TARGET_DIR" \
cargo check --locked 2>"$LOG_DIR/cargo-stderr.log"
printf 'logs: %s\n' "$LOG_DIR"
```

先运行单条、最小的 Cargo 命令，不要同时启动整套 gate。已有 daemon 是否采纳 client 进程的
`SCCACHE_LOG` 取决于版本和启动方式；若日志没有 server 端细节，也是一项需记录的结果，
不要立即重启共享 server。

日志可能包含绝对路径、用户名、仓库名、编译器完整命令行、feature 和环境参数。请保持
`umask 077`，粘贴或公开前脱敏；不得把 token、私有 URL 或源码参数原样发布。

### 7. 无破坏的 A/B 实验

所有 A/B 都应：使用此前不存在的唯一 target 名、串行运行、保留 stderr 和 stats、禁止
`cargo clean`/`rm`。先定义一批不与用户目录冲突的名称：

```bash
RUN_ID="$(date +%Y%m%d-%H%M%S)-$$"
BASE="/mnt/nvme_rust/rust-targets/${TASK_ID}-${RUN_ID}"
TARGET_NO_SCCACHE="${BASE}-no-sccache"
TARGET_SHARED_PORT="${BASE}-shared-port"
TARGET_CACHE_FIRST="${BASE}-cache-first"
TARGET_CACHE_SECOND="${BASE}-cache-second"
printf '%s\n' "$TARGET_NO_SCCACHE" "$TARGET_SHARED_PORT" \
  "$TARGET_CACHE_FIRST" "$TARGET_CACHE_SECOND"
```

确认这些路径不存在。若任何一个已存在，换 `RUN_ID`，不要删除：

```bash
for d in "$TARGET_NO_SCCACHE" "$TARGET_SHARED_PORT" \
         "$TARGET_CACHE_FIRST" "$TARGET_CACHE_SECOND"; do
  if [ -e "$d" ]; then
    printf 'REFUSE: target already exists: %s\n' "$d" >&2
    exit 1
  fi
done
```

#### A/B 1：禁用与保留 sccache

**执行位置：同一个发生错误的 sandbox/Fixer，串行执行。**

```bash
env RUSTC_WRAPPER= CARGO_TARGET_DIR="$TARGET_NO_SCCACHE" \
  cargo check --locked

env CARGO_TARGET_DIR="$TARGET_SHARED_PORT" \
  cargo check --locked
```

若禁用 sccache 仍失败，优先检查 Cargo/build-script、路径删除和并发，而不是 sccache cache
恢复。若只有保留 sccache 才在 cache hit 恢复时失败，再继续 namespace 和冷/热实验。

#### A/B 2：唯一共享 target 下的第一次/第二次编译

**执行位置：同一个 sandbox/Fixer，严格串行。** 第一目录用于填充或验证 cache，第二个全新
目录用于提高发生 cache hit 的机会；不要通过删除第一个目录制造“冷”环境。

```bash
$RUSTC_WRAPPER --show-stats
env CARGO_TARGET_DIR="$TARGET_CACHE_FIRST" cargo check --locked
$RUSTC_WRAPPER --show-stats
env CARGO_TARGET_DIR="$TARGET_CACHE_SECOND" cargo check --locked
$RUSTC_WRAPPER --show-stats
```

全局 cache 可能本来就是热的，因此以 `--show-stats` 的 hit/miss 变化为准，不要只按“第一
次/第二次”命名推断。记录失败发生在 miss 编译、cache 写入还是 hit 恢复。

#### A/B 3：两个不同 sandbox 对同一共享路径的可见性

**执行位置：两个不同 sandbox/Fixer。不要让两边同时编译同一 target。**

每边只做只读检查，分别替换为对方已创建的 target：

```bash
SHARED_TARGET="/mnt/nvme_rust/rust-targets/replace-with-existing-a-b-target"
printf 'sandbox cwd=%s mnt=%s\n' "$PWD" "$(readlink /proc/$$/ns/mnt)"
stat -- "$SHARED_TARGET"
findmnt -T "$SHARED_TARGET" 2>/dev/null || grep '/mnt/nvme_rust/rust-targets' /proc/self/mountinfo
```

这能证明两个 sandbox 的路径映射，但 daemon 仍需按第 3 步单独检查。

#### A/B 4：共享端口与独立端口（仅实验，不是首选配置）

独立端口实验会启动额外 server，风险高于前述 A/B。先用宿主机 `ss` 确认端口无人使用，
确认真实二进制不是固定 4227 的 wrapper，并确保实验结束时只停止自己启动的独立 server。
不要为了该实验停止 4227 上的共享 server。

**执行位置：计划运行实验 Cargo 的 sandbox/Fixer；宿主机先确认端口。**

```bash
TEST_PORT="replace-with-approved-unused-port"
SCCACHE_BIN="replace-with-real-sccache-binary-not-fixed-port-wrapper"
TEST_TARGET="${BASE}-independent-port"
ss -ltn | grep -E ":${TEST_PORT}[[:space:]]" || true
[ ! -e "$TEST_TARGET" ] || { echo "REFUSE: $TEST_TARGET exists" >&2; exit 1; }
```

确认无监听者后，才进入本文后面的“可选破坏性步骤”启动独立 server，再执行：

```bash
SCCACHE_SERVER_PORT="$TEST_PORT" "$SCCACHE_BIN" --show-stats
RUSTC_WRAPPER="$SCCACHE_BIN" \
SCCACHE_SERVER_PORT="$TEST_PORT" \
CARGO_TARGET_DIR="$TEST_TARGET" \
cargo check --locked
SCCACHE_SERVER_PORT="$TEST_PORT" "$SCCACHE_BIN" --show-stats
```

如果 wrapper 脚本无条件写死 4227，设置外部 `SCCACHE_SERVER_PORT` 不会形成有效 A/B；这是
必须直接使用真实 binary 的原因。独立端口成功也不等于“端口冲突”就是根因：新 daemon
可能同时改变了启动 namespace、server 状态和 cache 热度，需要结合 PID mount namespace
解释。

### 8. 可选破坏性步骤：stop/start server

> **默认不要执行。** stop/start 可能中断共享用户、其他 fixer 和 CI 任务。先在宿主机用
> `ss -ltnp`、`pgrep -af sccache`、进程用户、启动时间和 wrapper 配置确认 PID/端口归属，
> 并获得所有共享使用者同意。尤其不要直接停止 4227。

对于经批准的、确认无人使用的**独立实验端口**，命令形式如下：

```bash
TEST_PORT="replace-with-approved-unused-port"
SCCACHE_BIN="replace-with-real-sccache-binary-not-fixed-port-wrapper"
SCCACHE_SERVER_PORT="$TEST_PORT" "$SCCACHE_BIN" --start-server
ss -ltnp | grep "$TEST_PORT"
# 在这里串行执行独立端口 A/B。
SCCACHE_SERVER_PORT="$TEST_PORT" "$SCCACHE_BIN" --stop-server
```

停止共享 4227 的命令不在常规流程中提供，以避免误操作。如确需重启，应由确认其归属的
宿主机管理员在维护窗口执行，并在前后记录 PID、端口、server 版本、mount namespace 和
stats。不得用 `pkill sccache` 或 `killall sccache`。

## 诊断决策表

| 候选情况 | 支持/排除证据 | 下一步动作 |
| --- | --- | --- |
| 外部 daemon，且 output 路径可见 | 宿主机确认 4227 PID；在该 PID mount namespace 内对共享 target/output `stat` 成功；daemon 用户权限正确 | 排除“sandbox daemon 看不到路径”；转查同 target 并发、目录删除、build-script 和 sccache 恢复行为 |
| daemon 在某个 sandbox namespace，其他 worktree 路径不可见 | daemon 的 mount namespace 与启动 sandbox 一致；在 daemon namespace 内 `stat` 失败，但相应 fixer 内成功；改用共同挂载的唯一 target 后成功 | 保持 target 在所有 namespace 可见的共享挂载；再用共享 target 复现确认。不要在无 PID/ns 证据时宣称此项成立 |
| 同一 target 并发 | 同时存在多套 Cargo/rustc，环境或 cwd 指向同一 target；出现 build directory lock；串行后不再失败 | 每任务唯一 target，同一 target 串行 gate；查停并发启动源 |
| target/output 被 clean、rm 或替换 | 失败前后父目录/文件 inode 改变或目录消失；有 `cargo clean`、清理脚本或另一个任务操作同一路径 | 停止清理源；保留失败现场；新建唯一 target 重试，不要删除旧目录来验证 |
| 路径存在但权限/挂载不一致 | `namei` 某级无 execute，`test -w` 失败，`findmnt` 显示只读或映射不同；daemon 身份下复现 | 修正 bind/mount 和最小必要权限；不要用全局 `chmod 777` |
| sccache cache hit 恢复 bug | 无并发/删除；daemon 确认可见且可写；禁用 wrapper 成功；miss 成功而可确认的 hit 稳定触发相同 `.tmp*` ENOENT | 保存版本、debug 日志、stats 和最小复现；在不动共享 cache 的前提下测试已批准版本；向上游报告脱敏证据 |
| 非 sccache 的 Cargo/build-script 问题 | `RUSTC_WRAPPER=` 仍在 build-script `out` 失败；日志显示脚本自己删除/假定路径 | 缩小到对应 package/build script，检查其并发与目录生命周期 |
| Git worktree common metadata 不可见 | Git 报 `.git` 指向路径不可访问；Cargo output 探针和 sccache 路径问题可独立复现/消失 | 单独修复 common metadata bind；不要把它当作 `.tmp*` ENOENT 的证据 |

## 当前推荐的稳定配置

1. 为每个 fixer/任务设置唯一的、所有相关 namespace 都能看到的 target：

   ```bash
   export CARGO_TARGET_DIR="/mnt/nvme_rust/rust-targets/$TASK_ID"
   ```

2. 同一个 `$CARGO_TARGET_DIR` 内串行运行 Cargo gate；不同任务不得复用同一个 `<task>` 名。
3. 可以先保留 sccache 和共享 4227，观察唯一 target 是否持续稳定。
4. 若仍出现相同 ENOENT，在另一个全新唯一 target 上用 `RUSTC_WRAPPER=` 做 A/B。
5. 独立端口只用于区分 server 实例/namespace 的受控实验，不是默认方案，也不是首选修复。
6. 不清空共享 cache，不删除其他任务 target，不以重启共享 server 作为第一反应。

## 尚未证明的事项

目前尚未证明具体根因。特别是，**不能宣称 daemon 是在某个 sandbox namespace 内启动，
所以看不到其他 worktree**。用户指出的“sccache 本来就是外部服务器”完全可能成立；若
daemon 由宿主机预启动，且 daemon PID 的 mount namespace 能访问全部相关 host paths，
该假设应明确排除。

要从“候选原因”升级为结论，至少需要：

- 确认 4227 的实际 daemon PID、用户、版本、启动时间；
- 比较 daemon PID、宿主 shell、两个 sandbox shell 的 PID/mount namespace；
- 在 daemon mount namespace 内实际 `stat`/`namei` 目标 output 路径；
- 在无同 target 并发、无 clean/rm 的条件下，完成保留/禁用 sccache 与冷/热 hit A/B；
- 将 stats、debug 日志和路径生命周期按时间对齐。

## 最小复现脚本

下面脚本不删除任何目录、不停止 server、不并发 Cargo。它拒绝使用已存在的 target，依次
运行“无 sccache”“共享 sccache 第一次”“共享 sccache 第二次”三个实验，并采集 stats。
在仓库根目录、发生错误的 sandbox/Fixer 中保存为临时脚本后执行。它会创建三个新的
`/mnt/nvme_rust/rust-targets/...` 目录并保留现场。

```bash
#!/usr/bin/env bash
set -u

TASK_ID="replace-with-unique-task-name"
RUN_ID="$(date +%Y%m%d-%H%M%S)-$$"
ROOT="/mnt/nvme_rust/rust-targets"
NO_CACHE="$ROOT/${TASK_ID}-${RUN_ID}-no-sccache"
CACHE_1="$ROOT/${TASK_ID}-${RUN_ID}-cache-first"
CACHE_2="$ROOT/${TASK_ID}-${RUN_ID}-cache-second"
WRAPPER="${RUSTC_WRAPPER:-/home/discord9/.local/bin/sccache-pr2678}"
LOG_DIR="${TMPDIR:-/tmp}/sccache-repro-${TASK_ID}-${RUN_ID}"

umask 077
mkdir -p -- "$LOG_DIR"

for dir in "$NO_CACHE" "$CACHE_1" "$CACHE_2"; do
  if [ -e "$dir" ]; then
    printf 'REFUSE: target already exists: %s\n' "$dir" >&2
    exit 2
  fi
done

{
  echo '=== environment ==='
  env | grep -E '^(RUSTC_WRAPPER|CARGO_TARGET_DIR|CARGO_HOME|RUSTUP_HOME|SCCACHE_)=' | sort
  echo '=== wrapper ==='
  readlink -f "$WRAPPER"
  "$WRAPPER" --version
  echo '=== namespace ==='
  readlink /proc/$$/ns/pid
  readlink /proc/$$/ns/mnt
  echo '=== processes ==='
  pgrep -af 'cargo|rustc|sccache' || true
} >"$LOG_DIR/context.log" 2>&1

printf '1/3 no sccache: %s\n' "$NO_CACHE"
env RUSTC_WRAPPER= CARGO_TARGET_DIR="$NO_CACHE" \
  cargo check --locked >"$LOG_DIR/no-sccache.stdout" \
  2>"$LOG_DIR/no-sccache.stderr"
NO_CACHE_RC=$?

"$WRAPPER" --show-stats >"$LOG_DIR/stats-before.txt" 2>&1 || true

printf '2/3 sccache first: %s\n' "$CACHE_1"
env SCCACHE_LOG=debug \
  SCCACHE_ERROR_LOG="$LOG_DIR/sccache-error-first.log" \
  RUSTC_WRAPPER="$WRAPPER" CARGO_TARGET_DIR="$CACHE_1" \
  cargo check --locked >"$LOG_DIR/cache-first.stdout" \
  2>"$LOG_DIR/cache-first.stderr"
CACHE_1_RC=$?
"$WRAPPER" --show-stats >"$LOG_DIR/stats-after-first.txt" 2>&1 || true

printf '3/3 sccache second: %s\n' "$CACHE_2"
env SCCACHE_LOG=debug \
  SCCACHE_ERROR_LOG="$LOG_DIR/sccache-error-second.log" \
  RUSTC_WRAPPER="$WRAPPER" CARGO_TARGET_DIR="$CACHE_2" \
  cargo check --locked >"$LOG_DIR/cache-second.stdout" \
  2>"$LOG_DIR/cache-second.stderr"
CACHE_2_RC=$?
"$WRAPPER" --show-stats >"$LOG_DIR/stats-after-second.txt" 2>&1 || true

printf 'result: no-sccache=%s cache-first=%s cache-second=%s\n' \
  "$NO_CACHE_RC" "$CACHE_1_RC" "$CACHE_2_RC"
printf 'logs: %s\n' "$LOG_DIR"
printf 'targets retained:\n  %s\n  %s\n  %s\n' \
  "$NO_CACHE" "$CACHE_1" "$CACHE_2"
```

脚本故意不自动清理。分享日志前按前述要求脱敏。若当前已有其他 Cargo 指向这三个目录中的
任一个（正常情况下不应发生），立即停止实验并换新的 `TASK_ID`/`RUN_ID`，不要删除目录。

## 采集结果模板

复制以下模板，粘贴脱敏后的输出。路径可将用户名/仓库名替换为一致的占位符，但保留挂载
边界、target 是否相同、namespace inode 和错误末级文件名。

```text
### 基本信息
- 时间/时区：
- e-agent/Fixer 任务标识：
- worktree（脱敏）：
- OS/kernel：
- rustc/cargo 版本：
- sccache 版本：

### Wrapper 与环境
- RUSTC_WRAPPER：
- readlink -f wrapper：
- wrapper 是否脚本、是否固定 SCCACHE_SERVER_PORT=4227：
- CARGO_TARGET_DIR：
- CARGO_HOME / RUSTUP_HOME：
- SCCACHE_*（秘密值已删除）：
- type -a sccache：

### Server 与 namespace
- 4227 listener/PID/用户/启动时间：
- daemon 命令行（脱敏）：
- daemon PID namespace：
- daemon mount namespace：
- 宿主 shell PID/mount namespace：
- sandbox A PID/mount namespace：
- sandbox B PID/mount namespace：
- daemon namespace 内 stat target：成功 / 失败 / 无权限检查
- daemon namespace 内 namei/findmnt 摘要：

### 路径与挂载
- cache stat/namei/findmnt/test -w：
- target stat/namei/findmnt/test -w：
- 实际失败 output dir stat/namei/findmnt/test -w：
- create + rename 探针：成功 / 失败 / 未执行（原因）
- 失败路径原文（脱敏）：
- 失败前后目录是否消失或 inode 改变：

### 并发与目录生命周期
- 当时是否有多个 Cargo/rustc：是 / 否 / 未知
- 是否共用同一 target：是 / 否 / 未知
- 是否出现 target lock 提示：
- 是否有 cargo clean、rm 或清理脚本：是 / 否 / 未知

### A/B 结果
- 唯一共享 target + RUSTC_WRAPPER=：
- 唯一共享 target + 共享端口 sccache：
- 第一次/第二次 stats hit/miss 变化：
- 错误只在可确认的 cache hit 出现：是 / 否 / 未知
- 独立端口实验：未执行 / 结果（端口、PID/ns、版本）
- 两个 sandbox 对共享 target 的 stat/findmnt：

### 日志
- SCCACHE_LOG/SCCACHE_ERROR_LOG 是否产生日志：
- 最小错误摘要：
- 日志是否已脱敏：是 / 否

### 当前结论等级
- 已排除：
- 仍支持：
- 尚未证明：
- 下一项最低风险验证：
```
