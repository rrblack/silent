//! JNI entry point. UniFFI covers the API; libwebrtc additionally needs the
//! Java VM and the application context before the first call.

use jni::objects::{JClass, JObject};
use jni::JNIEnv;

/// `app.silent.SilentNative.init(context)`, called once from
/// `Application.onCreate` after `System.loadLibrary("silent_ffi")`.
#[no_mangle]
pub extern "system" fn Java_app_silent_SilentNative_init(
    env: JNIEnv,
    _class: JClass,
    context: JObject,
) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("silent")
            .with_max_level(log::LevelFilter::Info),
    );
    let Ok(vm) = env.get_java_vm() else {
        log::error!("no Java VM");
        return;
    };
    silent_engine::libwebrtc::android::initialize_android(&vm);
    if !silent_engine::libwebrtc::android::initialize_android_context(&vm, &context) {
        log::error!("libwebrtc rejected the application context");
    }
    log::info!("silent native initialised");
}
