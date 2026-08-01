#!/usr/bin/env bash
# =============================================================================
# scripts/backup.sh — e-agent 会话数据库备份（GreptimeDB parquet 导出）
#
# 通过 GreptimeDB 的 pg-wire（默认 127.0.0.1:15403）用 COPY DATABASE 把
# 每个库导出为 parquet 文件（session_entries / sessions / running_tasks）。
# 恢复方法见文件底部注释。
#
# 用法: scripts/backup.sh [--port PORT] [--out DIR] [--keep N]
#   --port    GreptimeDB pg-wire 端口（默认 15403）
#   --out     备份根目录（默认 ~/eagent-backup）
#   --keep    保留最近 N 份备份（默认 10，0 = 不清理）
#   --db      只备份指定库（可多次传；默认备份所有已知库 e_agent e_agent_win）
#
# 输出结构:
#   <out>/
#   ├── <库名>-<YYYYmmdd-HHMMSS>/     # 一次备份一份带时间戳的目录
#   │   ├── session_entries.parquet
#   │   ├── sessions.parquet
#   │   └── running_tasks.parquet
#   └── latest -> <最新一份>          # 软链，便于固定路径引用
#
# 恢复（示例，目标库需存在或先建）:
#   psql "host=127.0.0.1 port=15403 dbname=e_agent" \
#     -c "COPY DATABASE e_agent FROM '<out>/e_agent-<ts>/' WITH (FORMAT='parquet');"
# =============================================================================
set -u

PORT=15403
OUT="${HOME}/eagent-backup"
KEEP=10
DBS=(e_agent e_agent_win)

while [ $# -gt 0 ]; do
  case "$1" in
    --port) PORT="${2:-}"; shift 2 ;;
    --out)  OUT="${2:-}"; shift 2 ;;
    --keep) KEEP="${2:-}"; shift 2 ;;
    --db)   DBS+=("${2:-}"); shift 2 ;;
    *) echo "[ERR] 未知参数: $1（用法: $0 [--port PORT] [--out DIR] [--keep N] [--db NAME]）" >&2; exit 2 ;;
  esac
done

case "$PORT" in
  ''|*[!0-9]*) echo "[ERR] 非法端口: $PORT" >&2; exit 2 ;;
esac

CONN="host=127.0.0.1 port=${PORT} dbname=e_agent"

# 检查 psql 可用 + GreptimeDB 可达
if ! command -v psql >/dev/null 2>&1; then
  echo "[ERR] psql 不可用" >&2; exit 1
fi
if ! psql "$CONN" -c "SELECT 1" >/dev/null 2>&1; then
  echo "[ERR] 无法连接 GreptimeDB（127.0.0.1:${PORT}）—— 数据库没起？" >&2; exit 1
fi

mkdir -p "$OUT"
FAIL=0
for db in "${DBS[@]}"; do
  TS="$(date +%Y%m%d-%H%M%S)"
  DIR="${OUT}/${db}-${TS}"
  # COPY DATABASE 要求目标目录不存在或为空（Greptime 会自己建）
  if [ -e "$DIR" ]; then
    echo "[SKIP] $db: $DIR 已存在（同秒重跑？）"
    continue
  fi
  echo "[BACKUP] $db -> $DIR"
  if psql "$CONN" -c "COPY DATABASE ${db} TO '${DIR}/' WITH (FORMAT='parquet');" \
     2>&1 | grep -qE "^OK "; then
    echo "[OK] $db 备份完成"
    # 更新 latest 软链（多库时 latest 指向最后成功的库——用库名区分）
    ln -sfn "${db}-${TS}" "${OUT}/latest-${db}"
  else
    echo "[FAIL] $db 备份失败"
    FAIL=1
  fi
done

# 清理旧备份（按目录名排序保留最近 KEEP 份/库）
if [ "$KEEP" -gt 0 ]; then
  for db in "${DBS[@]}"; do
    # 只清理带时间戳的备份目录，不碰 latest 软链
    mapfile -t old < <(ls -1d "${OUT}/${db}-"* 2>/dev/null | grep -v -- "-latest" | sort -r | tail -n +$((KEEP + 1)))
    for d in "${old[@]}"; do
      echo "[CLEAN] 删除旧备份 $d"
      rm -rf "$d"
    done
  done
fi

if [ "$FAIL" -eq 0 ]; then
  echo ""
  echo "备份完成: $OUT"
  ls -la "${OUT}"/latest-* 2>/dev/null | awk '{print "  " $9 " -> " $11}'
  exit 0
else
  echo "[ERR] 有库备份失败" >&2
  exit 1
fi
