package app.silent

import android.app.Activity
import android.app.Notification
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.media.ImageReader
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.util.Log
import kotlin.math.max
import kotlin.math.roundToInt

/**
 * Captures the screen with MediaProjection and pushes RGBA frames into the
 * room. Android requires this to run as a mediaProjection foreground service.
 */
class ScreenShareService : Service() {
    private var projection: MediaProjection? = null
    private var display: VirtualDisplay? = null
    private var reader: ImageReader? = null
    private var thread: HandlerThread? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val notification = Notification.Builder(this, SilentApp.CALL_CHANNEL)
            .setSmallIcon(R.drawable.ic_silent)
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
        startCapture(resultCode, data)
        return START_NOT_STICKY
    }

    private fun startCapture(resultCode: Int, data: Intent) {
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
    }

    override fun onDestroy() {
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
        private const val MAX_LONG_EDGE = 1920f

        fun start(context: Context, resultCode: Int, data: Intent) {
            val intent = Intent(context, ScreenShareService::class.java)
                .putExtra(EXTRA_RESULT, resultCode)
                .putExtra(EXTRA_DATA, data)
            context.startForegroundService(intent)
        }

        fun stop(context: Context) =
            context.stopService(Intent(context, ScreenShareService::class.java))
    }
}
