#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""实时记录本机物理鼠标的 evdev 事件流（独立于 Oink 的对照证据）。

用途：先启动本脚本，再启动/操作 Oink。对照两条时间线即可判断：
  - 脚本看到位移    = evdev 层事件在流动（内核/权限侧正常）
  - 同期 oink 日志（target=oink_gap）没有对应记录 = 断点在 Oink 内部
  - 脚本也看不到    = 设备/权限/独占问题（与 Oink 无关）

用法：python3 scripts/mouse-trace.py     （Ctrl+C 结束，输出汇总）
"""
import os
import struct
import sys
import time

EV_REL = 0x02
REL_X, REL_Y = 0x00, 0x01
# x86_64: timeval = 2×int64 = 16B；type/code 各 u16；value s32 → 共 24B
FMT = 'qqHHi'
SIZE = struct.calcsize(FMT)


def find_mice():
    """从 /proc/bus/input/devices 找鼠标（Handlers 含 mouseN）。"""
    mice, cur = [], {}
    with open('/proc/bus/input/devices') as f:
        for line in f:
            line = line.rstrip()
            if line.startswith('N: Name='):
                cur['name'] = line.split('=', 1)[1].strip()
            elif line.startswith('H: Handlers='):
                handlers = line.split('=', 1)[1].split()
                events = [h for h in handlers if h.startswith('event')]
                if 'mouse' in handlers and events:
                    mice.append((f'/dev/input/{events[0]}', cur.get('name', '?')))
    return mice


def main():
    mice = find_mice()
    if not mice:
        sys.exit('未找到鼠标设备（/proc/bus/input/devices 里无 mouseN handler）')
    print(f'发现 {len(mice)} 个鼠标设备：')
    for path, name in mice:
        print(f'  {path}  {name}')
    print('开始记录。动本机鼠标即有输出；Ctrl+C 结束。\n')

    fds = []
    for path, _ in mice:
        fd = os.open(path, os.O_RDONLY)
        os.set_blocking(fd, False)
        fds.append(fd)

    x = y = 0.0
    events = 0
    started = time.time()
    last = 0.0
    try:
        while True:
            got = False
            for fd in fds:
                try:
                    data = os.read(fd, SIZE * 128)
                except BlockingIOError:
                    continue
                got = got or bool(data)
                for off in range(0, len(data) - SIZE + 1, SIZE):
                    _s, _us, etype, code, value = struct.unpack_from(FMT, data, off)
                    if etype == EV_REL:
                        if code == REL_X:
                            x += value
                        elif code == REL_Y:
                            y += value
                        events += 1
            now = time.time() - started
            if got and now - last >= 0.2:
                print(f'{now:7.1f}s  累计位移 ({x:+7.0f}, {y:+7.0f})   事件数 {events}')
                last = now
            time.sleep(0.002)
    except KeyboardInterrupt:
        pass
    print(f'\n汇总：{time.time() - started:.1f}s 内 {events} 个相对位移事件，'
          f'累计位移 ({x:+.0f}, {y:+.0f})')


if __name__ == '__main__':
    main()
