#!/usr/bin/env bash
# Packs the release build into "Endo's Unified Downloader.app" and makes the two files a macOS
# release ships for this Mac's architecture: the DMG for people and the .app.tar.gz the updater
# installs. Run it on a Mac after `cargo build --release`:
#   packaging/macos/bundle.sh <version> [output folder, default target/macos]
# It stops at the first step or check that fails.
set -euo pipefail

version="${1:?usage: bundle.sh <version> [output folder]}"
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
out="${2:-$repo/target/macos}"
bin="$repo/target/release"
name="Endo's Unified Downloader"
fail() { echo "bundle.sh: $*" >&2; exit 1; }

case "$(uname -m)" in
  arm64) arch=arm64; label=apple-silicon ;;
  x86_64) arch=x64; label=intel ;;
  *) fail "unsupported architecture $(uname -m)" ;;
esac
dmg="$out/Endos-Unified-Downloader-v$version-macos-$label.dmg"
tarball="$out/Endos-Unified-Downloader-macos-$arch.app.tar.gz"
app="$out/$name.app"

# The binaries must be this Mac's architecture, since the file names say so.
for exe in Endos-Unified-Downloader Endos-Unified-Downloader-CLI; do
  [ "$(lipo -archs "$bin/$exe")" = "$(uname -m)" ] || fail "$bin/$exe is not built for $(uname -m)"
done

rm -rf "$app" "$dmg" "$tarball" "$out/AppIcon.iconset" "$out/dmg"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin/Endos-Unified-Downloader" "$bin/Endos-Unified-Downloader-CLI" "$app/Contents/MacOS/"
sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"

# The icon: every size macOS asks for, from the 1024 px PNG.
iconset="$out/AppIcon.iconset"
mkdir "$iconset"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$here/AppIcon.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  sips -z $((size * 2)) $((size * 2)) "$here/AppIcon.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
rm -rf "$iconset"

# The browser extension without its tests; the app copies it to its data folder for Load unpacked.
cp -R "$repo/extension" "$app/Contents/Resources/extension"
rm -rf "$app/Contents/Resources/extension/test" "$app/Contents/Resources/extension/package.json"

# Ad-hoc signature (there is no Developer ID): the CLI first, then the bundle, which seals the rest.
codesign --force --sign - "$app/Contents/MacOS/Endos-Unified-Downloader-CLI"
codesign --force --sign - "$app"
codesign --verify --strict --verbose=2 "$app"

# The versions the updater compares, and the layout it expects.
plist_version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist")"
[ "$plist_version" = "$version" ] || fail "Info.plist says $plist_version, not $version"
cli_version="$("$app/Contents/MacOS/Endos-Unified-Downloader-CLI" --version)"
[ "$cli_version" = "Endos-Unified-Downloader-CLI $version" ] || fail "the CLI says '$cli_version', not $version"
[ -x "$app/Contents/MacOS/Endos-Unified-Downloader" ] || fail "the GUI is missing"
[ -f "$app/Contents/Resources/AppIcon.icns" ] || fail "the icon is missing"
[ -f "$app/Contents/Resources/extension/manifest.json" ] || fail "the extension is missing"

# For the updater: the bundle folder at the root of the archive, no symlinks, no AppleDouble files.
COPYFILE_DISABLE=1 tar -czf "$tarball" -C "$out" "$name.app"
listing="$(tar -tvzf "$tarball")"
if grep -q '^l' <<<"$listing"; then fail "the archive holds a symlink"; fi
if grep -q '/\._' <<<"$listing"; then fail "the archive holds AppleDouble files"; fi
if tar -tzf "$tarball" | grep -qv "^$name\.app/"; then fail "the archive has entries outside $name.app/"; fi

# For people: the app, a link to Applications and the read-me.
stage="$out/dmg"
mkdir "$stage"
ditto "$app" "$stage/$name.app"
ln -s /Applications "$stage/Applications"
cp "$here/Read me first.txt" "$stage/"
# hdiutil now and then fails with "Resource busy" on CI Macs; a retry gets through.
for try in 1 2 3; do
  if hdiutil create -volname "$name" -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$dmg"; then break; fi
  [ "$try" -lt 3 ] || fail "hdiutil create failed"
  sleep 5
done
rm -rf "$stage"
hdiutil verify "$dmg"

# The DMG as people get it: the app in it opens (signature intact), next to Applications and the read-me.
mnt="$(mktemp -d)"
hdiutil attach -nobrowse -readonly -mountpoint "$mnt" "$dmg" >/dev/null
trap 'hdiutil detach "$mnt" >/dev/null 2>&1 || true' EXIT
codesign --verify --strict "$mnt/$name.app" || fail "the app in the DMG fails its signature check"
[ -L "$mnt/Applications" ] || fail "the DMG has no Applications link"
[ -f "$mnt/Read me first.txt" ] || fail "the DMG has no read-me"
hdiutil detach "$mnt" >/dev/null
trap - EXIT

echo "$dmg"
echo "$tarball"
