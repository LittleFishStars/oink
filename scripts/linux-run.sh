#!/usr/bin/env bash
# Oink Linux 开发/运行启动器：先起后台服务，等 IPC 就绪，再起图形界面。
#
# 用法：
#   scripts/linux-run.sh            # debug 构建
#   scripts/linux-run.sh release    # release 构建
#   OINK_HOME=~/test-home scripts/linux-run.sh   # 隔离数据目录测试
#
# 前提：前端已构建（apps/oink/dist 存在；没有就先跑 `pnpm build`）。
# Ctrl-C 退出 UI 后，后台仍在运行：用 `oink-agent stop` 或 pkill -f 'oink-agent run' 收尾。
set -euo pipefail

cd "$(dirname "$0")/.."
PROFILE=debug
if [ "${1:-}" = release ]; then
  PROFILE=release
fi
BIN="target/$PROFILE"

[ -x "$BIN/oink-agent" ] || { echo "缺 $BIN/oink-agent，先: cargo build [-r] -p oink-agent"; exit 1; }
[ -x "$BIN/oink" ] || { echo "缺 $BIN/oink，先: pnpm build && cargo build [-r] -p oink-ui"; exit 1; }

DATA_DIR="${OINK_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/oink}"
LOG="${OINK_LOG:-$DATA_DIR/agent.log}"
mkdir -p "$(dirname "$LOG")"

# 后台已在运行就不重复起（ipc.json 即监听就绪信号）。
if [ -f "$DATA_DIR/ipc.json" ] && "$BIN/oink-agent" status >/dev/null 2>&1; then
  echo "后台已在运行（$DATA_DIR）"
else
  nohup "$BIN/oink-agent" run >> "$LOG" 2>&1 &
  AGENT_PID=$!
  echo "后台启动 pid=$AGENT_PID，日志 $LOG"
fi

# 等握手就绪（最多 20s）：用 status 探测，它走完整 IPC 链路。
for _ in $(seq 1 100); do
  if "$BIN/oink-agent" status >/dev/null 2>&1; then
    echo "后台就绪，启动界面…"
    break
  fi
  sleep 0.2
done

# UI 前台运行（Ctrl-C 退出界面；后台继续常驻，重开界面无需重启后台）。
exec "$BIN/oink"
