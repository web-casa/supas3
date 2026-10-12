#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
# Builds the Linux packages from a release binary: a .tar.gz and a .deb.
#   packaging/linux/package.sh <binary> <licenses-file> <out-dir>
# The architecture is the machine's own; the version comes from Cargo.toml.
set -euo pipefail
binary=$(realpath "$1")
licenses=$(realpath "$2")
out=$(mkdir -p "$3" && realpath "$3")
cd "$(dirname "$0")/../.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/supas3-app/Cargo.toml | head -1)
case $(uname -m) in
  x86_64) arch=x64 deb_arch=amd64 ;;
  aarch64) arch=arm64 deb_arch=arm64 ;;
  *) echo "unsupported architecture $(uname -m)" >&2; exit 2 ;;
esac
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Documents every package carries.
docs() {
  install -Dm644 LICENSE "$1/LICENSE"
  for notice in LICENSES/*; do
    install -Dm644 "$notice" "$1/$notice"
  done
  install -Dm644 THIRD_PARTY_NOTICES.md "$1/THIRD_PARTY_NOTICES.md"
  install -Dm644 "$licenses" "$1/THIRD_PARTY_LICENSES.txt"
}

# --- tarball: unpack anywhere and run ./supas3
name=supas3-$version-linux-$arch
tree=$work/$name
install -Dm755 "$binary" "$tree/supas3"
install -Dm644 packaging/linux/supas3.desktop "$tree/supas3.desktop"
install -Dm644 packaging/icons/supas3-256.png "$tree/supas3.png"
install -Dm644 README.md "$tree/README.md"
docs "$tree"
tar -C "$work" --owner=0 --group=0 --numeric-owner -czf "$out/$name.tar.gz" "$name"

# --- .deb
root=$work/deb
install -Dm755 "$binary" "$root/usr/bin/supas3"
install -Dm644 packaging/linux/supas3.desktop "$root/usr/share/applications/supas3.desktop"
install -Dm644 packaging/icons/supas3-256.png "$root/usr/share/icons/hicolor/256x256/apps/supas3.png"
docs "$root/usr/share/doc/supas3"
# Debian's name for the license file.
mv "$root/usr/share/doc/supas3/LICENSE" "$root/usr/share/doc/supas3/copyright"

# Libraries the binary links, as package names. Vulkan is loaded at run time,
# so nothing links it and it is named by hand.
mkdir -p "$work/shlibs/debian"
printf 'Source: supas3\n\nPackage: supas3\nArchitecture: any\n' > "$work/shlibs/debian/control"
linked=$(cd "$work/shlibs" && dpkg-shlibdeps -O "$root/usr/bin/supas3" 2>/dev/null | sed 's/^shlibs:Depends=//')
installed_kb=$(du -sk "$root" | cut -f1)
mkdir -p "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<CONTROL
Package: supas3
Version: $version
Section: net
Priority: optional
Architecture: $deb_arch
Depends: $linked, libvulkan1
Recommends: mesa-vulkan-drivers, libwayland-client0
Installed-Size: $installed_kb
Maintainer: supaS3 contributors <61506351+yeagoo@users.noreply.github.com>
Homepage: https://github.com/web-casa/supas3
Description: Multi-cloud object storage transfer workbench
 supaS3 browses Amazon S3 and S3-compatible storage, including Alibaba Cloud
 OSS, Tencent Cloud COS and Huawei Cloud OBS, in two panes side by side, and
 uploads, downloads, copies and syncs files between this computer and
 storage, or from one storage service to another.
CONTROL
dpkg-deb --root-owner-group --build "$root" "$out/supas3_${version}_$deb_arch.deb" >/dev/null

ls -l "$out"
