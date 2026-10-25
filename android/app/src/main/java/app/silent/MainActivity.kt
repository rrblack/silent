package app.silent

import android.Manifest
import android.app.Activity
import android.app.PictureInPictureParams
import android.content.res.Configuration
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.net.Uri
import android.os.Bundle
import android.provider.OpenableColumns
import android.util.Rational
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.activity.viewModels
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.core.content.ContextCompat
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

class MainActivity : ComponentActivity() {
    private val vm: RoomViewModel by viewModels()
    private lateinit var camera: CameraFeed

    private val permissions = registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) { }

    private val screenConsent = registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
        val data = result.data
        if (result.resultCode == Activity.RESULT_OK && data != null) {
            vm.screenOn = true
            ScreenShareService.start(this, result.resultCode, data)
        }
    }

    /** Bumped when the camera permission is granted, to start the camera then. */
    private var cameraAllowed by mutableStateOf(0)

    private val cameraPermission = registerForActivityResult(ActivityResultContracts.RequestPermission()) { ok ->
        if (ok) {
            cameraAllowed++
        } else if (vm.cameraOn) {
            vm.setCamera(false)
            vm.notify("Silent can't use the camera. Allow it in Settings → Apps → Silent.")
        }
    }

    private val pickFile = registerForActivityResult(ActivityResultContracts.GetContent()) { uri ->
        if (uri != null) sendFile(uri)
    }

    private var inPip by mutableStateOf(false)

    /** Picture-in-picture shows the call in a small window over other apps. */
    private fun pipParams(): PictureInPictureParams = PictureInPictureParams.Builder()
        .setAspectRatio(Rational(3, 4))
        .apply { if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) setAutoEnterEnabled(vm.room != null) }
        .build()

    private fun enterPip(): Boolean = runCatching { enterPictureInPictureMode(pipParams()) }.getOrDefault(false)

    override fun onUserLeaveHint() {
        super.onUserLeaveHint()
        // Android 12+ enters by itself (setAutoEnterEnabled).
        if (vm.room != null && Build.VERSION.SDK_INT < Build.VERSION_CODES.S) enterPip()
    }

    override fun onPictureInPictureModeChanged(isInPictureInPictureMode: Boolean, newConfig: Configuration) {
        super.onPictureInPictureModeChanged(isInPictureInPictureMode, newConfig)
        inPip = isInPictureInPictureMode
    }

    /** The file waiting for a place to be saved. */
    private var saving: Pair<String, ByteArray>? = null

    private val saveFile = registerForActivityResult(ActivityResultContracts.CreateDocument("*/*")) { uri ->
        val (name, data) = saving ?: return@registerForActivityResult
        saving = null
        if (uri == null) return@registerForActivityResult
        lifecycleScope.launch {
            val ok = withContext(Dispatchers.IO) {
                runCatching { contentResolver.openOutputStream(uri)?.use { it.write(data) } != null }.getOrDefault(false)
            }
            vm.notify(if (ok) "Saved $name." else "Couldn't save $name.")
        }
    }

    private fun sendFile(uri: Uri) {
        lifecycleScope.launch {
            val file = withContext(Dispatchers.IO) {
                runCatching {
                    val name = contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
                        ?.use { c -> if (c.moveToFirst()) c.getString(0) else null } ?: "file"
                    val data = contentResolver.openInputStream(uri)?.use(::readCapped)
                    name to data
                }.getOrNull()
            }
            when {
                file == null -> vm.notify("Couldn't read that file.")
                file.second == null -> vm.notify("${file.first} is too big: files can be up to 64 MB.")
                else -> vm.sendFile(file.first, file.second!!)
            }
        }
    }

    /** The whole stream, or null when it is over the file size limit. */
    private fun readCapped(input: java.io.InputStream): ByteArray? {
        val out = java.io.ByteArrayOutputStream()
        val buffer = ByteArray(64 * 1024)
        while (true) {
            val n = input.read(buffer)
            if (n < 0) return out.toByteArray()
            out.write(buffer, 0, n)
            if (out.size() > MAX_FILE_BYTES) return null
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        camera = CameraFeed(this)
        requestPermissions()

        setContent {
            SilentTheme {
                val inRoom = vm.room != null
                LaunchedEffect(vm.cameraOn, vm.frontCamera, inRoom, cameraAllowed) {
                    if (vm.cameraOn && inRoom && granted(Manifest.permission.CAMERA)) {
                        camera.start(this@MainActivity, vm.cameraHigh, vm.frontCamera) {
                            if (vm.cameraOn) {
                                vm.setCamera(false)
                                vm.notify("The camera couldn't start.")
                            }
                        }
                    } else {
                        camera.stop()
                        if (vm.cameraOn && inRoom) cameraPermission.launch(Manifest.permission.CAMERA)
                    }
                }
                LaunchedEffect(inRoom) {
                    runCatching { setPictureInPictureParams(pipParams()) }
                    if (inRoom) {
                        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
                    } else {
                        window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
                    }
                }
                // Back leaves the app, not the call: it shrinks to picture-in-picture,
                // or keeps running in the background.
                BackHandler(enabled = inRoom) { if (!enterPip()) moveTaskToBack(true) }
                DisposableEffect(Unit) { onDispose { camera.stop() } }
                if (inRoom && inPip) {
                    PipScreen(vm, camera)
                } else if (inRoom) {
                    RoomScreen(
                        vm, camera,
                        onShareScreen = {
                            val manager = getSystemService(MediaProjectionManager::class.java)
                            screenConsent.launch(manager.createScreenCaptureIntent())
                        },
                        onStopScreen = {
                            vm.screenOn = false
                            ScreenShareService.stop(this)
                        },
                        onAttach = { pickFile.launch("*/*") },
                        onSaveFile = { key ->
                            vm.file(key)?.let {
                                saving = it
                                saveFile.launch(it.first)
                            }
                        },
                    )
                } else {
                    HomeScreen(vm)
                }
            }
        }
    }

    private fun granted(permission: String) =
        ContextCompat.checkSelfPermission(this, permission) == PackageManager.PERMISSION_GRANTED

    private fun requestPermissions() {
        val wanted = buildList {
            add(Manifest.permission.RECORD_AUDIO)
            add(Manifest.permission.CAMERA)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) add(Manifest.permission.POST_NOTIFICATIONS)
        }.filterNot(::granted)
        if (wanted.isNotEmpty()) permissions.launch(wanted.toTypedArray())
    }
}
