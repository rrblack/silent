fn main() {
    // libwebrtc's Java classes (audio devices, hardware codecs) call back into
    // native code through `Java_livekit_org_webrtc_*` symbols. They live in
    // the static archive and must be kept and exported from this library.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        webrtc_sys_build::configure_jni_symbols().expect("configure libwebrtc JNI symbols");
    }
}
