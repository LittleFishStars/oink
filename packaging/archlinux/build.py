#!/usr/bin/env python3
"""Assemble a real pacman package (.pkg.tar.zst) for Arch Linux.

Build strategy: stage the PKGBUILD inputs into a scratch PKGBUILDDIR, then run
`makepkg --nodeps --noconfirm` so packaging semantics (depends/provides/
install script/compression) come from makepkg itself instead of a hand-rolled
tar. Requires makepkg + fakeroot (base-devel) on the build host.

The PKGBUILD supports two modes:
- local: OINK_BINARIES points at target/release (this script uses this)
- AUR:   replace source=() with the release tarball and fill sha256sums
"""
import argparse
import hashlib
import os
import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'linux'))
from common import (BINARIES, DESCRIPTION, HOMEPAGE, LICENSE, SYSTEMD_UNIT, UNIX_NAME, VERSION,
                    build_record, icon_png, write_desktop)

HERE = Path(__file__).resolve().parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binaries', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--icons', type=Path, default=Path(__file__).resolve().parents[2] / 'apps/oink/src-tauri/icons')
    parser.add_argument('--license-file', type=Path, default=Path(__file__).resolve().parents[2] / 'LICENSE')
    parser.add_argument('--build', type=int, default=1)
    parser.add_argument('--source-tree-sha256', required=True)
    parser.add_argument('--source-commit')
    parser.add_argument('--update-notes-file', type=Path)
    args = parser.parse_args()

    for name in BINARIES:
        if not (args.binaries / name).is_file():
            raise SystemExit(f'missing binary: {args.binaries / name}')
    for tool in ('makepkg', 'fakeroot'):
        if shutil.which(tool) is None:
            raise SystemExit(f'{tool} 不可用：安装 base-devel（pacman -S --needed base-devel）')

    work = args.output / 'makepkg'
    if work.exists():
        shutil.rmtree(work)
    work.mkdir(parents=True)

    # PKGBUILD 的 source 文件：desktop、unit、三个尺寸图标、许可证
    write_desktop(work)
    (work / f'{UNIX_NAME}.desktop').rename(work / 'oink.desktop')
    (work / 'oink.service').write_text(SYSTEMD_UNIT)
    for size in (32, 128, 256):
        (work / f'icon-{size}.png').write_bytes(icon_png(args.icons, size))
    if args.license_file.is_file():
        shutil.copy2(args.license_file, work / 'LICENSE')
    else:
        (work / 'LICENSE').write_text(f'{DESCRIPTION}\n\nLicense: {LICENSE}\nUpstream: {HOMEPAGE}\n')

    # PKGBUILD 版本号与 identity.json 对齐
    pkgbuild = (HERE / 'PKGBUILD').read_text()
    pkgbuild = pkgbuild.replace('pkgver=0.3.2', f'pkgver={VERSION}')
    (work / 'PKGBUILD').write_text(pkgbuild)
    shutil.copy2(HERE / 'oink.install', work / 'oink.install')

    environment = dict(os.environ,
                       OINK_BINARIES=str(args.binaries.resolve()),
                       PKGDEST=str(args.output.resolve()),
                       SRCDEST=str(work),
                       BUILDDIR=str(work / 'build'),
                       # makepkg 在容器/CI 里拒绝以 root 运行；这里强制普通用户语义
                       PKGEXT='.pkg.tar.zst')
    environment.pop('MAKEFLAGS', None)
    subprocess.run(['makepkg', '--nodeps', '--noconfirm', '--force', '--nocheck'],
                   cwd=work, env=environment, check=True)

    produced = sorted(args.output.glob(f'{UNIX_NAME}-{VERSION}-*.pkg.tar.zst'))
    if not produced:
        raise SystemExit('makepkg 未产出 .pkg.tar.zst')
    destination = produced[-1]

    digest = hashlib.sha256(destination.read_bytes()).hexdigest()
    (args.output / 'SHA256SUMS').write_text(f'{digest}  {destination.name}\n')
    build_record(args.binaries, args.output, args.build, args.source_commit,
                 {'format': 'pacman', 'package': destination.name, 'sha256': digest})
    print(f'pacman package complete: {destination}')


if __name__ == '__main__':
    main()
