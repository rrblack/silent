package app.silent

import android.app.Application
import android.os.Handler
import android.os.Looper
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
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

data class Friend(
    val id: UInt,
    val name: String = "Friend",
    val status: String = "Connecting…",
    val verified: Boolean = false,
    val mic: Boolean = true,
    val camera: Boolean = false,
    val screen: Boolean = false,
    val words: String? = null,
)

data class ChatLine(val who: String, val text: String, val mine: Boolean = false, val system: Boolean = false)

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
    var screenOn by mutableStateOf(false)
    var notice by mutableStateOf("")
        private set
    val friends = mutableStateMapOf<UInt, Friend>()
    val chat = mutableStateListOf<ChatLine>()
    var stats by mutableStateOf<List<String>>(emptyList())
        private set

    private val slots = ConcurrentHashMap<TileKey, FrameSlot>()
    fun slot(key: TileKey): FrameSlot = slots.getOrPut(key) { FrameSlot() }

    private val listener = object : RoomListener {
        override fun onUpdate(update: RoomUpdate) {
            main.post { apply(update) }
        }

        override fun onVideoFrame(peer: UInt, screen: Boolean, width: UInt, height: UInt, rgba: ByteArray) {
            val slot = slot(TileKey(peer, screen))
            val image = synchronized(slot) { slot.fill(width.toInt(), height.toInt(), rgba) }
            main.post { slot.image.value = image }
        }
    }

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
        if (relayUrl.isBlank()) {
            error = "Set a relay server under Settings first."
            return
        }
        busy = true
        error = ""
        viewModelScope.launch {
            val result = withContext(Dispatchers.IO) {
                runCatching {
                    if (code == null) createRoom(settings(), listener) else joinRoom(code, settings(), listener)
                }
            }
            busy = false
            result.onSuccess { r ->
                created = code == null
                room = r
                ActiveCall.room = r
                this@RoomViewModel.code = r.code()
                micOn = true
                cameraOn = false
                screenOn = false
                friends.clear()
                chat.clear()
                if (created) system("Room created. Share the code with up to three friends.")
                CallService.start(getApplication())
            }.onFailure { error = describe(it) }
        }
    }

    private fun describe(e: Throwable): String = when (e) {
        is SilentException.InvalidCode -> e.reason
        is SilentException.NoSuchRoom -> "No room is open with that number. Ask your friend for a fresh code."
        is SilentException.RoomFull -> "That room already has four people."
        is SilentException.Network -> "Couldn't reach the relay (${e.reason})."
        else -> e.message ?: e.toString()
    }

    private fun system(text: String) = chat.add(ChatLine("", text, system = true))

    private fun apply(update: RoomUpdate) {
        when (update) {
            is RoomUpdate.PeerJoining -> friends[update.peer] = Friend(update.peer)
            is RoomUpdate.PeerVerified -> {
                system("${update.name} joined")
                friends[update.peer]?.let {
                    friends[update.peer] = it.copy(name = update.name, status = "Securing connection…", verified = true)
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
                friends[update.peer] = it.copy(status = "Connection lost")
            }
            is RoomUpdate.PeerLeft -> {
                friends.remove(update.peer)?.let { system("${it.name} left") }
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
            is RoomUpdate.RelayLost -> notice = "Relay offline: the call continues, nobody new can join"
        }
    }

    fun toggleMic() {
        micOn = !micOn
        room?.setMic(micOn)
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
        cameraOn = false
        screenOn = false
        ScreenShareService.stop(getApplication())
        CallService.stop(getApplication())
        slots.clear()
        error = reason
        viewModelScope.launch(Dispatchers.IO) { r.leave() }
    }

    override fun onCleared() {
        leave()
        super.onCleared()
    }
}
