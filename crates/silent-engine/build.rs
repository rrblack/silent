fn main() {
    // libwebrtc's Wayland screen-capture portal calls into GLib/GIO but the
    // prebuilt archive only compiles against their headers.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        for lib in ["gio-2.0", "gobject-2.0", "glib-2.0"] {
            pkg_config::probe_library(lib).unwrap_or_else(|e| panic!("{lib} not found: {e}"));
        }
    }
}
