# Silent for iOS

A SwiftUI app on the same Rust engine as Android and desktop, through the
UniFFI bindings in `crates/silent-ffi`.

**State:** voice, camera (front and back), chat, verify words and stats. CI
builds it for the simulator (`.github/workflows/ios.yml`). It has never run
on a device.

**Missing:**
- Screen sharing: it needs a ReplayKit Broadcast Upload Extension, a
  separate process with a 50 MB memory limit that pushes frames to the app.
- CallKit, so calls survive the lock screen like phone calls.
- Signing: running on an iPhone needs an Apple Developer account ($99/yr).

## Building (needs a Mac with Xcode)

```sh
brew install xcodegen
ios/build-native.sh                   # Rust library + Swift bindings → SilentFFI.xcframework
cd ios && xcodegen generate           # Silent.xcodeproj (set SILENT_RELAY_URL first to bake the relay in)
open Silent.xcodeproj                 # pick your team under Signing, then run
```

`build-native.sh` builds for devices and Apple Silicon simulators; set
`SILENT_IOS_TARGETS` to build only one of them. The project, the bindings and
the XCFramework are generated and gitignored.
