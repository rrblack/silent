package app.silent

import android.graphics.Bitmap
import androidx.compose.runtime.MutableState
import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import java.nio.ByteBuffer

/**
 * Latest video frame per tile. Frames are written on a background thread
 * into a small ring of reused bitmaps, so the UI never sees a bitmap that is
 * being filled and the garbage collector isn't fed 30 large arrays a second.
 */
class FrameSlot {
    val image: MutableState<ImageBitmap?> = mutableStateOf(null)
    private val ring = arrayOfNulls<Bitmap>(3)
    private var next = 0

    /** Fills the next bitmap; returns it for posting to the main thread. */
    fun fill(width: Int, height: Int, rgba: ByteArray): ImageBitmap {
        var bmp = ring[next]
        if (bmp == null || bmp.width != width || bmp.height != height) {
            bmp = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
            ring[next] = bmp
        }
        // ARGB_8888 bitmaps are R, G, B, A bytes in memory, matching the engine.
        bmp!!.copyPixelsFromBuffer(ByteBuffer.wrap(rgba))
        next = (next + 1) % ring.size
        return bmp.asImageBitmap()
    }
}

data class TileKey(val peer: UInt, val screen: Boolean)
