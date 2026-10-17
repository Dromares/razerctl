#!/usr/bin/env bash
# Pre-seeds cargo-packager's AppImage build tools from sha256-pinned downloads, so the third-party binaries that end up
# in the shipped AppImage are the ones recorded below and not whatever a moving release tag serves today (DAN-1627).
#
#   packaging/fetch-appimage-tools.sh cargo packager --release --formats appimage
#
# cargo-packager 0.11.x downloads AppRun, linuxdeploy and linuxdeploy-plugin-appimage into
# ${XDG_CACHE_HOME:-~/.cache}/.cargo-packager/AppImage/ only when they are missing, and never checks them. The plugin runs
# appimagetool, which downloads type2-runtime `continuous` unless LDAI_RUNTIME_FILE names one. This script verifies the four
# files, copies them into place (fresh each run: the packager patches linuxdeploy's ELF header in place), exports
# LDAI_RUNTIME_FILE, then execs the given command. A hash mismatch aborts before anything is built.
#
# The upstream tags are mutable (`continuous`, `linuxdeploy`, `apprun-old`); when one moves, this script fails closed. To bump
# a pin: download the new file, inspect it, and replace its hash here and in the README.
set -euo pipefail
cd "$(dirname "$0")/.."

arch=x86_64
dl=target/appimage-tools
tools=${XDG_CACHE_HOME:-$HOME/.cache}/.cargo-packager/AppImage

# name  url  sha256
pins=(
  "AppRun-$arch https://github.com/tauri-apps/binary-releases/releases/download/apprun-old/AppRun-$arch f30140a43a0a59e46db21bdefdf749b9e9f2c6946e92afabbacf98b8ae73fb4f"
  "linuxdeploy-$arch.AppImage https://github.com/tauri-apps/binary-releases/releases/download/linuxdeploy/linuxdeploy-$arch.AppImage e762bea85c8eb0d4b3508d46e5c1f037f717d0f9303ae3b4aafc8b04991fa1ef"
  "linuxdeploy-plugin-appimage.AppImage https://github.com/linuxdeploy/linuxdeploy-plugin-appimage/releases/download/continuous/linuxdeploy-plugin-appimage-$arch.AppImage 49d6a17160675a6bd1781699aae6bdf7692d98552e02a3671d2183d10547842e"
  "runtime-$arch https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-$arch 156f4bdbde9c52d01814600013e0a273f0118dc2de98975f3c8c63427ec79074"
)

[ "$(uname -m)" = "$arch" ] || { echo "fetch-appimage-tools: only $arch is pinned" >&2; exit 2; }
mkdir -p "$dl" "$tools"

verified() { echo "$2  $1" | sha256sum --check --status; }

for pin in "${pins[@]}"; do
  read -r name url sha <<<"$pin"
  if [ ! -f "$dl/$name" ] || ! verified "$dl/$name" "$sha"; then
    echo "fetching $name" >&2
    curl --fail --silent --show-error --location --output "$dl/$name.part" "$url"
    if ! verified "$dl/$name.part" "$sha"; then
      echo "fetch-appimage-tools: $name does not match the pinned sha256 $sha (got $(sha256sum "$dl/$name.part" | cut -d' ' -f1))" >&2
      rm -f "$dl/$name.part"
      exit 1
    fi
    mv "$dl/$name.part" "$dl/$name"
  fi
  chmod 755 "$dl/$name"
  [ "$name" = "runtime-$arch" ] || install -m 755 "$dl/$name" "$tools/$name"
done

export LDAI_RUNTIME_FILE=$PWD/$dl/runtime-$arch
echo "tools verified; LDAI_RUNTIME_FILE=$LDAI_RUNTIME_FILE" >&2
[ $# -eq 0 ] || exec "$@"
