package app.silent

import android.content.Context
import android.util.Log
import android.util.Size
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.core.Preview
import androidx.camera.core.resolutionselector.ResolutionSelector
import androidx.camera.core.resolutionselector.ResolutionStrategy
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.core.content.ContextCompat
import androidx.lifecycle.LifecycleOwner
import java.util.concurrent.Executors

/**
 * Front or back camera through CameraX. Frames go to the room as YUV 4:2:0 with
 * their rotation, and to [preview] for the self view.
 */
class CameraFeed(private val context: Context) {
    val preview = PreviewView(context).apply {
        scaleType = PreviewView.ScaleType.FILL_CENTER
        implementationMode = PreviewView.ImplementationMode.COMPATIBLE
    }
    private val executor = Executors.newSingleThreadExecutor()
    private var provider: ProcessCameraProvider? = null
    private var y = ByteArray(0)
    private var u = ByteArray(0)
    private var v = ByteArray(0)

    fun start(owner: LifecycleOwner, high: Boolean, front: Boolean) {
        val future = ProcessCameraProvider.getInstance(context)
        future.addListener({
            val provider = future.get()
            this.provider = provider
            val size = if (high) Size(1920, 1080) else Size(1280, 720)
            val selector = ResolutionSelector.Builder()
                .setResolutionStrategy(
                    ResolutionStrategy(size, ResolutionStrategy.FALLBACK_RULE_CLOSEST_LOWER_THEN_HIGHER),
                )
                .build()
            val analysis = ImageAnalysis.Builder()
                .setResolutionSelector(selector)
                .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                .setOutputImageFormat(ImageAnalysis.OUTPUT_IMAGE_FORMAT_YUV_420_888)
                .build()
                .also { it.setAnalyzer(executor, ::onFrame) }
            val previewCase = Preview.Builder().build().also { it.surfaceProvider = preview.surfaceProvider }
            val wanted = if (front) CameraSelector.DEFAULT_FRONT_CAMERA else CameraSelector.DEFAULT_BACK_CAMERA
            val other = if (front) CameraSelector.DEFAULT_BACK_CAMERA else CameraSelector.DEFAULT_FRONT_CAMERA
            val camera = if (provider.hasCamera(wanted)) wanted else other
            try {
                provider.unbindAll()
                provider.bindToLifecycle(owner, camera, previewCase, analysis)
            } catch (e: Exception) {
                Log.e("silent", "camera failed to start", e)
            }
        }, ContextCompat.getMainExecutor(context))
    }

    fun stop() {
        provider?.unbindAll()
    }

    private fun onFrame(image: ImageProxy) {
        try {
            val room = ActiveCall.room ?: return
            val (py, pu, pv) = image.planes
            y = copy(py.buffer, y)
            u = copy(pu.buffer, u)
            v = copy(pv.buffer, v)
            room.pushCameraFrame(
                image.width.toUInt(), image.height.toUInt(), y, u, v,
                py.rowStride.toUInt(), pu.rowStride.toUInt(), pu.pixelStride.toUInt(),
                image.imageInfo.rotationDegrees.toUInt(),
            )
        } catch (e: Exception) {
            Log.w("silent", "camera frame dropped", e)
        } finally {
            image.close()
        }
    }

    private fun copy(src: java.nio.ByteBuffer, reuse: ByteArray): ByteArray {
        val out = if (reuse.size == src.remaining()) reuse else ByteArray(src.remaining())
        src.get(out)
        return out
    }
}
