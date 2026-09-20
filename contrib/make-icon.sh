#!/usr/bin/env bash
# Build contrib/Glide.icns from contrib/icon.swift. Idempotent: rerun at will.
set -euo pipefail

contrib="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="$contrib/Glide.icns"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

iconset="$tmp/Glide.iconset"
mkdir -p "$iconset"

# Each slot is rendered from the vector source at its own pixel size rather
# than downsampled from the 1024: crisper edges, and it is what lets
# icon.swift swap in the simplified mark for the 16/32px slots.
for spec in \
  icon_16x16:16      icon_16x16@2x:32 \
  icon_32x32:32      icon_32x32@2x:64 \
  icon_128x128:128   icon_128x128@2x:256 \
  icon_256x256:256   icon_256x256@2x:512 \
  icon_512x512:512   icon_512x512@2x:1024
do
  swift "$contrib/icon.swift" "$iconset/${spec%%:*}.png" "${spec#*:}" >/dev/null
done

rm -f "$out"
iconutil -c icns "$iconset" -o "$out"

# The menu bar wants a template image: alpha only, tinted by macOS to match the
# bar in light and dark. Two scales, because the bar is drawn at both.
swift "$contrib/icon.swift" "$contrib/MenubarIcon.png" 18 template >/dev/null
swift "$contrib/icon.swift" "$contrib/MenubarIcon@2x.png" 36 template >/dev/null

echo "wrote $out"
