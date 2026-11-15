#!/usr/bin/env bash
# Linux 打包快速验证：用已编译的 release 二进制跑三个打包器并检查产物结构。
# 用法：bash scripts/linux-package.sh [debug|release]（默认 release）
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${1:-release}"
BIN="target/$PROFILE"
OUT="${OINK_PACKAGE_OUT:-/home/ylxc/Projects/.tmp/oink-packages}"

for name in oink oink-agent oink-crash-collector; do
  [ -x "$BIN/$name" ] || { echo "缺 $BIN/$name，先编译（见头注释）"; exit 1; }
done

rm -rf "$OUT"
mkdir -p "$OUT"/{appimage,archlinux,debian}
TREE_SHA=$(printf 'local-dev' | sha256sum | cut -d' ' -f1)

echo "=== 1/3 pacman（archlinux） ==="
if command -v makepkg >/dev/null 2>&1; then
  python3 packaging/archlinux/build.py --binaries "$BIN" --output "$OUT/archlinux" \
    --build 1 --source-tree-sha256 "$TREE_SHA"
  bsdtar -xOf "$OUT"/archlinux/*.pkg.tar.zst .PKGINFO | head -8
else
  echo "跳过：无 makepkg（pacman -S --needed base-devel）"
fi

echo "=== 2/3 .deb（debian） ==="
if command -v dpkg-deb >/dev/null 2>&1; then
  python3 packaging/debian/build.py --binaries "$BIN" --output "$OUT/debian" \
    --build 1 --source-tree-sha256 "$TREE_SHA"
  dpkg-deb --info "$OUT"/debian/*.deb | head -12
else
  echo "跳过：无 dpkg-deb"
fi

echo "=== 3/3 AppImage（linux） ==="
if command -v appimagetool >/dev/null 2>&1 || [ -n "${APPIMAGETOOL:-}" ]; then
  python3 packaging/linux/build.py --binaries "$BIN" --output "$OUT/appimage" \
    --build 1 --source-tree-sha256 "$TREE_SHA" ${APPIMAGETOOL:+--tool "$APPIMAGETOOL"}
  ls -lh "$OUT"/appimage/*.AppImage
else
  echo "跳过：无 appimagetool（设 APPIMAGETOOL=/path 指向已有二进制，或让它自动下载）"
fi

echo
echo "产物目录：$OUT"
find "$OUT" -maxdepth 2 \( -name '*.pkg.tar.zst' -o -name '*.deb' -o -name '*.AppImage' \) | sort
