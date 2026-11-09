package net.mahirsn.mmspad

import android.os.Bundle
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.*
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import net.mahirsn.mmspad.network.ConnectionState
import net.mahirsn.mmspad.network.NetworkClient
import net.mahirsn.mmspad.ui.ConnectionScreen
import net.mahirsn.mmspad.ui.StreamScreen
import kotlinx.coroutines.delay

/** The PC's server, reached over the USB cable through `adb reverse tcp:9999 tcp:9999`. */
private const val SERVER_HOST = "127.0.0.1"
private const val SERVER_PORT = 9999

class MainActivity : ComponentActivity() {
    private var client: NetworkClient? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        setTheme(R.style.Theme_Mmspad)
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        WindowCompat.setDecorFitsSystemWindows(window, false)
        WindowInsetsControllerCompat(window, window.decorView).let { controller ->
            controller.hide(WindowInsetsCompat.Type.systemBars())
            controller.systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
        setContent { MaterialTheme { App() } }
    }

    override fun onDestroy() {
        super.onDestroy()
        client?.close()
    }

    @Composable
    private fun App() {
        // A new client for every attempt; the old one is closed first.
        var attempt by remember { mutableIntStateOf(0) }
        val current = remember(attempt) {
            client?.close()
            NetworkClient(SERVER_HOST, SERVER_PORT).also { client = it }
        }
        val state by current.connectionState.collectAsState()

        LaunchedEffect(current) { current.connect() }
        // Until the PC answers, try again every 2 seconds.
        LaunchedEffect(current, state) {
            if (state is ConnectionState.Error) {
                delay(2000)
                attempt++
            }
        }

        when (val s = state) {
            is ConnectionState.Connected -> key(s.width, s.height) {
                StreamScreen(
                    client = current,
                    videoPackets = current.videoPackets,
                    width = s.width,
                    height = s.height,
                    onDisconnect = { attempt++ },
                )
            }
            else -> ConnectionScreen(s)
        }
    }
}
