package app.silent

import android.Manifest
import android.app.Notification
import android.app.PendingIntent
import android.graphics.drawable.Icon
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.net.ConnectivityManager
import android.net.Network
import android.os.Build
import android.os.IBinder
import android.util.Log
import kotlin.concurrent.thread

/**
 * Keeps the call alive (microphone included) while the app is in the
 * background, routes audio for a call, and moves calls over when the phone
 * switches networks.
 */
class CallService : Service() {
    private var network: ConnectivityManager.NetworkCallback? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_MUTE -> ActiveCall.controls?.toggleMic()
            ACTION_LEAVE -> {
                ActiveCall.controls?.leave() ?: stopSelf()
                return START_NOT_STICKY
            }
        }
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                startForeground(ID, notification(), ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE)
            } else {
                startForeground(ID, notification())
            }
        } catch (e: RuntimeException) {
            // No microphone permission, or Android won't let us start from the
            // background: the call still works while the app is open.
            Log.w("silent", "call can't keep running in the background", e)
            stopSelf()
            return START_NOT_STICKY
        }
        if (intent?.action == null || intent.action == ACTION_ROUTE) routeAudio(true)
        watchNetwork()
        return START_NOT_STICKY
    }

    private fun notification(): Notification {
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )
        fun action(name: String, label: String) = Notification.Action.Builder(
            Icon.createWithResource(this, R.drawable.ic_silent_mono),
            label,
            PendingIntent.getService(
                this, name.hashCode(), Intent(this, CallService::class.java).setAction(name),
                PendingIntent.FLAG_IMMUTABLE,
            ),
        ).build()
        val muted = ActiveCall.controls?.micOn == false
        return Notification.Builder(this, SilentApp.CALL_CHANNEL)
            .setSmallIcon(R.drawable.ic_silent_mono)
            .setContentTitle(if (muted) "In a Silent room · muted" else "In a Silent room")
            .setContentText("End-to-end encrypted. Tap to return.")
            .setContentIntent(open)
            .setOngoing(true)
            .addAction(action(ACTION_MUTE, if (muted) "Unmute" else "Mute"))
            .addAction(action(ACTION_LEAVE, "Leave"))
            .build()
    }

    override fun onDestroy() {
        network?.let { getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(it) }
        network = null
        routeAudio(false)
        super.onDestroy()
    }

    /** Restarts ICE whenever the default network changes (Wi-Fi to mobile data, say). */
    private fun watchNetwork() {
        if (network != null) return
        val callback = object : ConnectivityManager.NetworkCallback() {
            private var current: Network? = null

            override fun onAvailable(available: Network) {
                val changed = current != null && current != available
                current = available
                if (changed) ActiveCall.room?.let { thread { it.restartConnections() } }
            }
        }
        getSystemService(ConnectivityManager::class.java).registerDefaultNetworkCallback(callback)
        network = callback
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
            // Prefer a headset if one is plugged in, otherwise the loudspeaker
            // or the earpiece, whichever the user picked.
            val builtIn = if (ActiveCall.speaker) {
                AudioDeviceInfo.TYPE_BUILTIN_SPEAKER
            } else {
                AudioDeviceInfo.TYPE_BUILTIN_EARPIECE
            }
            val devices = audio.availableCommunicationDevices
            val preferred = devices.firstOrNull {
                it.type == AudioDeviceInfo.TYPE_WIRED_HEADSET ||
                    it.type == AudioDeviceInfo.TYPE_BLUETOOTH_SCO ||
                    it.type == AudioDeviceInfo.TYPE_USB_HEADSET
            } ?: devices.firstOrNull { it.type == builtIn }
            preferred?.let { audio.setCommunicationDevice(it) }
        } else {
            audio.isSpeakerphoneOn = ActiveCall.speaker && !audio.isWiredHeadsetOn
        }
    }

    companion object {
        private const val ID = 1
        private const val ACTION_MUTE = "app.silent.MUTE"
        private const val ACTION_LEAVE = "app.silent.LEAVE"
        private const val ACTION_REFRESH = "app.silent.REFRESH"
        private const val ACTION_ROUTE = "app.silent.ROUTE"

        /** Keeps the call alive in the background. Needs the microphone permission. */
        fun start(context: Context) {
            val granted = context.checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED
            if (!granted) return
            runCatching { context.startForegroundService(Intent(context, CallService::class.java)) }
                .onFailure { Log.w("silent", "call can't keep running in the background", it) }
        }

        /** Moves call audio to the loudspeaker or the earpiece. */
        fun setSpeaker(context: Context, on: Boolean) {
            ActiveCall.speaker = on
            if (ActiveCall.room != null) {
                context.startService(Intent(context, CallService::class.java).setAction(ACTION_ROUTE))
            }
        }

        /** Redraws the notification, e.g. after muting from the app. */
        fun refresh(context: Context) {
            if (ActiveCall.room != null) {
                context.startService(Intent(context, CallService::class.java).setAction(ACTION_REFRESH))
            }
        }

        fun stop(context: Context) =
            context.stopService(Intent(context, CallService::class.java))
    }
}
