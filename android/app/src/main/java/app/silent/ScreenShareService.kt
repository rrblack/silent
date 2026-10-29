package app.silent

import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.app.Notification
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.ImageReader
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.util.Log
import kotlin.concurrent.thread
import kotlin.math.max
import kotlin.math.roundToInt

/**
 * Captures the screen with MediaProjection and pushes RGBA frames into the
 * room, and on Android 10+ the phone's media and game sound with it (never
 * the call itself, so friends don't hear themselves). Android requires this
 * to run as a mediaProjection foreground service.
 */
class ScreenShareService : Service() {
    private var projection: MediaProjection? = null
    private var display: VirtualDisplay? = null
    private var reader: ImageReader? = null
    private var thread: HandlerThread? = null
    private var audio: AudioRecord? = null
    private var audioThread: Thread? = null
    @Volatile
    private var capturingAudio = false

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val notification = Notification.Builder(this, SilentApp.CALL_CHANNEL)
            .setSmallIcon(R.drawable.ic_silent_mono)
            .setContentTitle("Sharing your screen")
            .setContentText("Only people in your Silent room can see it.")
            .setOngoing(true)
            .build()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION)
        } else {
            startForeground(ID, notification)
        }

        val resultCode = intent?.getIntExtra(EXTRA_RESULT, Activity.RESULT_CANCELED) ?: Activity.RESULT_CANCELED
        @Suppress("DEPRECATION")
        val data: Intent? = intent?.getParcelableExtra(EXTRA_DATA)
        if (resultCode != Activity.RESULT_OK || data == null) {
            stopSelf()
            return START_NOT_STICKY
        }
        startCapture(resultCode, data, intent.getBooleanExtra(EXTRA_AUDIO, false))
        return START_NOT_STICKY
    }

    private fun startCapture(resultCode: Int, data: Intent, withAudio: Boolean) {
        val manager = getSystemService(MediaProjectionManager::class.java)
        val projection = manager.getMediaProjection(resultCode, data) ?: return stopSelf()
        this.projection = projection
        projection.registerCallback(object : MediaProjection.Callback() {
            override fun onStop() {
                ActiveCall.room?.setScreen(false)
                stopSelf()
            }
        }, null)

        // Capture at most 1920 on the long edge; the engine encodes from there.
        val metrics = resources.displayMetrics
        val scale = minOf(1f, MAX_LONG_EDGE / max(metrics.widthPixels, metrics.heightPixels).toFloat())
        val width = ((metrics.widthPixels * scale).roundToInt()) and 1.inv()
        val height = ((metrics.heightPixels * scale).roundToInt()) and 1.inv()

        val thread = HandlerThread("silent-screen").also { it.start() }
        this.thread = thread
        val reader = ImageReader.newInstance(width, height, PixelFormat.RGBA_8888, 3)
        this.reader = reader
        var buffer = ByteArray(0)
        reader.setOnImageAvailableListener({ r ->
            val image = r.acquireLatestImage() ?: return@setOnImageAvailableListener
            try {
                val plane = image.planes[0]
                val bytes = plane.buffer
                if (buffer.size != bytes.remaining()) buffer = ByteArray(bytes.remaining())
                bytes.get(buffer)
                ActiveCall.room?.pushScreenFrame(
                    image.width.toUInt(), image.height.toUInt(), plane.rowStride.toUInt(), buffer,
                )
            } catch (e: Exception) {
                Log.w("silent", "screen frame dropped", e)
            } finally {
                image.close()
            }
        }, Handler(thread.looper))

        display = projection.createVirtualDisplay(
            "silent-screen", width, height, metrics.densityDpi,
            DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR, reader.surface, null, null,
        )
        ActiveCall.room?.setScreen(true)
        if (withAudio && Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) startAudio(projection)
    }

    /** Media, game and unknown sounds; calls (Silent's own audio) aren't captured. */
    @SuppressLint("NewApi")
    private fun startAudio(projection: MediaProjection) {
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) return
        val config = AudioPlaybackCaptureConfiguration.Builder(projection)
            .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
            .addMatchingUsage(AudioAttributes.USAGE_GAME)
            .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
            .build()
        val format = AudioFormat.Builder()
            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
            .setSampleRate(RATE)
            .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
            .build()
        val minimum = AudioRecord.getMinBufferSize(RATE, AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_16BIT)
        val record = try {
            AudioRecord.Builder()
                .setAudioFormat(format)
                .setBufferSizeInBytes(max(minimum, RATE * 4 / 5))
                .setAudioPlaybackCaptureConfig(config)
                .build()
        } catch (e: Exception) {
            Log.w("silent", "phone audio capture unavailable", e)
            return
        }
        audio = record
        capturingAudio = true
        record.startRecording()
        audioThread = thread(name = "silent-screen-audio") {
            // 10 ms of 16-bit stereo.
            val chunk = ByteArray(RATE / 100 * 4)
            while (capturingAudio) {
                val n = record.read(chunk, 0, chunk.size)
                if (n > 0) ActiveCall.room?.pushScreenAudio(chunk.copyOf(n))
                if (n < 0) break
            }
        }
    }

    override fun onDestroy() {
        capturingAudio = false
        // Stopping unblocks a waiting read, so the thread can finish first.
        audio?.let { runCatching { it.stop() } }
        audioThread?.join(500)
        audio?.release()
        display?.release()
        reader?.close()
        projection?.stop()
        thread?.quitSafely()
        ActiveCall.room?.setScreen(false)
        Handler(mainLooper).post { ActiveCall.controls?.screenStopped() }
        super.onDestroy()
    }

    companion object {
        private const val ID = 2
        private const val EXTRA_RESULT = "result"
        private const val EXTRA_DATA = "data"
        private const val EXTRA_AUDIO = "audio"
        private const val RATE = 48_000
        private const val MAX_LONG_EDGE = 1920f

        fun start(context: Context, resultCode: Int, data: Intent, withAudio: Boolean) {
            val intent = Intent(context, ScreenShareService::class.java)
                .putExtra(EXTRA_RESULT, resultCode)
                .putExtra(EXTRA_DATA, data)
                .putExtra(EXTRA_AUDIO, withAudio)
            context.startForegroundService(intent)
        }

        fun stop(context: Context) =
            context.stopService(Intent(context, ScreenShareService::class.java))
    }
}
