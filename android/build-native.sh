#!/usr/bin/env bash
# Builds the Rust engine for Android, regenerates the Kotlin bindings and
# copies libwebrtc's Java classes into the app. Run before ./gradlew.
#
# Needs: Rust with the aarch64-linux-android target, cargo-ndk, and
# ANDROID_NDK_HOME pointing at an NDK (r28 is what CI uses).
set -euo pipefail
cd "$(dirname "$0")/.."

: "${ANDROID_NDK_HOME:?set ANDROID_NDK_HOME to your NDK, e.g. \$ANDROID_HOME/ndk/28.2.13676358}"

cargo ndk -t arm64-v8a -P 26 -o android/app/src/main/jniLibs build -p silent-ffi --profile mobile

cargo run -q -p uniffi-bindgen -- generate \
    --library target/aarch64-linux-android/mobile/libsilent_ffi.so \
    --language kotlin --out-dir android/app/src/main/java --no-format

jar=$(find target -path "*android-arm64-release/libwebrtc.jar" -print -quit)
if [ -z "$jar" ]; then
    echo "libwebrtc.jar not found under target/ (webrtc-sys should have downloaded it)" >&2
    exit 1
fi
mkdir -p android/app/libs
cp "$jar" android/app/libs/libwebrtc.jar
echo "native library, bindings and libwebrtc.jar are in place"
