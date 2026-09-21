#!/usr/bin/env bash
#
# Build Glide.app and install it.
#
#   contrib/bundle.sh [destination]        (destination defaults to /Applications)
#
# Signing decides whether macOS keeps your permission grants.
#
#   ad-hoc (`--sign -`)  designated => cdhash H"..."        changes every build
#   a certificate        designated => identifier "dev.glide"
#                                      and certificate root = H"..."   stable
#
# TCC stores the designated requirement, so an ad-hoc build loses Accessibility
# and Input Monitoring on every rebuild, while a certificate-signed one keeps
# them. Any certificate works for this, including a self-signed one - Apple's
# $99 programme is only needed to ship to other people.
#
# Make one once:
#   openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
#     -keyout key.pem -out cert.pem -subj "/CN=Glide Self Signed/O=Glide" \
#     -addext "basicConstraints=critical,CA:false" \
#     -addext "keyUsage=critical,digitalSignature" \
#     -addext "extendedKeyUsage=critical,codeSigning"
#   openssl pkcs12 -export -legacy -out glide.p12 -inkey key.pem -in cert.pem \
#     -passout pass:glide -name "Glide Self Signed"
#   security import glide.p12 -k ~/Library/Keychains/login.keychain-db \
#     -T /usr/bin/codesign -P glide
set -euo pipefail

contrib="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname "$contrib")"
dest="${1:-/Applications}"

cargo build --release --manifest-path "$root/Cargo.toml"

staged="$(mktemp -d)"
trap 'rm -rf "$staged"' EXIT
app="$staged/Glide.app"

mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$root/target/release/glide" "$app/Contents/MacOS/glide"
chmod +x "$app/Contents/MacOS/glide"
cp "$contrib/Glide.icns" "$app/Contents/Resources/Glide.icns"
cp "$contrib/MenubarIcon.png" "$app/Contents/Resources/MenubarIcon.png"
cp "$contrib/MenubarIcon@2x.png" "$app/Contents/Resources/MenubarIcon@2x.png"
cp "$contrib/Info.plist" "$app/Contents/Info.plist"

# Prefer the local signing certificate; fall back to ad-hoc with a warning,
# because an ad-hoc build will silently lose its input permissions.
identity="$(security find-certificate -c "Glide Self Signed" -Z 2>/dev/null \
  | awk '/SHA-1 hash/{print $3; exit}' || true)"
if [ -n "$identity" ]; then
  codesign --force --deep --sign "$identity" "$app"
else
  echo "warning: no 'Glide Self Signed' certificate; signing ad-hoc, so macOS" >&2
  echo "         will drop the input permissions on every rebuild" >&2
  codesign --force --deep --sign - "$app"
fi

mkdir -p "$dest"
rm -rf "${dest%/}/Glide.app"
cp -R "$app" "${dest%/}/Glide.app"

echo "${dest%/}/Glide.app"
