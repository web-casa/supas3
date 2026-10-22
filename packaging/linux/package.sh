#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
# Builds the Linux packages from a release binary: a .tar.gz, a .deb, an
# .rpm (needs rpmbuild) and an AppImage (downloads a pinned appimagetool and
# runtime, checked against their SHA-256).
#   packaging/linux/package.sh <binary> <licenses-file> <out-dir>
# The architecture is the machine's own; the version comes from Cargo.toml.
set -euo pipefail
binary=$(realpath "$1")
licenses=$(realpath "$2")
out=$(mkdir -p "$3" && realpath "$3")
cd "$(dirname "$0")/../.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/supas3-app/Cargo.toml | head -1)
case $(uname -m) in
  x86_64) arch=x64 deb_arch=amd64
    appimagetool_sha256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
    runtime_sha256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d ;;
  aarch64) arch=arm64 deb_arch=arm64
    appimagetool_sha256=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158
    runtime_sha256=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444 ;;
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

# --- .rpm: the same files; library dependencies are found by rpmbuild
machine=$(uname -m)
# RPM versions cannot hold "-"; a pre-release sorts before the release
# with "~" (0.3.0~rc.1 < 0.3.0).
rpm_version=${version//-/\~}
stage=$work/rpmroot
install -Dm755 "$binary" "$stage/usr/bin/supas3"
install -Dm644 packaging/linux/supas3.desktop "$stage/usr/share/applications/supas3.desktop"
install -Dm644 packaging/icons/supas3-256.png "$stage/usr/share/icons/hicolor/256x256/apps/supas3.png"
docs "$stage/usr/share/doc/supas3"
topdir=$work/rpmbuild
mkdir -p "$topdir/SPECS"
cat > "$topdir/SPECS/supas3.spec" <<SPEC
%global debug_package %{nil}
%global __strip /bin/true
Name: supas3
Version: $rpm_version
Release: 1
Summary: Multi-cloud object storage transfer workbench
License: AGPL-3.0-or-later
URL: https://github.com/web-casa/supas3
# Vulkan is loaded at run time, so nothing links it.
Requires: vulkan-loader
Recommends: mesa-vulkan-drivers

%description
supaS3 browses Amazon S3 and S3-compatible storage, including Alibaba Cloud
OSS, Tencent Cloud COS and Huawei Cloud OBS, in two panes side by side, and
uploads, downloads, copies and syncs files between this computer and
storage, or from one storage service to another.

%install
cp -a $stage/. %{buildroot}/

%files
/usr/bin/supas3
/usr/share/applications/supas3.desktop
/usr/share/icons/hicolor/256x256/apps/supas3.png
# Licenses, unlike documentation, are installed even where documentation
# is left out (tsflags=nodocs, as in Fedora's container images).
%dir /usr/share/doc/supas3
%license /usr/share/doc/supas3/LICENSE
%license /usr/share/doc/supas3/LICENSES
%license /usr/share/doc/supas3/THIRD_PARTY_NOTICES.md
%license /usr/share/doc/supas3/THIRD_PARTY_LICENSES.txt
SPEC
rpmbuild --quiet --define "_topdir $topdir" --target "$machine" -bb "$topdir/SPECS/supas3.spec"
cp "$topdir/RPMS/$machine/supas3-$rpm_version-1.$machine.rpm" "$out/supas3-$version-1.$machine.rpm"

# --- AppImage: one file that runs on most distributions of the same age
tools=${SUPAS3_APPIMAGE_TOOLS:-$work/appimage-tools}
mkdir -p "$tools"
fetch() { # <url> <file> <sha256>
  [ -f "$2" ] || curl -sSfL -o "$2" "$1"
  echo "$3  $2" | sha256sum -c --quiet - || { rm -f "$2"; echo "checksum mismatch: $1" >&2; exit 1; }
}
fetch "https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-$machine.AppImage" \
  "$tools/appimagetool-$machine.AppImage" "$appimagetool_sha256"
fetch "https://github.com/AppImage/type2-runtime/releases/download/20251108/runtime-$machine" \
  "$tools/runtime-$machine" "$runtime_sha256"
chmod +x "$tools/appimagetool-$machine.AppImage"
appdir=$work/supaS3.AppDir
install -Dm755 "$binary" "$appdir/usr/bin/supas3"
install -Dm644 packaging/linux/supas3.desktop "$appdir/supas3.desktop"
install -Dm644 packaging/linux/supas3.desktop "$appdir/usr/share/applications/supas3.desktop"
install -Dm644 packaging/icons/supas3-256.png "$appdir/supas3.png"
install -Dm644 packaging/icons/supas3-256.png "$appdir/usr/share/icons/hicolor/256x256/apps/supas3.png"
ln -s supas3.png "$appdir/.DirIcon"
docs "$appdir/usr/share/doc/supas3"
cat > "$appdir/AppRun" <<'APPRUN'
#!/bin/sh
here=$(dirname "$(readlink -f "$0")")
exec "$here/usr/bin/supas3" "$@"
APPRUN
chmod 755 "$appdir/AppRun"
# Without FUSE (containers, CI) the tool extracts itself to run.
APPIMAGE_EXTRACT_AND_RUN=1 ARCH=$machine "$tools/appimagetool-$machine.AppImage" --no-appstream \
  --runtime-file "$tools/runtime-$machine" "$appdir" "$out/supas3-$version-linux-$arch.AppImage" \
  >"$work/appimagetool.log" 2>&1 || { cat "$work/appimagetool.log" >&2; echo "appimagetool failed" >&2; exit 1; }

ls -l "$out"
