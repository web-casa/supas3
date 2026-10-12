#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
# Builds supaS3.app and a .dmg holding it, from a release binary.
#   packaging/macos/package.sh <binary> <licenses-file> <out-dir>
# The bundle is signed ad hoc: it runs, but it is not notarized, so macOS
# asks the user to confirm the first launch.
set -euo pipefail
binary=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
licenses=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
mkdir -p "$3"
out=$(cd "$3" && pwd)
cd "$(dirname "$0")/../.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/supas3-app/Cargo.toml | head -1)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

app=$work/dmg/supaS3.app
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/Licenses/LICENSES"
cp "$binary" "$app/Contents/MacOS/supas3"
chmod 755 "$app/Contents/MacOS/supas3"
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"

# The .icns holds the icon at every size the system asks for.
iconset=$work/supas3.iconset
mkdir "$iconset"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" packaging/icons/supas3-1024.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" packaging/icons/supas3-1024.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/supas3.icns"

cp LICENSE THIRD_PARTY_NOTICES.md "$app/Contents/Resources/Licenses/"
cp LICENSES/* "$app/Contents/Resources/Licenses/LICENSES/"
cp "$licenses" "$app/Contents/Resources/Licenses/THIRD_PARTY_LICENSES.txt"

codesign --force --sign - "$app"
codesign --verify --strict "$app"

# The disk image shows the app next to a shortcut to Applications.
ln -s /Applications "$work/dmg/Applications"
cp LICENSE "$work/dmg/LICENSE.txt"
name=supas3-$version-macos-arm64
hdiutil create -volname "supaS3 $version" -srcfolder "$work/dmg" -ov -format UDZO "$out/$name.dmg" >/dev/null
hdiutil verify "$out/$name.dmg" >/dev/null

ls -l "$out"
