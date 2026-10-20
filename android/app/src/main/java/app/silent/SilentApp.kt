package app.silent

import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context

class SilentApp : Application() {
    override fun onCreate() {
        super.onCreate()
        System.loadLibrary("silent_ffi")
        SilentNative.init(this)
        getSystemService(NotificationManager::class.java).createNotificationChannel(
            NotificationChannel(CALL_CHANNEL, "Calls", NotificationManager.IMPORTANCE_LOW).apply {
                description = "Shown while you are in a Silent room"
            }
        )
    }

    companion object {
        const val CALL_CHANNEL = "call"
    }
}

/** JNI entry points that UniFFI doesn't cover (see crates/silent-ffi/src/android.rs). */
object SilentNative {
    @JvmStatic
    external fun init(context: Context)
}
