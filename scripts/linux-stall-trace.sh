#!/usr/bin/env bash
# 启动 Oink 后运行：采集「托盘不显示」与「应用卡」的运行时证据。
# 用法：先 ./target/release/oink 起来并从 Windows 控制本机（复现卡顿），再跑本脚本。
set -u
echo "════════ 1. Oink 进程 ════════"
pgrep -af 'target/(release|debug)/oink' | grep -v grep || echo "  （Oink 未运行——请先启动再采集）"

echo "════════ 2. SNI 条目：Oink 是否注册进 noctalia ════════"
ITEMS=$(busctl --user get-property org.kde.StatusNotifierWatcher /StatusNotifierWatcher \
  org.kde.StatusNotifierWatcher RegisteredStatusNotifierItems 2>&1)
echo "$ITEMS" | tr ',' '\n' | sed 's/^/  /'
if echo "$ITEMS" | grep -qi 'oink'; then
  echo "  ✓ Oink 条目已在总线——若 bar 上仍看不到，是 noctalia 渲染/过滤问题（非 Oink）"
else
  echo "  ✗ Oink 条目不在总线——libayatana 注册未生效（Oink 侧问题）"
fi

echo "════════ 3. CPU 占用（卡顿时的元凶）════════"
for p in $(pgrep -f 'target/(release|debug)/oink'); do
  ps -o pid,pcpu,pmem,nlwp,etime,comm -p "$p" --no-headers 2>/dev/null | \
    awk '{printf "  PID %-8s CPU %5s%% MEM %5s%% 线程 %-4s 运行 %s %s\n",$1,$2,$3,$4,$5,$6}'
done

echo "════════ 4. 最忙的线程（top -H 采样 3 秒）════════"
top -H -b -n 2 -d 1.5 2>/dev/null | awk '
  /PID|oink/{print}
' | grep -i oink | head -12 | sed 's/^/  /'

echo "════════ 5. 卡顿时的调用栈（若装了 gdb，抓 oink-agent 主线程）════════"
AGENT=$(pgrep -f 'oink-agent run' | head -1)
if [ -n "${AGENT:-}" ] && command -v gdb >/dev/null 2>&1; then
  timeout 8 gdb -p "$AGENT" -batch -ex 'thread apply all bt' 2>/dev/null | \
    grep -A6 -iE 'niri|wayland|inject|outputs|fork|Command|wait' | head -30 | sed 's/^/  /'
else
  echo "  （跳过：需 sudo gdb -p $AGENT 或安装 gdb）"
fi

echo "════════ 完成：把以上输出贴回 ════════"
