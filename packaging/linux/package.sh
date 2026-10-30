#!/usr/bin/env bash
# Packs the release build into the files a release ships for this Linux machine's architecture
# (x86_64 or aarch64): the tar.gz (both programs, the README, the browser extension, the menu
# entry and its icon, which install.sh installs), the .deb and the .rpm. Run it on Linux, with
# dpkg-deb and rpmbuild installed, after `cargo build --release --locked`:
#   packaging/linux/package.sh <version> [output folder, default target/linux]
# It stops at the first step or check that fails.
set -euo pipefail
umask 022

version="${1:?usage: package.sh <version> [output folder]}"
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
out="${2:-$repo/target/linux}"
bin="$repo/target/release"
fail() { echo "package.sh: $*" >&2; exit 1; }

# The release names: x64 and arm64 as on the other systems; Debian's and RPM's own names inside.
rpm_arch="$(uname -m)"
case "$rpm_arch" in
  x86_64) arch=x64 deb_arch=amd64 ;;
  aarch64) arch=arm64 deb_arch=arm64 ;;
  *) fail "no package is made for $rpm_arch" ;;
esac

cli_version="$("$bin/Endos-Unified-Downloader-CLI" --version)"
[ "$cli_version" = "Endos-Unified-Downloader-CLI $version" ] || fail "the CLI says '$cli_version', not $version"
[ -x "$bin/Endos-Unified-Downloader" ] || fail "the GUI is missing"

mkdir -p "$out"
out="$(cd "$out" && pwd)"
name="Endos-Unified-Downloader-v$version-linux-$arch"
tarball="$out/$name.tar.gz"
deb="$out/endos-unified-downloader_${version}_$deb_arch.deb"
rpm="$out/endos-unified-downloader-$version-1.$rpm_arch.rpm"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# The tar.gz: everything in one folder named like it; the extension without its tests.
dir="$work/$name"
mkdir "$dir"
cp "$bin/Endos-Unified-Downloader" "$bin/Endos-Unified-Downloader-CLI" "$repo/README.md" "$repo/LICENSE" "$repo/NOTICE" "$here/endos-unified-downloader.desktop" "$dir/"
cp "$repo/assets/logo.svg" "$dir/endos-unified-downloader.svg"
cp -R "$repo/extension" "$dir/extension"
rm -rf "$dir/extension/test" "$dir/extension/package.json"
rm -f "$tarball"
tar -czf "$tarball" -C "$work" --owner=0 --group=0 "$name"

# The .deb and the .rpm hold the same tree: the programs and the extension side by side in
# /usr/lib/endos-unified-downloader, as the app looks for the extension next to itself, and the
# commands in /usr/bin linking to them.
root="$work/deb"
lib="$root/usr/lib/endos-unified-downloader"
share="$root/usr/share"
mkdir -p "$lib" "$root/usr/bin" "$root/DEBIAN" "$share/applications" "$share/icons/hicolor/scalable/apps" "$share/endos-unified-downloader"
install -m 755 "$bin/Endos-Unified-Downloader" "$bin/Endos-Unified-Downloader-CLI" "$lib/"
cp -R "$dir/extension" "$share/endos-unified-downloader/extension"
ln -s ../../share/endos-unified-downloader/extension "$lib/extension"
ln -s ../lib/endos-unified-downloader/Endos-Unified-Downloader "$root/usr/bin/endos-downloader-gui"
ln -s ../lib/endos-unified-downloader/Endos-Unified-Downloader-CLI "$root/usr/bin/endos-downloader-cli"
ln -s endos-downloader-cli "$root/usr/bin/endos-downloader"
install -m 644 "$here/endos-unified-downloader.desktop" "$share/applications/"
install -m 644 "$repo/LICENSE" "$repo/NOTICE" "$share/endos-unified-downloader/"
install -m 644 "$repo/assets/logo.svg" "$share/icons/hicolor/scalable/apps/endos-unified-downloader.svg"

# The newest glibc symbol version the programs use is the oldest glibc they run with.
glibc="$(objdump -T "$lib/Endos-Unified-Downloader" "$lib/Endos-Unified-Downloader-CLI" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -n 1)"
[ -n "$glibc" ] || fail "no glibc version found in the programs"
glibc="${glibc#GLIBC_}"
summary="Multi-connection download accelerator"
description="A command-line tool and a desktop GUI on the same download engine, with the
 browser extension that sends downloads to the GUI."
home=https://github.com/Adityasharma0101911/Endo-s-Unified-Downloader
cat >"$root/DEBIAN/control" <<EOF
Package: endos-unified-downloader
Version: $version
Architecture: $deb_arch
Maintainer: Endo's Unified Downloader contributors <$home/issues>
Installed-Size: $(du -sk "$root/usr" | cut -f1)
Depends: libc6 (>= $glibc)
Recommends: libgl1, libxkbcommon0, xdg-desktop-portal
Section: net
Priority: optional
Homepage: $home
Description: $summary
 $description
EOF
rm -f "$deb"
dpkg-deb --root-owner-group --build "$root" "$deb" >/dev/null

# The .rpm, from the same tree as it is: no stripping, no debug package, no dependencies read
# from the programs (only glibc, as for the .deb).
mkdir -p "$work/rpm/SPECS"
cat >"$work/rpm/SPECS/endo.spec" <<EOF
Name: endos-unified-downloader
Version: $version
Release: 1
Summary: $summary
License: GPL-3.0-or-later
URL: $home
AutoReqProv: no
Requires: glibc >= $glibc
Recommends: mesa-libGL, libxkbcommon, xdg-desktop-portal
%global debug_package %{nil}
%global __os_install_post %{nil}
%define _build_id_links none

%description
$description

%install
mkdir -p %{buildroot}
cp -a "$root/usr" %{buildroot}/

%files
%defattr(-,root,root,-)
/usr/bin/endos-downloader
/usr/bin/endos-downloader-cli
/usr/bin/endos-downloader-gui
/usr/lib/endos-unified-downloader
/usr/share/endos-unified-downloader
/usr/share/applications/endos-unified-downloader.desktop
/usr/share/icons/hicolor/scalable/apps/endos-unified-downloader.svg
EOF
rpmbuild -bb --quiet --define "_topdir $work/rpm" "$work/rpm/SPECS/endo.spec" >/dev/null
rm -f "$rpm"
mv "$work/rpm/RPMS/$rpm_arch/endos-unified-downloader-$version-1.$rpm_arch.rpm" "$rpm"

# What install.sh and the app rely on.
listing="$(tar -tzf "$tarball")"
for file in Endos-Unified-Downloader Endos-Unified-Downloader-CLI LICENSE NOTICE endos-unified-downloader.desktop endos-unified-downloader.svg extension/manifest.json; do
  grep -qx "$name/$file" <<<"$listing" || fail "the tar.gz has no $file"
done
if grep -q "^$name/extension/test/" <<<"$listing"; then fail "the tar.gz holds the extension's tests"; fi
contents="$(dpkg-deb --contents "$deb")"
rpm_contents="$(rpm -qlp "$rpm" 2>/dev/null)"
for file in /usr/bin/endos-downloader-cli /usr/bin/endos-downloader-gui /usr/lib/endos-unified-downloader/extension /usr/share/endos-unified-downloader/extension/manifest.json /usr/share/endos-unified-downloader/LICENSE /usr/share/applications/endos-unified-downloader.desktop; do
  grep -q " \.$file\( ->\|$\)" <<<"$contents" || fail "the .deb has no $file"
  grep -qx "$file" <<<"$rpm_contents" || fail "the .rpm has no $file"
done

echo "$tarball"
echo "$deb"
echo "$rpm"
