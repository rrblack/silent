package app.silent

import android.app.Notification
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.os.Build
import android.os.IBinder

/**
 * Keeps the call alive (microphone included) while the app is in the
 * background, and routes audio for a call.
 */
class CallService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = Notification.Builder(this, SilentApp.CALL_CHANNEL)
            .setSmallIcon(R.drawable.ic_silent)
            .setContentTitle("In a Silent room")
            .setContentText("End-to-end encrypted. Tap to return.")
            .setContentIntent(open)
            .setOngoing(true)
            .build()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE)
        } else {
            startForeground(ID, notification)
        }
        routeAudio(true)
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        routeAudio(false)
        super.onDestroy()
    }

    @Suppress("DEPRECATION")
    private fun routeAudio(inCall: Boolean) {
        val audio = getSystemService(AudioManager::class.java)
        if (!inCall) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) audio.clearCommunicationDevice()
            else audio.isSpeakerphoneOn = false
            audio.mode = AudioManager.MODE_NORMAL
            return
        }
        audio.mode = AudioManager.MODE_IN_COMMUNICATION
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            // Prefer a headset if one is plugged in, otherwise the loudspeaker.
            val devices = audio.availableCommunicationDevices
            val preferred = devices.firstOrNull {
                it.type == AudioDeviceInfo.TYPE_WIRED_HEADSET ||
                    it.type == AudioDeviceInfo.TYPE_BLUETOOTH_SCO ||
                    it.type == AudioDeviceInfo.TYPE_USB_HEADSET
            } ?: devices.firstOrNull { it.type == AudioDeviceInfo.TYPE_BUILTIN_SPEAKER }
            preferred?.let { audio.setCommunicationDevice(it) }
        } else {
            audio.isSpeakerphoneOn = !audio.isWiredHeadsetOn
        }
    }

    companion object {
        private const val ID = 1

        fun start(context: Context) =
            context.startForegroundService(Intent(context, CallService::class.java))

        fun stop(context: Context) =
            context.stopService(Intent(context, CallService::class.java))
    }
}
