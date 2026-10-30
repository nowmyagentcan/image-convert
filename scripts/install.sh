#!/bin/sh
# Installs image-convert @VERSION@ into ~/.local/bin, and its licences into ~/.local/share/doc/image-convert, after checking the archive's SHA-256.
# curl -fsSL https://github.com/nowmyagentcan/image-convert/releases/latest/download/install.sh | sh
set -eu
version=@VERSION@
base=${IMAGE_CONVERT_BASE:-https://github.com/nowmyagentcan/image-convert/releases/download/$version}
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target=linux-x64 ;;
  Linux-aarch64 | Linux-arm64) target=linux-arm64 ;;
  Darwin-x86_64) target=macos-x64 ;;
  Darwin-arm64) target=macos-arm64 ;;
  *) echo "image-convert: no build for $(uname -s) $(uname -m)." >&2; exit 1 ;;
esac
# The Linux builds use the system's glibc, as found on Ubuntu 22.04 and later.
case $target-$(ldd --version 2>&1 | head -n 1) in
  macos-* | *" 2.3"[5-9]* | *" 2."[4-9][0-9]*) ;;
  *) echo "image-convert needs glibc 2.35 or newer (Ubuntu 22.04, Debian 12, Fedora 36 or later). Alpine and other musl systems aren't supported yet." >&2; exit 1 ;;
esac
archive=image-convert-$version-$target.tar.gz
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/$archive" "$base/$archive"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"
expected=$(awk -v f="$archive" '$2 == f { print $1 }' "$tmp/SHA256SUMS")
if command -v sha256sum >/dev/null; then actual=$(sha256sum "$tmp/$archive"); else actual=$(shasum -a 256 "$tmp/$archive"); fi
if [ -z "$expected" ] || [ "${actual%% *}" != "$expected" ]; then
  echo "image-convert: the SHA-256 of $archive doesn't match SHA256SUMS; nothing was installed." >&2
  exit 1
fi
tar -xzf "$tmp/$archive" -C "$tmp" image-convert LICENSE THIRD-PARTY-LICENSES.txt
# An empty file "runs" as an empty script, so the answer is checked too.
case $("$tmp/image-convert" --version 2>/dev/null || :) in
  "image-convert "*) ;;
  *) echo "image-convert: $archive has no image-convert that runs here; nothing was installed." >&2; exit 1 ;;
esac
dir=$HOME/.local/bin
doc=$HOME/.local/share/doc/image-convert
mkdir -p "$dir" "$doc"
mv "$tmp/LICENSE" "$tmp/THIRD-PARTY-LICENSES.txt" "$doc"
mv "$tmp/image-convert" "$dir/image-convert"
"$dir/image-convert" --version
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "Add $dir to your PATH: export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
esac
