#!/usr/bin/env python3
"""Assemble an x86_64 AppImage (type 2) from the compiled release binaries.

Layout inside the squashfs:
  AppDir/usr/bin/{oink,oink-agent,oink-crash-collector}
  AppDir/usr/share/{applications,icons/hicolor/{size}x{size}/apps}
  AppDir/usr/share/metainfo/appstream.xml
No build-time AppImageTool download verification beyond the upstream SHA256;
the tool is fetched from the canonical appimagetool releases.
"""
import argparse
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import urllib.request

sys.path.insert(0, str(Path(__file__).resolve().parent))
from common import BINARIES, IDENTIFIER, UNIX_NAME, VERSION, build_record, icon_png, write_desktop

APP_ENV_UI = 'oink'  # IDENTITY['binaries']['ui']；AppRun 入口

# 上游自 1.9.x 起改为 continuous 滚动发布，资产名固定为 appimagetool-<arch>.AppImage。
APPIMAGETOOL_TAG = 'continuous'
APPIMAGETOOL_URL = (
    'https://github.com/AppImage/appimagetool/releases/download/'
    f'{APPIMAGETOOL_TAG}/appimagetool-x86_64.AppImage'
)


def appstream() -> str:
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<component type="desktop-application">
  <id>{IDENTIFIER}</id>
  <name>Oink</name>
  <summary>Software KVM: share keyboard, mouse and clipboard</summary>
  <metadata_license>CC0-1.0</metadata_license>
  <project_license>GPL-3.0-only</project_license>
  <releases>
    <release version="{VERSION}" date="$(date -u +%F)"/>
  </releases>
</component>
"""


def ensure_tool(cache: Path, explicit: Path | None) -> Path:
    if explicit:
        tool = explicit
    else:
        cache.mkdir(parents=True, exist_ok=True)
        tool = cache / 'appimagetool-x86_64'
        if not tool.exists():
            destination = Path(str(tool) + '.download')
            print(f'downloading appimagetool ({APPIMAGETOOL_TAG}) from {APPIMAGETOOL_URL}')
            urllib.request.urlretrieve(APPIMAGETOOL_URL, destination)
            destination.rename(tool)
    tool.chmod(0o755)
    return tool


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binaries', type=Path, required=True, help='release binaries dir')
    parser.add_argument('--output', type=Path, required=True, help='package output dir')
    parser.add_argument('--icons', type=Path, default=Path(__file__).resolve().parents[2] / 'apps/oink/src-tauri/icons')
    parser.add_argument('--tool-cache', type=Path, default=Path.home() / '.cache/oink-build')
    parser.add_argument('--tool', type=Path, help='existing appimagetool binary')
    parser.add_argument('--build', type=int, default=1)
    parser.add_argument('--source-tree-sha256', required=True)
    parser.add_argument('--source-commit')
    parser.add_argument('--update-notes-file', type=Path)
    args = parser.parse_args()

    for name in BINARIES:
        if not (args.binaries / name).is_file():
            raise SystemExit(f'missing binary: {args.binaries / name}')

    appdir = args.output / 'AppDir'
    if appdir.exists():
        import shutil
        shutil.rmtree(appdir)
    (appdir / 'usr/bin').mkdir(parents=True)
    (appdir / 'usr/share/applications').mkdir(parents=True)
    (appdir / 'usr/share/metainfo').mkdir(parents=True)

    for name in BINARIES:
        target = appdir / 'usr/bin' / name
        target.write_bytes((args.binaries / name).read_bytes())
        target.chmod(0o755)

    write_desktop(appdir / 'usr/share/applications')
    metainfo = appdir / 'usr/share/metainfo' / f'{IDENTIFIER}.metainfo.xml'
    metainfo.write_text(appstream().replace('$(date -u +%F)', __import__('datetime').date.today().isoformat()))
    for size in (32, 128, 256):
        directory = appdir / 'usr/share/icons/hicolor' / f'{size}x{size}' / 'apps'
        directory.mkdir(parents=True)
        (directory / f'{UNIX_NAME}.png').write_bytes(icon_png(args.icons, size))
    # AppImage 规范：AppRun 是可执行入口（AppImage 挂载后直接运行它）。
    apprun = appdir / 'AppRun'
    apprun.write_text(
        '#!/bin/sh\n'
        '# AppImage 入口：按需设置自身目录并移交参数。\n'
        'HERE="$(dirname "$(readlink -f "$0")")"\n'
        f'exec "$HERE/usr/bin/{APP_ENV_UI}" "$@"\n'
    )
    apprun.chmod(0o755)
    # AppImage 规范：.desktop 必须同时位于 AppDir 根目录（appimagetool 据此取
    # Name/Icon/Exec），图标与 .DirIcon 也在根级。
    (appdir / f'{UNIX_NAME}.desktop').write_text((appdir / 'usr/share/applications' / f'{UNIX_NAME}.desktop').read_text())
    (appdir / f'{UNIX_NAME}.png').write_bytes(icon_png(args.icons, 256))
    (appdir / '.DirIcon').write_bytes(icon_png(args.icons, 256))

    tool = ensure_tool(args.tool_cache, args.tool)
    destination = args.output / f'Oink-{VERSION}-x86_64.AppImage'
    # appimagetool 自身是 AppImage：无 FUSE 的环境（容器/CI）需解包运行。
    environment = dict(os.environ, ARCH='x86_64', APPIMAGE_EXTRACT_AND_RUN='1')
    subprocess.run([str(tool), str(appdir), str(destination)], check=True, env=environment)

    digest = hashlib.sha256(destination.read_bytes()).hexdigest()
    (args.output / 'SHA256SUMS').write_text(f'{digest}  {destination.name}\n')
    build_record(args.binaries, args.output, args.build, args.source_commit,
                 {'format': 'appimage', 'appimage': destination.name, 'sha256': digest})
    print(f'AppImage complete: {destination}')


if __name__ == '__main__':
    main()
