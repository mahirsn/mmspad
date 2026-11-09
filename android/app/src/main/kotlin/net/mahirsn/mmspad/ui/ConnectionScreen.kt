package net.mahirsn.mmspad.ui

import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import net.mahirsn.mmspad.R
import net.mahirsn.mmspad.network.ConnectionState

/** Shown until the PC answers; the app keeps trying on its own. */
@Composable
fun ConnectionScreen(connectionState: ConnectionState) {
    Column(
        modifier = Modifier.fillMaxSize().background(Color.Black).padding(32.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Image(
            painter = painterResource(id = R.mipmap.ic_launcher),
            contentDescription = null,
            modifier = Modifier.size(96.dp).clip(RoundedCornerShape(20.dp)),
        )
        Spacer(Modifier.height(16.dp))
        Text("mmspad", style = MaterialTheme.typography.headlineLarge, color = Color.White)
        Spacer(Modifier.height(32.dp))
        CircularProgressIndicator(color = Color.White)
        Spacer(Modifier.height(16.dp))
        Text(
            "Waiting for the PC. Connect the USB cable and start mmspad on the PC.",
            color = Color.White.copy(alpha = 0.8f),
            textAlign = TextAlign.Center,
        )
        (connectionState as? ConnectionState.Error)?.let {
            Spacer(Modifier.height(8.dp))
            Text(it.message, color = Color.White.copy(alpha = 0.5f), textAlign = TextAlign.Center)
        }
    }
}
