package io.github.xiecheesejohn.copym

import android.Manifest
import android.os.Build
import android.os.Bundle
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

class MainActivity : ComponentActivity() {
    private val notifPermission = registerForActivityResult(ActivityResultContracts.RequestPermission()) {}

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        Core.init(this)
        if (Prefs(this).backgroundSync) SyncService.start(this)
        setContent {
            CopymTheme {
                MainScreen(onBackgroundSyncChanged = ::setBackgroundSync)
            }
        }
    }

    private fun setBackgroundSync(on: Boolean) {
        Prefs(this).backgroundSync = on
        if (on) {
            if (Build.VERSION.SDK_INT >= 33) notifPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
            SyncService.start(this)
        } else {
            SyncService.stop(this)
        }
    }

    /** The only moment Android lets us read the clipboard: when we have focus. */
    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (!hasFocus || !Core.ready) return
        val snap = Clip.snapshot(this)
        lifecycleScope.launch {
            val msg = withContext(Dispatchers.IO) { Clip.save(applicationContext, snap, force = false) }
            if (msg != null) Toast.makeText(this@MainActivity, msg, Toast.LENGTH_SHORT).show()
        }
    }
}

private val Accent = Color(0xFF3D5AFE)

@Composable
fun CopymTheme(content: @Composable () -> Unit) {
    val dark = isSystemInDarkTheme()
    val ctx = LocalContext.current
    val scheme = when {
        Build.VERSION.SDK_INT >= 31 -> if (dark) dynamicDarkColorScheme(ctx) else dynamicLightColorScheme(ctx)
        dark -> darkColorScheme(primary = Color(0xFF8C9EFF))
        else -> lightColorScheme(primary = Accent)
    }
    MaterialTheme(colorScheme = scheme, content = content)
}
