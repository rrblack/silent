package app.silent

import android.Manifest
import android.app.Activity
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.activity.viewModels
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.core.content.ContextCompat

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

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        camera = CameraFeed(this)
        requestPermissions()

        setContent {
            SilentTheme {
                val inRoom = vm.room != null
                LaunchedEffect(vm.cameraOn, inRoom) {
                    if (vm.cameraOn && inRoom && granted(Manifest.permission.CAMERA)) {
                        camera.start(this@MainActivity, vm.cameraHigh)
                    } else {
                        camera.stop()
                    }
                }
                DisposableEffect(Unit) { onDispose { camera.stop() } }
                if (inRoom) {
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
