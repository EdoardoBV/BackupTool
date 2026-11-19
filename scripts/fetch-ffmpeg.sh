#!/usr/bin/env bash
# Puts the FFmpeg that ships inside the app where the Tauri build expects it:
#   app/src-tauri/binaries/ffmpeg-<target triple>[.exe]
#
# The binaries are not kept in git (about 100 MB each): run this once after
# cloning, and again to move to a newer FFmpeg. Without it the build stops
# with "resource path binaries/ffmpeg-... doesn't exist".
#
# Every download is a pinned version with a pinned checksum:
#   Windows  gyan.dev, one of the Windows builds ffmpeg.org links to.
#   macOS    ffmpeg.martin-riedl.de, static builds for Apple Silicon and Intel
#            made with an open build script (git.martin-riedl.de/ffmpeg/build-script).
#            ffmpeg.org itself only links to Intel builds for the Mac.
# All of them are GPL builds: anyone given the app is entitled to FFmpeg's
# source and licence.
set -euo pipefail

VERSION="9.0.2"

WINDOWS_URL="https://www.gyan.dev/ffmpeg/builds/packages/ffmpeg-${VERSION}-essentials_build.zip"
WINDOWS_SHA256="60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba"

MAC_ARM_URL="https://ffmpeg.martin-riedl.de/download/macos/arm64/1789931890_${VERSION}/ffmpeg.zip"
MAC_ARM_SHA256="c8ed4c4e6978a03c485edbfe4e0a5dc2380f8a30bba5150531b31b094492d924"
MAC_INTEL_URL="https://ffmpeg.martin-riedl.de/download/macos/amd64/1789931006_${VERSION}/ffmpeg.zip"
MAC_INTEL_SHA256="7c6b4125b191cbf773832dc51f424cf2b6bb7da43007d1e066f95909e47cacd4"

here="$(cd "$(dirname "$0")/.." && pwd)"
out="$here/app/src-tauri/binaries"

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# fetch <url> <sha256> <file>: downloads, and stops unless the checksum matches.
fetch() {
  curl -fL --retry 5 --retry-delay 5 --retry-all-errors -o "$3" "$1"
  local got
  got="$(sha256_of "$3")"
  if [ "$got" != "$2" ]; then
    echo "Checksum mismatch for $1: expected $2, got $got. Nothing was installed." >&2
    exit 1
  fi
}

# Whether <file> is already this version of FFmpeg.
in_place() {
  [ -f "$1" ] && "$1" -version 2>/dev/null | head -1 | grep -q "version ${VERSION}[- ]"
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$out"

case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*)
    target="$out/ffmpeg-x86_64-pc-windows-msvc.exe"
    if in_place "$target"; then
      echo "FFmpeg ${VERSION} is already in place."
      exit 0
    fi
    echo "Downloading FFmpeg ${VERSION} for Windows (about 110 MB)..."
    fetch "$WINDOWS_URL" "$WINDOWS_SHA256" "$work/ffmpeg.zip"
    unzip -q -j "$work/ffmpeg.zip" "*/bin/ffmpeg.exe" -d "$work"
    mv "$work/ffmpeg.exe" "$target"
    "$target" -version | head -1
    echo "Installed at ${target#"$here"/}"
    ;;

  Darwin)
    # The app is built once for both kinds of Mac. Cargo compiles it for each
    # in turn and wants the FFmpeg of that architecture; the bundle then takes
    # the two joined into one universal binary.
    arm="$out/ffmpeg-aarch64-apple-darwin"
    intel="$out/ffmpeg-x86_64-apple-darwin"
    universal="$out/ffmpeg-universal-apple-darwin"
    if in_place "$universal" && [ -f "$arm" ] && [ -f "$intel" ]; then
      echo "FFmpeg ${VERSION} is already in place."
      exit 0
    fi
    echo "Downloading FFmpeg ${VERSION} for Apple Silicon and Intel (about 60 MB)..."
    fetch "$MAC_ARM_URL" "$MAC_ARM_SHA256" "$work/arm.zip"
    fetch "$MAC_INTEL_URL" "$MAC_INTEL_SHA256" "$work/intel.zip"
    mkdir "$work/arm" "$work/intel"
    unzip -q -j "$work/arm.zip" ffmpeg -d "$work/arm"
    unzip -q -j "$work/intel.zip" ffmpeg -d "$work/intel"
    lipo -create -output "$work/universal" "$work/arm/ffmpeg" "$work/intel/ffmpeg"
    mv "$work/arm/ffmpeg" "$arm"
    mv "$work/intel/ffmpeg" "$intel"
    mv "$work/universal" "$universal"
    chmod +x "$arm" "$intel" "$universal"
    lipo -archs "$universal"
    "$universal" -version | head -1
    echo "Installed in ${out#"$here"/}"
    ;;

  *)
    echo "No FFmpeg is bundled for $(uname -s); nothing to fetch."
    ;;
esac
