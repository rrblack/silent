#!/usr/bin/env bash
# Packages the release build (cargo build --release -p silent-app) into dist/:
#   macOS:   Silent-<version>-macOS.dmg with Silent.app (ad-hoc signed)
#   Linux:   silent_<version>_<arch>.deb, plus silent-<version>-linux-<arch>.tar.gz
#            with the app and silent-relay
#   Windows: Silent-<version>-windows.zip with Silent.exe and silent-relay.exe
# Build silent-relay too (-p silent-relay) to have it in the archives.
# SILENT_BIN_DIR overrides where the built binary is taken from.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(dirname "$here")
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n1)
bin=${SILENT_BIN_DIR:-$root/target/release}
dist=$root/dist
rm -rf "$dist"
mkdir -p "$dist"

case "$(uname -s)" in
Darwin)
    stage=$(mktemp -d)
    app=$stage/Silent.app
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    cp "$bin/silent" "$app/Contents/MacOS/silent"
    cp "$here/silent.icns" "$app/Contents/Resources/silent.icns"
    sed "s/__VERSION__/$version/g" "$here/macos/Info.plist" >"$app/Contents/Info.plist"
    # Without a signature Apple Silicon refuses to run the bundle; ad hoc is
    # enough until there is a Developer ID.
    codesign --force --deep --sign - "$app"
    ln -s /Applications "$stage/Applications"
    hdiutil create -volname Silent -srcfolder "$stage" -ov -format UDZO "$dist/Silent-$version-macOS.dmg"
    rm -rf "$stage"
    ;;
Linux)
    arch=$(dpkg --print-architecture 2>/dev/null || uname -m)
    stage=$(mktemp -d)
    pkg=$stage/silent_${version}_$arch
    install -Dm755 "$bin/silent" "$pkg/usr/bin/silent"
    install -Dm644 "$here/linux/silent.desktop" "$pkg/usr/share/applications/silent.desktop"
    install -Dm644 "$here/silent.png" "$pkg/usr/share/icons/hicolor/256x256/apps/silent.png"
    # dpkg-shlibdeps wants to run inside a source package.
    mkdir -p "$stage/debian"
    printf 'Source: silent\n\nPackage: silent\nArchitecture: any\n' >"$stage/debian/control"
    depends=$(cd "$stage" && dpkg-shlibdeps -O "$pkg/usr/bin/silent" 2>/dev/null | sed -n 's/^shlibs:Depends=//p')
    # Loaded at run time, so dpkg-shlibdeps can't see them: keyboard, OpenGL, audio.
    depends="${depends:+$depends, }libxkbcommon-x11-0, libegl1 | libgl1, libpulse0"
    mkdir -p "$pkg/DEBIAN"
    cat >"$pkg/DEBIAN/control" <<CONTROL
Package: silent
Version: $version
Architecture: $arch
Maintainer: Silent <noreply@silent.invalid>
Depends: $depends
Recommends: pipewire, xdg-desktop-portal
Section: net
Priority: optional
Description: Encrypted calls, screen sharing and chat for up to four friends
 Peer-to-peer voice, video, screen sharing and chat. No accounts; rooms are
 joined with a short code and nothing is stored.
CONTROL
    dpkg-deb --root-owner-group --build "$pkg" "$dist/silent_${version}_$arch.deb"
    tarball=$stage/silent-$version
    mkdir -p "$tarball"
    cp "$bin/silent" "$tarball/"
    if [ -f "$bin/silent-relay" ]; then cp "$bin/silent-relay" "$tarball/"; fi
    tar -C "$stage" -czf "$dist/silent-$version-linux-$arch.tar.gz" "silent-$version"
    rm -rf "$stage"
    ;;
MINGW* | MSYS* | CYGWIN*)
    stage=$(mktemp -d)
    cp "$bin/silent.exe" "$stage/Silent.exe"
    if [ -f "$bin/silent-relay.exe" ]; then cp "$bin/silent-relay.exe" "$stage/"; fi
    (cd "$stage" && 7z a -tzip "$dist/Silent-$version-windows.zip" ./* >/dev/null)
    rm -rf "$stage"
    ;;
*)
    echo "unsupported OS: $(uname -s)" >&2
    exit 1
    ;;
esac
ls -l "$dist"
