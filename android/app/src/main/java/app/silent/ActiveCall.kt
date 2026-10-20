package app.silent

import app.silent.ffi.SilentRoom

/** The room currently open, shared with the capture services. */
object ActiveCall {
    @Volatile
    var room: SilentRoom? = null
}
