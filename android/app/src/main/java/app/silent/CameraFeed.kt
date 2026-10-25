package app.silent

import android.content.Context
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CaptureRequest
import android.util.Log
import android.util.Range
import android.util.Size
import androidx.annotation.OptIn
import androidx.camera.camera2.interop.Camera2CameraInfo
import androidx.camera.camera2.interop.Camera2Interop
import androidx.camera.camera2.interop.ExperimentalCamera2Interop
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

    /** [onFailure] runs on the main thread when no camera could start. */
    fun start(owner: LifecycleOwner, high: Boolean, front: Boolean, onFailure: () -> Unit) {
        val future = ProcessCameraProvider.getInstance(context)
        future.addListener({
            try {
                val provider = future.get()
                this.provider = provider
                val wanted = if (front) CameraSelector.DEFAULT_FRONT_CAMERA else CameraSelector.DEFAULT_BACK_CAMERA
                val other = if (front) CameraSelector.DEFAULT_BACK_CAMERA else CameraSelector.DEFAULT_FRONT_CAMERA
                val camera = if (provider.hasCamera(wanted)) wanted else other
                val size = if (high) Size(1920, 1080) else Size(1280, 720)
                val selector = ResolutionSelector.Builder()
                    .setResolutionStrategy(
                        ResolutionStrategy(size, ResolutionStrategy.FALLBACK_RULE_CLOSEST_LOWER_THEN_HIGHER),
                    )
                    .build()
                val analysisBuilder = ImageAnalysis.Builder()
                    .setResolutionSelector(selector)
                    .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                    .setOutputImageFormat(ImageAnalysis.OUTPUT_IMAGE_FORMAT_YUV_420_888)
                if (high) fps60(provider, camera)?.let { fps(analysisBuilder, it) }
                val analysis = analysisBuilder.build().also { it.setAnalyzer(executor, ::onFrame) }
                val previewCase = Preview.Builder().build().also { it.surfaceProvider = preview.surfaceProvider }
                provider.unbindAll()
                provider.bindToLifecycle(owner, camera, previewCase, analysis)
            } catch (e: Exception) {
                Log.e("silent", "camera failed to start", e)
                onFailure()
            }
        }, ContextCompat.getMainExecutor(context))
    }

    /** A frame rate range reaching 60 fps (and down to 30 in low light), if the camera has one. */
    @OptIn(ExperimentalCamera2Interop::class)
    private fun fps60(provider: ProcessCameraProvider, camera: CameraSelector): Range<Int>? {
        val info = camera.filter(provider.availableCameraInfos).firstOrNull() ?: return null
        val ranges = Camera2CameraInfo.from(info)
            .getCameraCharacteristic(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES)
            ?: return null
        return ranges.filter { it.upper >= 60 && it.lower <= 30 }.minByOrNull { it.lower }
    }

    @OptIn(ExperimentalCamera2Interop::class)
    private fun fps(builder: ImageAnalysis.Builder, range: Range<Int>) {
        Camera2Interop.Extender(builder).setCaptureRequestOption(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, range)
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
