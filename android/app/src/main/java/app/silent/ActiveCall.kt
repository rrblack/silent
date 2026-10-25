package app.silent

import app.silent.ffi.SilentRoom

/** The room currently open, shared with the capture and call services. */
object ActiveCall {
    @Volatile
    var room: SilentRoom? = null

    /** Loudspeaker (true) or earpiece (false), when no headset is connected. */
    @Volatile
    var speaker: Boolean = true

    /** Set by whoever owns the call, so the call notification can act on it. Main thread only. */
    var controls: CallControls? = null
}

interface CallControls {
    val micOn: Boolean
    fun toggleMic()
    fun leave()

    /** Screen sharing ended, whatever ended it (the system, an error, the user). */
    fun screenStopped()
}
