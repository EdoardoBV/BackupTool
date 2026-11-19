#!/usr/bin/env bash
# Puts the FFmpeg that ships inside the app where the Tauri build expects it:
#   app/src-tauri/binaries/ffmpeg-<target triple>[.exe]
#
# The binary is not kept in git (about 100 MB): run this once after cloning,
# and again to move to a newer FFmpeg. Without it the Windows build stops
# with "resource path binaries/ffmpeg-... doesn't exist".
#
# Windows only for now. The build comes from gyan.dev, one of the Windows
# builds ffmpeg.org links to, at a pinned version and checksum. It is a GPL
# build: anyone given the app is entitled to FFmpeg's source and licence.
#
# The macOS app does not bundle FFmpeg yet and uses one installed on the Mac.
set -euo pipefail

VERSION="9.0.2"
SHA256="60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba"
URL="https://www.gyan.dev/ffmpeg/builds/packages/ffmpeg-${VERSION}-essentials_build.zip"

here="$(cd "$(dirname "$0")/.." && pwd)"
out="$here/app/src-tauri/binaries"
target="$out/ffmpeg-x86_64-pc-windows-msvc.exe"

case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) ;;
  *) echo "Only the Windows FFmpeg is bundled so far; nothing to fetch on $(uname -s)." && exit 0 ;;
esac

if [ -f "$target" ] && "$target" -version 2>/dev/null | head -1 | grep -q "version ${VERSION}-"; then
  echo "FFmpeg ${VERSION} is already in place."
  exit 0
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
echo "Downloading FFmpeg ${VERSION} (about 110 MB)..."
curl -fL --retry 5 --retry-delay 5 --retry-all-errors -o "$work/ffmpeg.zip" "$URL"

got="$(sha256sum "$work/ffmpeg.zip" | cut -d' ' -f1)"
if [ "$got" != "$SHA256" ]; then
  echo "Checksum mismatch: expected $SHA256, got $got. Nothing was installed." >&2
  exit 1
fi

unzip -q -j "$work/ffmpeg.zip" "*/bin/ffmpeg.exe" -d "$work"
mkdir -p "$out"
mv "$work/ffmpeg.exe" "$target"
"$target" -version | head -1
echo "Installed at ${target#"$here"/}"
