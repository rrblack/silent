package app.silent

import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.AttachFile
import androidx.compose.material.icons.filled.CallEnd
import androidx.compose.material.icons.filled.Cameraswitch
import androidx.compose.material.icons.filled.Chat
import androidx.compose.material.icons.filled.Hearing
import androidx.compose.material.icons.filled.VolumeUp
import androidx.compose.material.icons.filled.Mic
import androidx.compose.material.icons.filled.MicOff
import androidx.compose.material.icons.filled.ScreenShare
import androidx.compose.material.icons.filled.StopScreenShare
import androidx.compose.material.icons.filled.Videocam
import androidx.compose.material.icons.filled.VideocamOff
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import android.view.ViewGroup
import kotlinx.coroutines.delay

object Palette {
    val bg = Color(0xFF070707)
    val panel = Color(0xFF101010)
    val card = Color(0xFF171717)
    val border = Color(0xFF2A1A1C)
    val text = Color(0xFFF2F2F2)
    val subtext = Color(0xFFA3A3A3)
    val accent = Color(0xFFE11D2E)
    val accentDim = Color(0xFF4A0F16)
    val danger = Color(0xFF9B111E)
    val ok = Color(0xFF3FB67A)
}

@Composable
fun SilentTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = darkColorScheme(
            primary = Palette.accent,
            onPrimary = Color.White,
            background = Palette.bg,
            surface = Palette.panel,
            onSurface = Palette.text,
            onBackground = Palette.text,
            error = Palette.accent,
        ),
        content = content,
    )
}

@Composable
private fun fieldColors() = OutlinedTextFieldDefaults.colors(
    focusedBorderColor = Palette.accent,
    unfocusedBorderColor = Palette.border,
    cursorColor = Palette.accent,
    focusedTextColor = Palette.text,
    unfocusedTextColor = Palette.text,
    focusedContainerColor = Palette.card,
    unfocusedContainerColor = Palette.card,
)

@Composable
fun HomeScreen(vm: RoomViewModel) {
    var showSettings by remember { mutableStateOf(vm.relayUrl.isBlank()) }
    Column(
        Modifier
            .fillMaxSize()
            .background(Palette.bg)
            .safeDrawingPadding()
            .imePadding()
            .verticalScroll(rememberScrollState())
            .padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(14.dp),
    ) {
        Spacer(Modifier.height(40.dp))
        Text("Silent", color = Palette.text, fontSize = 44.sp, fontWeight = FontWeight.ExtraBold,
            modifier = Modifier.fillMaxWidth(), textAlign = TextAlign.Center)
        Text(
            "Encrypted calls, screen sharing and chat for up to four friends.\nNo accounts. Nothing stored.",
            color = Palette.subtext, fontSize = 14.sp, textAlign = TextAlign.Center,
            modifier = Modifier.fillMaxWidth(),
        )
        Spacer(Modifier.height(8.dp))
        OutlinedTextField(
            value = vm.name, onValueChange = { vm.name = it.take(32) },
            label = { Text("Your name") }, singleLine = true, colors = fieldColors(),
            modifier = Modifier.fillMaxWidth(),
        )
        Button(
            onClick = vm::create, enabled = !vm.busy,
            colors = ButtonDefaults.buttonColors(containerColor = Palette.accent),
            modifier = Modifier.fillMaxWidth().height(52.dp),
        ) { Text(if (vm.busy) "Connecting…" else "Start a room", fontWeight = FontWeight.SemiBold) }
        Text("or join a friend", color = Palette.subtext, fontSize = 12.sp,
            modifier = Modifier.fillMaxWidth(), textAlign = TextAlign.Center)
        Row(horizontalArrangement = Arrangement.spacedBy(10.dp), verticalAlignment = Alignment.CenterVertically) {
            OutlinedTextField(
                value = vm.joinCode, onValueChange = { vm.joinCode = it },
                placeholder = { Text("1234-word-word-word") }, singleLine = true, colors = fieldColors(),
                keyboardOptions = KeyboardOptions(imeAction = ImeAction.Go),
                keyboardActions = KeyboardActions(onGo = { vm.join() }),
                modifier = Modifier.weight(1f),
            )
            Button(
                onClick = vm::join, enabled = !vm.busy && vm.joinCode.isNotBlank(),
                colors = ButtonDefaults.buttonColors(containerColor = Palette.card),
                modifier = Modifier.height(56.dp),
            ) { Text("Join") }
        }
        if (vm.error.isNotEmpty()) {
            Text(vm.error, color = Palette.accent, fontSize = 13.sp, textAlign = TextAlign.Center,
                modifier = Modifier.fillMaxWidth())
        }
        Text(
            if (showSettings) "Hide settings" else "Settings", color = Palette.accent, fontSize = 13.sp,
            textAlign = TextAlign.Center,
            modifier = Modifier.fillMaxWidth().clickable { showSettings = !showSettings }.padding(8.dp),
        )
        if (showSettings) {
            OutlinedTextField(
                value = vm.relayUrl, onValueChange = { vm.relayUrl = it },
                label = { Text("Relay server") }, placeholder = { Text("http://192.168.1.23:8787") },
                singleLine = true, colors = fieldColors(), modifier = Modifier.fillMaxWidth(),
            )
            SettingSwitch("Hide my IP from friends (relay all media)", vm.relayOnly) { vm.relayOnly = it }
            SettingSwitch("HD camera (1080p, 60 fps)", vm.cameraHigh) { vm.cameraHigh = it }
            SettingSwitch("Sharp screen share (1440p, 30 fps)", vm.screenDetail) { vm.screenDetail = it }
        }
    }
}

@Composable
private fun SettingSwitch(label: String, checked: Boolean, onChange: (Boolean) -> Unit) {
    Row(verticalAlignment = Alignment.CenterVertically, modifier = Modifier.fillMaxWidth()) {
        Text(label, color = Palette.text, fontSize = 14.sp, modifier = Modifier.weight(1f))
        Switch(
            checked = checked, onCheckedChange = onChange,
            colors = SwitchDefaults.colors(checkedTrackColor = Palette.accent),
        )
    }
}

private sealed interface TileModel {
    val key: String

    data class Me(val name: String, val camera: Boolean, val mic: Boolean) : TileModel {
        override val key = "me"
    }

    data class Remote(val friend: Friend, val screen: Boolean) : TileModel {
        override val key = "${friend.id}:${if (screen) "screen" else "camera"}"
    }
}

@Composable
fun RoomScreen(
    vm: RoomViewModel,
    camera: CameraFeed,
    onShareScreen: () -> Unit,
    onStopScreen: () -> Unit,
    onAttach: () -> Unit,
    onSaveFile: (String) -> Unit,
) {
    var showChat by remember { mutableStateOf(false) }
    var showVerify by remember { mutableStateOf(false) }
    var showStats by remember { mutableStateOf(false) }
    val clipboard = LocalClipboardManager.current

    val friends = vm.friends.values.sortedBy { it.id }
    val tiles = buildList<TileModel> {
        add(TileModel.Me(vm.name.ifBlank { "You" }, vm.cameraOn, vm.micOn))
        friends.forEach { add(TileModel.Remote(it, screen = false)) }
        friends.filter { it.screen }.forEach { add(TileModel.Remote(it, screen = true)) }
    }

    Column(Modifier.fillMaxSize().background(Palette.bg).safeDrawingPadding().imePadding()) {
        Row(
            Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(Modifier.weight(1f).clickable { clipboard.setText(AnnotatedString(vm.code)) }) {
                Text("Room code · tap to copy", color = Palette.subtext, fontSize = 11.sp)
                Text(vm.code, color = Palette.text, fontSize = 16.sp, fontWeight = FontWeight.SemiBold)
            }
            val connected = friends.any { it.words != null }
            TextButton(onClick = { showStats = true }, enabled = connected) { Text("Stats", color = Palette.text) }
            TextButton(onClick = { showVerify = true }, enabled = connected) { Text("Verify", color = Palette.text) }
        }
        if (vm.notice.isNotEmpty()) {
            Text(vm.notice, color = Palette.subtext, fontSize = 12.sp, modifier = Modifier.padding(horizontal = 12.dp))
        }

        if (showChat) {
            ChatPanel(vm, Modifier.weight(1f), onAttach, onSaveFile)
        } else {
            LazyVerticalGrid(
                columns = GridCells.Fixed(if (tiles.size == 1) 1 else 2),
                modifier = Modifier.weight(1f).padding(horizontal = 8.dp),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                items(tiles, key = { it.key }, span = { tile ->
                    GridItemSpan(if (tile is TileModel.Remote && tile.screen) maxLineSpan else 1)
                }) { tile -> Tile(vm, camera, tile) }
            }
        }

        Row(
            Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 12.dp),
            horizontalArrangement = Arrangement.SpaceEvenly,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            ControlButton(if (vm.micOn) Icons.Filled.Mic else Icons.Filled.MicOff, !vm.micOn) { vm.toggleMic() }
            ControlButton(if (vm.cameraOn) Icons.Filled.Videocam else Icons.Filled.VideocamOff, vm.cameraOn) {
                vm.setCamera(!vm.cameraOn)
            }
            if (vm.cameraOn) {
                ControlButton(Icons.Filled.Cameraswitch, active = false) { vm.flipCamera() }
            }
            ControlButton(if (vm.screenOn) Icons.Filled.StopScreenShare else Icons.Filled.ScreenShare, vm.screenOn) {
                if (vm.screenOn) onStopScreen() else onShareScreen()
            }
            ControlButton(if (vm.speakerOn) Icons.Filled.VolumeUp else Icons.Filled.Hearing, active = false) {
                vm.toggleSpeaker()
            }
            ControlButton(Icons.Filled.Chat, showChat) { showChat = !showChat }
            ControlButton(Icons.Filled.CallEnd, active = false, danger = true) { vm.leave() }
        }
    }

    if (showVerify) {
        AlertDialog(
            onDismissRequest = { showVerify = false },
            confirmButton = { TextButton(onClick = { showVerify = false }) { Text("Done", color = Palette.accent) } },
            title = { Text("Check for eavesdroppers") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                    Text(
                        "Read your words aloud with each friend. If they match, your connection is end to end " +
                            "and nobody is listening in. If they differ, leave the room.",
                        color = Palette.subtext, fontSize = 13.sp,
                    )
                    friends.filter { it.words != null }.forEach {
                        Text(it.name, color = Palette.subtext, fontSize = 12.sp)
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Text(
                                it.words!!, color = Palette.text, fontSize = 18.sp, fontWeight = FontWeight.Bold,
                                modifier = Modifier.weight(1f),
                            )
                            TextButton(onClick = { vm.toggleConfirmed(it.id) }) {
                                Text(
                                    if (it.confirmed) "Verified" else "They match",
                                    color = if (it.confirmed) Palette.ok else Palette.accent,
                                )
                            }
                        }
                    }
                }
            },
            containerColor = Palette.panel,
        )
    }

    if (showStats) {
        LaunchedEffect(Unit) {
            while (true) {
                vm.refreshStats()
                delay(1000)
            }
        }
        AlertDialog(
            onDismissRequest = { showStats = false },
            confirmButton = { TextButton(onClick = { showStats = false }) { Text("Done", color = Palette.accent) } },
            title = { Text("Call quality") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    vm.stats.forEach { Text(it, color = Palette.text, fontSize = 12.sp) }
                }
            },
            containerColor = Palette.panel,
        )
    }
}

/** The call in a picture-in-picture window: the one tile that matters most. */
@Composable
fun PipScreen(vm: RoomViewModel, camera: CameraFeed) {
    val friends = vm.friends.values.sortedBy { it.id }
    val tile = friends.firstOrNull { it.screen }?.let { TileModel.Remote(it, screen = true) }
        ?: friends.firstOrNull { it.id in vm.speaking && it.camera }?.let { TileModel.Remote(it, screen = false) }
        ?: friends.firstOrNull { it.camera }?.let { TileModel.Remote(it, screen = false) }
        ?: friends.firstOrNull()?.let { TileModel.Remote(it, screen = false) }
        ?: TileModel.Me(vm.name.ifBlank { "You" }, vm.cameraOn, vm.micOn)
    Box(Modifier.fillMaxSize().background(Palette.bg), contentAlignment = Alignment.Center) {
        Tile(vm, camera, tile)
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun Tile(vm: RoomViewModel, camera: CameraFeed, tile: TileModel) {
    val screen = tile is TileModel.Remote && tile.screen
    val speaking = !screen && when (tile) {
        is TileModel.Me -> vm.me in vm.speaking
        is TileModel.Remote -> tile.friend.id in vm.speaking
    }
    val friend = (tile as? TileModel.Remote)?.friend?.takeIf { !screen }
    Box(
        Modifier
            .fillMaxWidth()
            .aspectRatio(if (screen) 16f / 9f else 3f / 4f)
            .clip(RoundedCornerShape(12.dp))
            .background(Palette.card)
            .border(
                if (speaking) 3.dp else 1.dp,
                if (speaking) Palette.ok else Palette.border,
                RoundedCornerShape(12.dp),
            )
            .then(
                // Long press silences a friend for you only.
                if (friend != null) {
                    Modifier.combinedClickable(onClick = {}, onLongClick = { vm.toggleMuteFriend(friend.id) })
                } else {
                    Modifier
                },
            ),
        contentAlignment = Alignment.Center,
    ) {
        val label: String
        when (tile) {
            is TileModel.Me -> {
                label = tile.name + " (you)" + if (tile.mic) "" else " · muted"
                if (tile.camera) {
                    AndroidView(
                        factory = {
                            (camera.preview.parent as? ViewGroup)?.removeView(camera.preview)
                            camera.preview
                        },
                        modifier = Modifier.fillMaxSize(),
                    )
                } else {
                    Avatar(tile.name, "")
                }
            }
            is TileModel.Remote -> {
                val f = tile.friend
                label = f.name + (if (tile.screen) "'s screen" else "") +
                    (if (!tile.screen && !f.mic) " · muted" else "") +
                    (if (!tile.screen && f.mutedForMe) " · muted for you" else "") +
                    (if (!tile.screen && f.confirmed) " · verified" else "")
                val image = vm.slot(TileKey(f.id, tile.screen)).image.value
                val showing = if (tile.screen) f.screen else f.camera
                if (showing && image != null) {
                    Image(
                        bitmap = image, contentDescription = label,
                        contentScale = if (tile.screen) ContentScale.Fit else ContentScale.Crop,
                        modifier = Modifier.fillMaxSize(),
                    )
                } else {
                    Avatar(f.name, f.status)
                }
            }
        }
        Text(
            label, color = Palette.text, fontSize = 11.sp,
            modifier = Modifier
                .align(Alignment.BottomStart)
                .padding(8.dp)
                .background(Color(0xB0000000), RoundedCornerShape(10.dp))
                .padding(horizontal = 8.dp, vertical = 3.dp),
        )
    }
}

@Composable
private fun Avatar(name: String, status: String) {
    Column(horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Box(Modifier.size(64.dp).clip(CircleShape).background(Palette.accentDim), contentAlignment = Alignment.Center) {
            Text(name.take(1).uppercase(), color = Palette.text, fontSize = 26.sp, fontWeight = FontWeight.Bold)
        }
        if (status.isNotEmpty()) Text(status, color = Palette.subtext, fontSize = 12.sp)
    }
}

@Composable
private fun ControlButton(icon: ImageVector, active: Boolean, danger: Boolean = false, onClick: () -> Unit) {
    FilledIconButton(
        onClick = onClick,
        modifier = Modifier.size(46.dp),
        colors = IconButtonDefaults.filledIconButtonColors(
            containerColor = when {
                danger -> Palette.danger
                active -> Palette.accent
                else -> Palette.card
            },
            contentColor = Color.White,
        ),
    ) { Icon(icon, contentDescription = null) }
}

@Composable
private fun ChatPanel(vm: RoomViewModel, modifier: Modifier, onAttach: () -> Unit, onSaveFile: (String) -> Unit) {
    var draft by remember { mutableStateOf("") }
    val listState = rememberLazyListState()
    LaunchedEffect(vm.chat.size) {
        if (vm.chat.isNotEmpty()) listState.animateScrollToItem(vm.chat.size - 1)
    }
    Column(modifier.padding(horizontal = 12.dp)) {
        Text("End-to-end encrypted. Gone when you leave.", color = Palette.subtext, fontSize = 11.sp)
        LazyColumn(Modifier.weight(1f), state = listState, verticalArrangement = Arrangement.spacedBy(8.dp)) {
            items(vm.chat) { line ->
                if (line.system) {
                    Text(line.text, color = Palette.subtext, fontSize = 12.sp, fontStyle = FontStyle.Italic)
                } else {
                    Column {
                        Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                            Text(line.who, color = if (line.mine) Palette.accent else Palette.text,
                                fontSize = 12.sp, fontWeight = FontWeight.Bold)
                            Text(line.time, color = Palette.subtext, fontSize = 11.sp)
                        }
                        Text(line.text, color = Palette.text, fontSize = 14.sp)
                        line.image?.let {
                            Image(
                                bitmap = it, contentDescription = line.text,
                                modifier = Modifier.padding(top = 4.dp).size(220.dp).clip(RoundedCornerShape(8.dp)),
                                contentScale = ContentScale.Fit,
                            )
                        }
                        val key = line.file
                        if (key != null && line.fileReady && !line.mine) {
                            TextButton(onClick = { onSaveFile(key) }) { Text("Save…", color = Palette.accent) }
                        }
                    }
                }
            }
        }
        Row(verticalAlignment = Alignment.CenterVertically) {
            OutlinedTextField(
                value = draft, onValueChange = { draft = it }, placeholder = { Text("Message") },
                singleLine = true, colors = fieldColors(),
                keyboardOptions = KeyboardOptions(imeAction = ImeAction.Send),
                keyboardActions = KeyboardActions(onSend = {
                    vm.send(draft)
                    draft = ""
                }),
                modifier = Modifier.weight(1f).padding(vertical = 8.dp),
            )
            ControlButton(Icons.Filled.AttachFile, active = false, onClick = onAttach)
        }
    }
}
