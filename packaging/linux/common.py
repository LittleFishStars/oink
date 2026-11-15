"""Linux 打包共享常量与产物元数据（AppImage / archlinux / debian 三格式共用）。

产物内容（源自 packaging/identity.json 与 build.py 的 release-build.json）：
- oink           UI（Tauri，前端经 custom-protocol 嵌入）
- oink-agent     后台服务（组网/键鼠/剪贴板）
- oink-crash-collector  崩溃收集器
"""
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'build-support'))
from identity import IDENTITY  # noqa: E402

PRODUCT = IDENTITY['name']
IDENTIFIER = IDENTITY['bundle_id']
VERSION = IDENTITY['version']
UNIX_NAME = IDENTITY['unix_data_directory']  # oink
DESCRIPTION = 'Share keyboard, mouse and clipboard between computers (software KVM)'
HOMEPAGE = f"https://github.com/{IDENTITY['repository_org']}/oink"
LICENSE = 'GPL-3.0-only'

# 运行时角色（build.py: linux roles）
BINARIES = ('oink', 'oink-agent', 'oink-crash-collector')

# WebKitGTK 等 Tauri 运行时依赖由各格式自行声明；这里只列产品自带的资源。
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

DESKTOP_FILE = f"""[Desktop Entry]
Type=Application
Name={PRODUCT}
GenericName=Software KVM
Comment={DESCRIPTION}
Exec=oink
Icon={UNIX_NAME}
Categories=Utility;Network;
Keywords=kvm;keyboard;mouse;clipboard;sharing;
Terminal=false
StartupWMClass={IDENTIFIER}
X-KDE-StartupNotify=false
"""


def build_record(binaries_dir: Path, output: Path, build: int, source_commit, extra: dict) -> dict:
    """三格式共同的发布元数据；binaries 哈希与 build.py 的产物对账。"""
    import hashlib
    record = {
        'schema': 1,
        'platform': 'linux-x64',
        'version': VERSION,
        'build': build,
        'source_commit': source_commit,
        'files': {},
        **extra,
    }
    for name in BINARIES:
        data = (binaries_dir / name).read_bytes()
        record['files'][name] = hashlib.sha256(data).hexdigest()
    destination = output / 'release.json'
    destination.write_text(json.dumps(record, indent=2) + '\n')
    return record


def write_desktop(root: Path) -> None:
    (root / f'{UNIX_NAME}.desktop').write_text(DESKTOP_FILE)


def icon_png(source_icons: Path, size: int) -> bytes:
    """按尺寸取产品图标；128@2x 是最高分辨率源。"""
    candidates = [source_icons / f'{size}x{size}.png']
    if size > 128:
        candidates.insert(0, source_icons / f'128x128@2x.png')
    for candidate in candidates:
        if candidate.exists():
            return candidate.read_bytes()
    return (source_icons / 'icon.png').read_bytes()
