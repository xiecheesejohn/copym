package io.github.xiecheesejohn.copym

import android.graphics.BitmapFactory
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Clear
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.Search
import androidx.compose.material.icons.filled.Star
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.Checkbox
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun SyncSheet(sync: SyncState, onDismiss: () -> Unit) {
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    var pairing by remember { mutableStateOf<Pairing?>(null) }
    var code by remember { mutableStateOf("") }
    var busy by remember { mutableStateOf(false) }
    var message by remember { mutableStateOf<String?>(null) }
    var confirmLeave by remember { mutableStateOf(false) }
    var peers by remember { mutableStateOf(Prefs(ctx).peers) }
    var now by remember { mutableLongStateOf(System.currentTimeMillis()) }
    LaunchedEffect(pairing) {
        while (pairing != null) {
            now = System.currentTimeMillis()
            delay(1000)
        }
    }

    ModalBottomSheet(onDismissRequest = onDismiss, sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)) {
        Column(
            Modifier.fillMaxWidth().verticalScroll(rememberScrollState()).padding(horizontal = 24.dp).navigationBarsPadding().padding(bottom = 16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text("局域网同步", style = MaterialTheme.typography.titleLarge)
            if (!sync.enabled) {
                Text("同步不可用（网络初始化失败）", color = MaterialTheme.colorScheme.error)
                return@Column
            }
            Text(
                "本机：${sync.name} · " + if (sync.paired) "已加入同步组" else "未配对",
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            if (sync.peers.isEmpty()) {
                Text(if (sync.paired) "暂无在线设备" else "与电脑或其他手机配对后，复制的内容会在设备之间同步（只进入历史，不会改动剪贴板）。")
            } else {
                sync.peers.forEach { p ->
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Box(Modifier.size(8.dp).clip(RoundedCornerShape(4.dp)).background(Color(0xFF43A047)))
                        Spacer(Modifier.width(8.dp))
                        Text(p.name, Modifier.weight(1f))
                        Text(p.addr, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                }
            }
            HorizontalDivider()

            val active = pairing?.takeIf { it.expiresAt > now && sync.code == it.code }
            if (active != null) {
                Text("在另一台设备上输入配对码：", style = MaterialTheme.typography.bodyMedium)
                Text(active.code, fontSize = 40.sp, fontWeight = FontWeight.Bold, fontFamily = FontFamily.Monospace, letterSpacing = 2.sp)
                Text(
                    "${(active.expiresAt - now) / 1000} 秒内有效 · 自动发现失败时可输入：${active.code}@${active.addrs.firstOrNull() ?: ""}",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            } else {
                FilledTonalButton(onClick = {
                    scope.launch {
                        message = null
                        pairing = withContext(Dispatchers.IO) {
                            runCatching { parsePairing(Native.pairStart()) }.onFailure { message = it.message }.getOrNull()
                        }
                    }
                }, modifier = Modifier.fillMaxWidth()) { Text("添加设备（生成配对码）") }
            }

            Text("加入另一台设备的同步组", style = MaterialTheme.typography.titleSmall)
            Row(verticalAlignment = Alignment.CenterVertically) {
                OutlinedTextField(
                    value = code,
                    onValueChange = { code = it },
                    modifier = Modifier.weight(1f),
                    placeholder = { Text("1234-5678") },
                    singleLine = true,
                    enabled = !busy,
                )
                Spacer(Modifier.width(8.dp))
                Button(enabled = !busy && code.count { it.isDigit() } >= 8, onClick = {
                    busy = true
                    message = "正在查找并配对…"
                    scope.launch {
                        val r = withContext(Dispatchers.IO) { runCatching { Native.join(code) } }
                        busy = false
                        message = r.fold({ code = ""; "已加入 $it 的同步组" }, { "配对失败：${it.message}" })
                    }
                }) { Text("加入") }
            }
            message?.let { Text(it, style = MaterialTheme.typography.bodyMedium) }

            HorizontalDivider()
            Text("手动地址（网络屏蔽自动发现时使用，重启 App 生效）", style = MaterialTheme.typography.bodySmall)
            OutlinedTextField(
                value = peers,
                onValueChange = { peers = it; Prefs(ctx).peers = it },
                modifier = Modifier.fillMaxWidth().heightIn(min = 56.dp),
                placeholder = { Text("192.168.1.10, 192.168.1.20:47800") },
                singleLine = true,
            )
            if (sync.paired) {
                OutlinedButton(onClick = { confirmLeave = true }, modifier = Modifier.fillMaxWidth()) {
                    Text("退出同步组", color = MaterialTheme.colorScheme.error)
                }
            }
        }
    }

    if (confirmLeave) {
        AlertDialog(
            onDismissRequest = { confirmLeave = false },
            title = { Text("退出同步组？") },
            text = { Text("本机会忘记组密钥并断开所有设备。历史记录会保留。") },
            confirmButton = {
                TextButton(onClick = {
                    confirmLeave = false
                    pairing = null
                    scope.launch(Dispatchers.IO) { runCatching { Native.leave() } }
                }) { Text("退出") }
            },
            dismissButton = { TextButton(onClick = { confirmLeave = false }) { Text("取消") } },
        )
    }
}
