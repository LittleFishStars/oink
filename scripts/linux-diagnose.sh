#!/usr/bin/env bash
# Oink Linux 诊断：一次性检查托盘/注入/穿越的全部环境前提。
# 用法：bash scripts/linux-diagnose.sh
set -uo pipefail

echo "════════ 1. 运行中的 Oink 进程 ════════"
pgrep -a oink 2>/dev/null | grep -v grep || echo "  （无）"
echo
echo "════════ 2. 托盘前提 ════════"
echo "  StatusNotifierWatcher（托盘宿主）:"
busctl --user list 2>/dev/null | grep -i 'StatusNotifierWatcher' | head -2 || echo "    ❌ 未找到——waybar/noctalia 需启用 tray 模块"
echo "  waybar tray 模块:"
grep -l 'tray' ~/.config/waybar/config* 2>/dev/null | head -1 && echo "    ✓ waybar 配置含 tray" || echo "    （waybar 未配 tray 或非 waybar）"
grep -rn 'tray' ~/.config/niri/noctalia.kdl 2>/dev/null | head -2 || true
echo "  XDG_CURRENT_DESKTOP: ${XDG_CURRENT_DESKTOP:-（未设置）}"
echo
echo "════════ 3. evdev 源机捕获前提 ════════"
echo "  input 组: $(id -Gn | tr ' ' '\n' | grep -qx input && echo ✓ 在 || echo ❌ 不在)"
ls /dev/input/event* >/dev/null 2>&1 && echo "  /dev/input/event*: ✓ 可见（$(ls /dev/input/event* | wc -l) 个）" || echo "  /dev/input/event*: ❌ 不可见"
echo
echo "════════ 4. 最近一次 agent 日志的关键行 ════════"
LOG=$(ls -t ~/.local/share/oink/agent.log ~/.local/share/oink/Logs/*.log 2>/dev/null | head -1)
if [ -n "${LOG:-}" ]; then
  echo "  （$LOG 的最后 12 行相关内容）"
  grep -E '注入|tray|evdev|backlog|暂停|ERROR|WARN' "$LOG" 2>/dev/null | tail -12 || tail -6 "$LOG"
else
  echo "  未找到 agent.log（OINK_HOME 或 ~/.local/share/oink）"
fi
echo
echo "════════ 5. 屏幕边缘布局（穿越前提：两台设备的屏幕要相邻）════════"
oink-agent layout show 2>/dev/null | head -12 || echo "  （agent 未运行或无布局）"
echo
echo "════════ 完成 ════════"
echo "把以上全部输出贴回给我。"
