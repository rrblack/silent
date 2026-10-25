# Third-party code

## webrtc-sys

[webrtc-sys](https://crates.io/crates/webrtc-sys) 0.3.49 from LiveKit
(Apache-2.0, see `webrtc-sys/NOTICE.md`), patched in through
`[patch.crates-io]` in the workspace `Cargo.toml`. One change, in
`src/webrtc.cpp`:

```diff
-  webrtc::LogMessage::AddLogToStream(this, webrtc::LoggingSeverity::LS_VERBOSE);
+  webrtc::LogMessage::AddLogToStream(this, webrtc::LoggingSeverity::LS_WARNING);
```

The upstream log sink asks libwebrtc for verbose logs, so every log line in
libwebrtc is formatted and handed to Rust, and dcSCTP hex-dumps every data
channel packet. That kept file transfers under 1 MB/s and cost CPU in every
call. As of webrtc-sys 0.3.51 upstream still hardcodes verbose.

To move to a newer webrtc-sys, copy the new crate here and reapply the change,
or drop the patch once upstream makes the level configurable.
