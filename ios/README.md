# Silent for iOS

A SwiftUI app on the same Rust engine as Android and desktop, through the
UniFFI bindings in `crates/silent-ffi`.

**State:** voice, camera (front and back), chat, files, speaking indicators,
verify words, stats and screen sharing. CI builds it for the simulator
(`.github/workflows/ios.yml`). It has never run on a device.

**Screen sharing** goes through a ReplayKit Broadcast Upload Extension
(`Broadcast/`), a separate process with a 50 MB memory limit. It hands NV12
frames to the app over a Unix socket in the App Group container
(`Shared/ScreenSocket.swift`), and the app pushes them into the call. The App
Group (`group.app.silent`) only exists in a signed build, so screen sharing
can't work in the simulator build from CI. The frame rotation mapping is
untested.

**Missing:**
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
