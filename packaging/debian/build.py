#!/usr/bin/env python3
"""Assemble a Debian .deb (x86_64) from the compiled release binaries.

Uses dpkg-deb when available (falling back to a manual ar+tar data/control
build is deliberately not implemented: packaging hosts are expected to have
dpkg, matching upstream's pinned-tool policy).

Layout mirrors the tar.gz package plus Debian metadata:
  DEBIAN/control | conffiles | postinst | prerm
  usr/bin/... usr/share/... usr/lib/systemd/user/oink.service
"""
import argparse
import hashlib
import subprocess
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'linux'))
from common import BINARIES, DESCRIPTION, HOMEPAGE, LICENSE, UNIX_NAME, VERSION, build_record, icon_png, write_desktop

# 与 tar.gz 相同的 unit 与 README 内容在安装后由 postinst 启用说明补充。
SYSTEMD_UNIT = """[Unit]
Description=Oink (software KVM)
After=graphical-session.target
PartOf=graphical-session.target

[Service]
ExecStart=/usr/bin/oink-agent run
Restart=on-failure

[Install]
WantedBy=graphical-session.target
"""

# WebKitGTK 运行时是 Tauri UI 的硬依赖；libayatana-appindicator3-1 提供托盘。
DEPENDS = 'libc6 (>= 2.31), libwebkit2gtk-4.1-0, libgtk-3-0, libayatana-appindicator3-1, libssl3'

CONTROL = f"""Package: {UNIX_NAME}
Version: {VERSION}
Section: utils
Priority: optional
Architecture: amd64
Depends: {DEPENDS}
Homepage: {HOMEPAGE}
License: {LICENSE}
Installed-Size: __INSTALLED_SIZE__
Maintainer: Oink contributors <noreply@example.invalid>
Description: {DESCRIPTION}
 Peer-to-peer software KVM: use any computer's keyboard and mouse to control
 the others, with clipboard sharing and encrypted local networking.
 desktop file and icons; the background agent is oink-agent (systemd user
 service unit included; enable with `systemctl --user enable --now oink`).
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

    staging = args.output / 'deb-staging'
    if staging.exists():
        import shutil
        shutil.rmtree(staging)
    debian = staging / 'DEBIAN'
    (staging / 'usr/bin').mkdir(parents=True)
    (staging / 'usr/share/applications').mkdir(parents=True)
    (staging / 'usr/share/doc' / UNIX_NAME).mkdir(parents=True)
    (staging / 'usr/lib/systemd/user').mkdir(parents=True)
    debian.mkdir()

    total_kb = 0
    for name in BINARIES:
        source = args.binaries / name
        target = staging / 'usr/bin' / name
        target.write_bytes(source.read_bytes())
        target.chmod(0o755)
        total_kb += len(source.read_bytes()) // 1024
    write_desktop(staging / 'usr/share/applications')
    for size in (32, 128, 256):
        directory = staging / 'usr/share/icons/hicolor' / f'{size}x{size}' / 'apps'
        directory.mkdir(parents=True)
        (directory / f'{UNIX_NAME}.png').write_bytes(icon_png(args.icons, size))
    (staging / 'usr/lib/systemd/user' / 'oink.service').write_text(SYSTEMD_UNIT)
    copyright = staging / 'usr/share/doc' / UNIX_NAME / 'copyright'
    copyright.write_text(f'Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/\n'
                         f'Upstream-Name: Oink\nSource: {HOMEPAGE}\n\n'
                         f'Files: *\nCopyright: Oink contributors\nLicense: {LICENSE}\n')
    # conffiles：无（配置在用户数据目录，非 /etc）

    control = debian / 'control'
    control.write_text(CONTROL.replace('__INSTALLED_SIZE__', str(max(total_kb, 1))))
    (debian / 'postinst').write_text(
        '#!/bin/sh\n'
        'set -e\n'
        '# 图标缓存刷新失败不阻塞安装。\n'
        'if command -v update-desktop-database >/dev/null 2>&1; then\n'
        '    update-desktop-database /usr/share/applications || true\n'
        'fi\n'
        'if command -v gtk-update-icon-cache >/dev/null 2>&1; then\n'
        '    gtk-update-icon-cache -f /usr/share/icons/hicolor || true\n'
        'fi\n'
        f'echo "{UNIX_NAME} {VERSION} installed. Start with: systemctl --user enable --now oink"\n'
    )
    (debian / 'prerm').write_text(
        '#!/bin/sh\n'
        'set -e\n'
        '# 升级/卸载前停掉用户服务；失败不阻塞（可能本来就没启用）。\n'
        'if [ "$1" = remove ] || [ "$1" = upgrade ]; then\n'
        '    systemctl --user stop oink 2>/dev/null || true\n'
        'fi\n'
    )
    for script in ('postinst', 'prerm'):
        (debian / script).chmod(0o755)

    destination = args.output / f'{UNIX_NAME}_{VERSION}_amd64.deb'
    subprocess.run(['dpkg-deb', '--root-owner-group', '--build', str(staging), str(destination)], check=True)

    digest = hashlib.sha256(destination.read_bytes()).hexdigest()
    (args.output / 'SHA256SUMS').write_text(f'{digest}  {destination.name}\n')
    build_record(args.binaries, args.output, args.build, args.source_commit,
                 {'format': 'deb', 'deb': destination.name, 'sha256': digest})
    print(f'.deb complete: {destination}')


if __name__ == '__main__':
    main()
