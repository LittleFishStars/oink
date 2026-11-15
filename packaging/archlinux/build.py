#!/usr/bin/env python3
"""Assemble an Arch Linux package tree packed as a .tar.gz (pacman-compatible
"bare" archive for manual extraction, not a .pkg.tar.zst).

Layout:
  oink-linux-x64/usr/bin/{oink,oink-agent,oink-crash-collector}
  oink-linux-x64/usr/share/applications/oink.desktop
  oink-linux-x64/usr/share/icons/hicolor/...
  oink-linux-x64/usr/lib/systemd/user/oink.service
  oink-linux-x64/usr/share/doc/oink/README-linux.md
Intended for servers/headless hosts and environments without a package manager
frontend; desktop users should prefer the AppImage or the .deb.
"""
import argparse
import hashlib
import io
import os
import tarfile
import time
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'linux'))
from common import BINARIES, DESCRIPTION, HOMEPAGE, LICENSE, UNIX_NAME, VERSION, build_record, icon_png, write_desktop

SYSTEMD_UNIT = f"""[Unit]
Description=Oink (software KVM)
After=graphical-session.target
PartOf=graphical-session.target

[Service]
ExecStart=/usr/bin/oink-agent run
Restart=on-failure

[Install]
WantedBy=graphical-session.target
"""

README = f"""# Oink {VERSION} (Linux x64, tar.gz)

{DESCRIPTION}

## Contents

- `/usr/bin/oink` — UI (Tauri, frontend embedded)
- `/usr/bin/oink-agent` — background service (networking, input, clipboard)
- `/usr/bin/oink-crash-collector` — crash collector
- `/usr/lib/systemd/user/oink.service` — systemd user unit for the agent

## Run

Desktop session required (Wayland; niri/Hyprland/Sway and other wlr-session
compositors are supported). Source capture additionally requires membership in
the `input` group.

    systemctl --user enable --now oink
    oink-agent status

License: {LICENSE}. Upstream: {HOMEPAGE}
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binaries', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--icons', type=Path, default=Path(__file__).resolve().parents[2] / 'apps/oink/src-tauri/icons')
    parser.add_argument('--build', type=int, default=1)
    parser.add_argument('--source-tree-sha256', required=True)
    parser.add_argument('--source-commit')
    parser.add_argument('--update-notes-file', type=Path)
    args = parser.parse_args()

    for name in BINARIES:
        if not (args.binaries / name).is_file():
            raise SystemExit(f'missing binary: {args.binaries / name}')

    root = Path(f'{UNIX_NAME}-linux-x64')
    staging = args.output / root
    if staging.exists():
        import shutil
        shutil.rmtree(staging)
    (staging / 'usr/bin').mkdir(parents=True)
    (staging / 'usr/share/applications').mkdir(parents=True)
    (staging / 'usr/share/doc' / UNIX_NAME).mkdir(parents=True)
    (staging / 'usr/lib/systemd/user').mkdir(parents=True)

    for name in BINARIES:
        target = staging / 'usr/bin' / name
        target.write_bytes((args.binaries / name).read_bytes())
        target.chmod(0o755)
    write_desktop(staging / 'usr/share/applications')
    for size in (32, 128, 256):
        directory = staging / 'usr/share/icons/hicolor' / f'{size}x{size}' / 'apps'
        directory.mkdir(parents=True)
        (directory / f'{UNIX_NAME}.png').write_bytes(icon_png(args.icons, size))
    (staging / 'usr/lib/systemd/user' / 'oink.service').write_text(SYSTEMD_UNIT)
    (staging / 'usr/share/doc' / UNIX_NAME / 'README-linux.md').write_text(README)

    destination = args.output / f'{UNIX_NAME}-{VERSION}-linux-x64.tar.gz'
    # 固定 mtime 保证可复现；文件按排序写入。
    normalization = int(time.mktime(time.strptime('2026-01-01', '%Y-%m-%d')))
    with tarfile.open(destination, 'w:gz') as archive:
        for path in sorted(staging.rglob('*')):
            info = archive.gettarinfo(str(path), arcname=str(path.relative_to(args.output)))
            info.mtime = normalization
            info.uid = info.gid = 0
            info.uname = info.gname = 'root'
            if path.is_file() and os.access(path, os.X_OK):
                info.mode = 0o755
            elif path.is_file():
                info.mode = 0o644
            else:
                info.mode = 0o755
            if path.is_file():
                archive.addfile(info, io.BytesIO(path.read_bytes()))
            else:
                archive.addfile(info)

    digest = hashlib.sha256(destination.read_bytes()).hexdigest()
    (args.output / 'SHA256SUMS').write_text(f'{digest}  {destination.name}\n')
    build_record(args.binaries, args.output, args.build, args.source_commit,
                 {'format': 'tar-gz', 'archive': destination.name, 'sha256': digest})
    print(f'tar.gz complete: {destination}')


if __name__ == '__main__':
    main()
