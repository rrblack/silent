#!/usr/bin/env bash
# Builds the Rust engine as a static library for iOS, generates the Swift
# bindings and wraps both in SilentFFI.xcframework. Run on a Mac before
# `xcodegen generate` (see ios/README.md).
#
# SILENT_IOS_TARGETS picks the Rust targets (default: device and Apple
# Silicon simulator).
set -euo pipefail
cd "$(dirname "$0")/.."

read -r -a targets <<<"${SILENT_IOS_TARGETS:-aarch64-apple-ios aarch64-apple-ios-sim}"
export IPHONEOS_DEPLOYMENT_TARGET=16.0

for target in "${targets[@]}"; do
    rustup target add "$target" >/dev/null
    cargo rustc -p silent-ffi --lib --crate-type staticlib --profile mobile --target "$target"
done

out=ios/build
rm -rf "$out" ios/SilentFFI.xcframework
mkdir -p "$out/headers" ios/Generated
cargo run -q -p uniffi-bindgen -- generate \
    --library "target/${targets[0]}/mobile/libsilent_ffi.a" \
    --language swift --out-dir "$out/bindings" --no-format
cp "$out/bindings/SilentFFI.swift" ios/Generated/
cp "$out/bindings/SilentFFIFFI.h" "$out/headers/"
cp "$out/bindings/SilentFFIFFI.modulemap" "$out/headers/module.modulemap"

args=()
for target in "${targets[@]}"; do
    args+=(-library "target/$target/mobile/libsilent_ffi.a" -headers "$out/headers")
done
xcodebuild -create-xcframework "${args[@]}" -output ios/SilentFFI.xcframework
echo "SilentFFI.xcframework and ios/Generated/SilentFFI.swift are in place"
