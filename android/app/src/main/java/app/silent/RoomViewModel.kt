package app.silent

import android.app.Application
import android.graphics.BitmapFactory
import android.os.Handler
import android.os.Looper
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import app.silent.ffi.RoomListener
import app.silent.ffi.RoomSettings
import app.silent.ffi.RoomUpdate
import app.silent.ffi.SilentException
import app.silent.ffi.SilentRoom
import app.silent.ffi.checkCode
import app.silent.ffi.createRoom
import app.silent.ffi.joinRoom
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.util.concurrent.ConcurrentHashMap
import kotlin.concurrent.thread

data class Friend(
    val id: UInt,
    val name: String = "Friend",
    val status: String = "Connecting…",
    val verified: Boolean = false,
    val mic: Boolean = true,
    val camera: Boolean = false,
    val screen: Boolean = false,
    val words: String? = null,
    val mutedForMe: Boolean = false,
    /** You compared safety words with them and they matched. */
    val confirmed: Boolean = false,
)

data class ChatLine(
    val who: String,
    val text: String,
    val time: String = java.time.LocalTime.now().format(java.time.format.DateTimeFormatter.ofPattern("HH:mm")),
    val mine: Boolean = false,
    val system: Boolean = false,
    /** Set for a file line; the key to save it with. */
    val file: String? = null,
    val fileReady: Boolean = false,
    val image: ImageBitmap? = null,
)

/** Files can be up to this big (the engine's limit). */
const val MAX_FILE_BYTES = 64L * 1024 * 1024

fun sizeText(bytes: Long): String = when {
    bytes < 1024 -> "$bytes B"
    bytes < 1024 * 1024 -> "${bytes / 1024} KB"
    else -> "%.1f MB".format(bytes / 1024.0 / 1024.0)
}

class RoomViewModel(app: Application) : AndroidViewModel(app) {
    private val main = Handler(Looper.getMainLooper())

    // Home screen
    var name by mutableStateOf("")
    var joinCode by mutableStateOf("")
    var relayUrl by mutableStateOf(BuildConfig.RELAY_URL)
    var relayOnly by mutableStateOf(false)
    var cameraHigh by mutableStateOf(false)
    var screenDetail by mutableStateOf(false)
    var busy by mutableStateOf(false)
    var error by mutableStateOf("")

    // Room
    var room by mutableStateOf<SilentRoom?>(null)
        private set
    var code by mutableStateOf("")
        private set
    var created = false
        private set
    var micOn by mutableStateOf(true)
        private set
    var cameraOn by mutableStateOf(false)
        private set
    var frontCamera by mutableStateOf(true)
    var speakerOn by mutableStateOf(true)
        private set
    var screenOn by mutableStateOf(false)
    var notice by mutableStateOf("")
        private set
    val friends = mutableStateMapOf<UInt, Friend>()
    /** Who is talking now; includes [me] for yourself. */
    var speaking by mutableStateOf<Set<UInt>>(emptySet())
        private set
    var me: UInt = 0u
        private set
    val chat = mutableStateListOf<ChatLine>()
    var stats by mutableStateOf<List<String>>(emptyList())
        private set

    private val slots = ConcurrentHashMap<TileKey, FrameSlot>()
    /** Received files, in memory only, until the room is left. */
    private val files = mutableMapOf<String, Pair<String, ByteArray>>()
    private var sentFiles = 0
    fun slot(key: TileKey): FrameSlot = slots.getOrPut(key) { FrameSlot() }

    /**
     * One per room. Once the room is left, late callbacks from it are
     * dropped so they can't leak into the next room's state.
     */
    private inner class Listener : RoomListener {
        @Volatile
        var active = true

        override fun onUpdate(update: RoomUpdate) {
            if (active) main.post { if (active) apply(update) }
        }

        override fun onVideoFrame(peer: UInt, screen: Boolean, width: UInt, height: UInt, rgba: ByteArray) {
            if (!active) return
            val slot = slot(TileKey(peer, screen))
            val image = synchronized(slot) { slot.fill(width.toInt(), height.toInt(), rgba) }
            main.post { if (active) slot.image.value = image }
        }
    }

    private var listener: Listener? = null

    private fun settings() = RoomSettings(
        relayUrl = relayUrl.trim(),
        name = name,
        cameraHigh = cameraHigh,
        screenDetail = screenDetail,
        relayOnly = relayOnly,
        useAudio = true,
    )

    fun create() = open(null)

    fun join() {
        checkCode(joinCode)?.let {
            error = "That doesn't look like a Silent code: $it."
            return
        }
        open(joinCode.trim())
    }

    private fun open(code: String?) {
        if (busy || room != null) return
        if (relayUrl.isBlank()) {
            error = "Set a relay server under Settings first."
            return
        }
        busy = true
        error = ""
        val listener = Listener()
        viewModelScope.launch {
            val result = withContext(Dispatchers.IO) {
                runCatching {
                    if (code == null) createRoom(settings(), listener) else joinRoom(code, settings(), listener)
                }
            }
            busy = false
            result.onSuccess { r ->
                this@RoomViewModel.listener = listener
                slots.clear()
                notice = ""
                stats = emptyList()
                created = code == null
                room = r
                ActiveCall.room = r
                this@RoomViewModel.code = r.code()
                me = r.me()
                speaking = emptySet()
                micOn = true
                cameraOn = false
                screenOn = false
                friends.clear()
                chat.clear()
                ActiveCall.controls = controls
                speakerOn = true
                ActiveCall.speaker = true
                if (created) system("Room created. Share the code with up to three friends.")
                CallService.start(getApplication())
            }.onFailure { error = describe(it) }
        }
    }

    private fun describe(e: Throwable): String = when (e) {
        is SilentException.InvalidCode -> e.reason
        is SilentException.NoSuchRoom -> "No room is open with that number. Ask your friend for a fresh code."
        is SilentException.RoomFull -> "That room already has four people."
        is SilentException.TooManyJoins -> "Too many attempts to join that room. Wait a minute and try again."
        is SilentException.Network -> "Couldn't reach the relay (${e.reason})."
        else -> e.message ?: e.toString()
    }

    private fun system(text: String) = chat.add(ChatLine("", text, system = true))

    private fun apply(update: RoomUpdate) {
        when (update) {
            is RoomUpdate.PeerJoining -> friends[update.peer] = Friend(update.peer)
            is RoomUpdate.PeerVerified -> {
                val returning = update.replaces?.let { old ->
                    slots.remove(TileKey(old, false))
                    slots.remove(TileKey(old, true))
                    friends.remove(old)
                }
                if (returning == null) system("${update.name} joined")
                friends[update.peer]?.let {
                    friends[update.peer] = it.copy(
                        name = update.name, status = "Securing connection…", verified = true,
                        mutedForMe = returning?.mutedForMe ?: false,
                    )
                }
            }
            is RoomUpdate.PeerWrongCode -> {
                friends.remove(update.peer)
                if (created) {
                    system("Someone tried to join with the wrong code and was kept out.")
                } else if (friends.isEmpty()) {
                    leave("That code doesn't match the room. Check the words and try again.")
                }
            }
            is RoomUpdate.PeerConnected -> friends[update.peer]?.let {
                friends[update.peer] = it.copy(status = "", words = update.safetyWords.joinToString(" "))
            }
            is RoomUpdate.PeerConnectionLost -> friends[update.peer]?.let {
                friends[update.peer] = it.copy(status = "Reconnecting…")
            }
            is RoomUpdate.PeerLeft -> {
                friends.remove(update.peer)?.takeIf { it.verified }?.let { system("${it.name} left") }
                slots.remove(TileKey(update.peer, false))
                slots.remove(TileKey(update.peer, true))
            }
            is RoomUpdate.PeerMedia -> friends[update.peer]?.let {
                friends[update.peer] = it.copy(mic = update.mic, camera = update.camera, screen = update.screen)
            }
            is RoomUpdate.Chat -> {
                val who = friends[update.from]?.name ?: "Friend"
                chat.add(ChatLine(who, update.text))
            }
            is RoomUpdate.LocalMediaError -> {
                if (update.screen) screenOn = false else cameraOn = false
                system(update.reason)
            }
            is RoomUpdate.RelayLost -> notice = "Relay offline: the call continues, reconnecting…"
            is RoomUpdate.RelayRestored -> if (notice.startsWith("Relay")) notice = ""
            is RoomUpdate.Speaking -> speaking = update.peers.toSet()
            is RoomUpdate.Removed -> leave("You were removed from the room: your code doesn't match. Check the words and try again.")
            is RoomUpdate.FileIncoming -> chat.add(
                ChatLine(
                    friends[update.from]?.name ?: "Friend",
                    "File: ${update.name} (${sizeText(update.size.toLong())}) · receiving…",
                    file = "${update.from}:${update.id}",
                ),
            )
            is RoomUpdate.FileReceived -> {
                val key = "${update.from}:${update.id}"
                files[key] = update.name to update.data
                updateFileLine(key) {
                    it.copy(text = "File: ${update.name} (${sizeText(update.data.size.toLong())})", fileReady = true)
                }
                showPreview(key, update.data)
            }
            is RoomUpdate.FileFailed -> updateFileLine("${update.from}:${update.id}") {
                it.copy(text = it.text.removeSuffix(" · receiving…") + " · didn't arrive")
            }
        }
    }

    private val controls = object : CallControls {
        override val micOn get() = this@RoomViewModel.micOn
        override fun toggleMic() = this@RoomViewModel.toggleMic()
        override fun leave() = this@RoomViewModel.leave()
        override fun screenStopped() {
            screenOn = false
        }
    }

    fun toggleMic() {
        micOn = !micOn
        room?.setMic(micOn)
        CallService.refresh(getApplication())
    }

    private fun updateFileLine(key: String, change: (ChatLine) -> ChatLine) {
        val i = chat.indexOfLast { it.file == key }
        if (i >= 0) chat[i] = change(chat[i])
    }

    /** Decodes a small preview when the file is an image. */
    private fun showPreview(key: String, data: ByteArray) {
        viewModelScope.launch {
            val image = withContext(Dispatchers.Default) {
                val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
                BitmapFactory.decodeByteArray(data, 0, data.size, bounds)
                if (bounds.outWidth <= 0) return@withContext null
                var sample = 1
                while (bounds.outWidth / sample > 1024 || bounds.outHeight / sample > 1024) sample *= 2
                val options = BitmapFactory.Options().apply { inSampleSize = sample }
                BitmapFactory.decodeByteArray(data, 0, data.size, options)?.asImageBitmap()
            } ?: return@launch
            updateFileLine(key) { it.copy(image = image) }
        }
    }

    fun sendFile(fileName: String, data: ByteArray) {
        val r = room ?: return
        try {
            r.sendFile(fileName, data)
        } catch (e: SilentException) {
            system("Couldn't send $fileName: ${e.message}")
            return
        }
        sentFiles += 1
        val key = "me:$sentFiles"
        chat.add(
            ChatLine(
                name.ifBlank { "Me" }, "File: $fileName (${sizeText(data.size.toLong())})",
                mine = true, file = key, fileReady = true,
            ),
        )
        showPreview(key, data)
    }

    /** The name and bytes of a received file, to save. */
    fun file(key: String): Pair<String, ByteArray>? = files[key]

    fun notify(text: String) = system(text)

    fun toggleConfirmed(peer: UInt) {
        friends[peer]?.let { friends[peer] = it.copy(confirmed = !it.confirmed) }
    }

    /** Silences a friend for this phone only. */
    fun toggleMuteFriend(peer: UInt) {
        val friend = friends[peer] ?: return
        val muted = !friend.mutedForMe
        friends[peer] = friend.copy(mutedForMe = muted)
        room?.setFriendMuted(peer, muted)
    }

    fun toggleSpeaker() {
        speakerOn = !speakerOn
        CallService.setSpeaker(getApplication(), speakerOn)
    }

    fun flipCamera() {
        frontCamera = !frontCamera
    }

    fun setCamera(on: Boolean) {
        cameraOn = on
        room?.setCamera(on)
    }

    fun send(text: String) {
        val trimmed = text.trim()
        if (trimmed.isEmpty()) return
        room?.sendChat(trimmed)
        chat.add(ChatLine(name.ifBlank { "Me" }, trimmed, mine = true))
    }

    fun refreshStats() {
        val r = room ?: return
        viewModelScope.launch {
            val lines = withContext(Dispatchers.IO) {
                r.stats().flatMap { s ->
                    val who = friends[s.peer]?.name ?: "Friend"
                    val rtt = s.rttMs?.let { " (round trip ${it.toInt()} ms)" } ?: ""
                    listOf("$who$rtt") + s.lines.map { "    $it" }
                }
            }
            stats = lines.ifEmpty { listOf("No video flowing yet.") }
        }
    }

    fun leave(reason: String = "") {
        val r = room ?: return
        room = null
        ActiveCall.room = null
        ActiveCall.controls = null
        cameraOn = false
        screenOn = false
        ScreenShareService.stop(getApplication())
        CallService.stop(getApplication())
        listener?.active = false
        listener = null
        slots.clear()
        files.clear()
        error = reason
        // Not viewModelScope: when the ViewModel is cleared, that scope is
        // cancelled before onCleared runs, and the room would never be left.
        thread(name = "silent-leave") { r.leave() }
    }

    override fun onCleared() {
        leave()
        super.onCleared()
    }
}
