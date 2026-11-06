package app.silent

import android.graphics.SurfaceTexture
import android.media.AudioAttributes
import android.media.MediaDataSource
import android.media.MediaPlayer
import android.view.Surface
import android.view.TextureView
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.viewinterop.AndroidView

/** Lets the phone's media framework read a file that only exists in memory. */
class BytesSource(private val data: ByteArray) : MediaDataSource() {
    override fun getSize(): Long = data.size.toLong()

    override fun readAt(position: Long, buffer: ByteArray, offset: Int, size: Int): Int {
        if (position >= data.size) return -1
        val count = minOf(size.toLong(), data.size - position).toInt()
        System.arraycopy(data, position.toInt(), buffer, offset, count)
        return count
    }

    override fun close() {}
}

/**
 * Plays a video from chat, straight from memory, with the phone's own decoders (H.264, HEVC and
 * whatever else it supports). Tapping pauses and resumes; [onEnd] runs when it finishes.
 */
@Composable
fun InlineVideo(data: ByteArray, paused: Boolean, onEnd: () -> Unit, modifier: Modifier = Modifier) {
    val prepared = remember(data) { booleanArrayOf(false) }
    val player = remember(data) {
        MediaPlayer().apply {
            setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_MEDIA)
                    .setContentType(AudioAttributes.CONTENT_TYPE_MOVIE)
                    .build(),
            )
            setDataSource(BytesSource(data))
        }
    }
    DisposableEffect(player) {
        player.setOnCompletionListener { onEnd() }
        player.setOnErrorListener { _, _, _ ->
            onEnd()
            true
        }
        player.setOnPreparedListener {
            prepared[0] = true
            if (!paused) it.start()
        }
        player.prepareAsync()
        onDispose { player.release() }
    }
    AndroidView(
        factory = { context ->
            TextureView(context).apply {
                surfaceTextureListener = object : TextureView.SurfaceTextureListener {
                    private var surface: Surface? = null

                    override fun onSurfaceTextureAvailable(texture: SurfaceTexture, width: Int, height: Int) {
                        surface = Surface(texture).also { runCatching { player.setSurface(it) } }
                    }

                    override fun onSurfaceTextureSizeChanged(texture: SurfaceTexture, width: Int, height: Int) {}

                    override fun onSurfaceTextureDestroyed(texture: SurfaceTexture): Boolean {
                        runCatching { player.setSurface(null) }
                        surface?.release()
                        surface = null
                        return true
                    }

                    override fun onSurfaceTextureUpdated(texture: SurfaceTexture) {}
                }
            }
        },
        update = {
            if (prepared[0]) {
                if (paused && player.isPlaying) player.pause()
                if (!paused && !player.isPlaying) player.start()
            }
        },
        modifier = modifier,
    )
}
