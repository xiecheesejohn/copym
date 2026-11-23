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

private val FILTERS = listOf("全部", "文字", "图片", "固定")

/** Polls the core's change counter while the screen is visible. */
@Composable
private fun rememberCoreVersion(): Long {
    var version by remember { mutableLongStateOf(0L) }
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    LaunchedEffect(lifecycle) {
        lifecycle.repeatOnLifecycle(Lifecycle.State.STARTED) {
            while (true) {
                val v = Native.version()
                if (v != version) version = v
                delay(700)
            }
        }
    }
    return version
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MainScreen(onBackgroundSyncChanged: (Boolean) -> Unit) {
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    val snackbar = remember { SnackbarHostState() }
    var query by remember { mutableStateOf("") }
    var regex by remember { mutableStateOf(false) }
    var filter by remember { mutableIntStateOf(0) }
    var searchError by remember { mutableStateOf<String?>(null) }
    var showSync by remember { mutableStateOf(false) }
    var menu by remember { mutableStateOf(false) }
    var confirmClear by remember { mutableStateOf(false) }
    var bgSync by remember { mutableStateOf(Prefs(ctx).backgroundSync) }
    val version = if (Core.ready) rememberCoreVersion() else 0L

    val items by produceState(emptyList<Item>(), query, regex, filter, version) {
        if (!Core.ready) return@produceState
        value = withContext(Dispatchers.IO) {
            try {
                parseItems(Native.list(query, regex, filter, 300)).also { searchError = null }
            } catch (e: Exception) {
                searchError = e.message
                value
            }
        }
    }
    val sync by produceState(SyncState(false, false, "", emptyList(), null), version) {
        while (Core.ready) {
            value = withContext(Dispatchers.IO) { parseSync(Native.syncStatus()) }
            delay(2000)
        }
    }

    Scaffold(
        snackbarHost = { SnackbarHost(snackbar) },
        topBar = {
            TopAppBar(
                title = { Text("copym", fontWeight = FontWeight.SemiBold) },
                actions = {
                    TextButton(onClick = { showSync = true }) {
                        Text(
                            when {
                                !sync.paired -> "同步 · 未配对"
                                sync.peers.isEmpty() -> "同步 · 离线"
                                else -> "同步 · ${sync.peers.size} 台在线"
                            },
                        )
                    }
                    IconButton(onClick = { menu = true }) { Icon(Icons.Default.MoreVert, "更多") }
                    DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                        DropdownMenuItem(
                            text = { Text("后台保持同步") },
                            trailingIcon = { Checkbox(checked = bgSync, onCheckedChange = null) },
                            onClick = {
                                bgSync = !bgSync
                                onBackgroundSyncChanged(bgSync)
                                menu = false
                            },
                        )
                        DropdownMenuItem(text = { Text("清空历史（保留固定项）") }, onClick = { menu = false; confirmClear = true })
                    }
                },
            )
        },
    ) { padding ->
        Column(Modifier.padding(padding).fillMaxSize()) {
            Core.error?.let {
                Text("核心库启动失败：$it", color = MaterialTheme.colorScheme.error, modifier = Modifier.padding(16.dp))
            }
            OutlinedTextField(
                value = query,
                onValueChange = { query = it },
                modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp),
                placeholder = { Text(if (regex) "正则表达式" else "搜索文字和图片中的文字") },
                leadingIcon = { Icon(Icons.Default.Search, null) },
                trailingIcon = {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        if (query.isNotEmpty()) {
                            IconButton(onClick = { query = "" }) { Icon(Icons.Default.Clear, "清除") }
                        }
                        TextButton(onClick = { regex = !regex }) {
                            Text(
                                ".*",
                                fontFamily = FontFamily.Monospace,
                                fontWeight = if (regex) FontWeight.Bold else FontWeight.Normal,
                                color = if (regex) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                    }
                },
                singleLine = true,
                isError = searchError != null,
                supportingText = searchError?.let { { Text(it) } },
                shape = RoundedCornerShape(28.dp),
                keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
            )
            LazyRow(
                contentPadding = PaddingValues(horizontal = 16.dp),
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                items(FILTERS.size) { i ->
                    FilterChip(selected = filter == i, onClick = { filter = i }, label = { Text(FILTERS[i]) })
                }
            }
            if (items.isEmpty()) {
                EmptyState(query.isNotEmpty() || filter != 0)
            } else {
                LazyColumn(
                    contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 8.dp, bottom = 24.dp),
                    verticalArrangement = Arrangement.spacedBy(8.dp),
                    modifier = Modifier.fillMaxSize(),
                ) {
                    items(items, key = { it.id }) { item ->
                        ItemRow(
                            item = item,
                            onCopy = {
                                scope.launch {
                                    val ok = withContext(Dispatchers.IO) { runCatching { Clip.copy(ctx, item) }.getOrDefault(false) }
                                    snackbar.showSnackbar(if (ok) "已复制" else "复制失败")
                                }
                            },
                            onPin = { scope.launch(Dispatchers.IO) { runCatching { Native.togglePin(item.id) } } },
                            onShare = { scope.launch(Dispatchers.IO) { runCatching { Clip.share(ctx, item) } } },
                            onDelete = { scope.launch(Dispatchers.IO) { runCatching { Native.delete(item.id) } } },
                        )
                    }
                }
            }
        }
    }

    if (confirmClear) {
        AlertDialog(
            onDismissRequest = { confirmClear = false },
            title = { Text("清空历史？") },
            text = { Text("固定的条目会保留。已配对的设备上对应的条目也会被删除。") },
            confirmButton = {
                TextButton(onClick = {
                    confirmClear = false
                    scope.launch(Dispatchers.IO) { runCatching { Native.clear() } }
                }) { Text("清空") }
            },
            dismissButton = { TextButton(onClick = { confirmClear = false }) { Text("取消") } },
        )
    }
    if (showSync) SyncSheet(sync, onDismiss = { showSync = false })
}

@Composable
private fun EmptyState(filtered: Boolean) {
    Box(Modifier.fillMaxSize().padding(32.dp), contentAlignment = Alignment.Center) {
        Text(
            if (filtered) {
                "没有匹配的条目"
            } else {
                "还没有历史记录\n\n• 复制内容后打开 copym 会自动保存\n• 在任意 App 中“分享 → 保存到 copym”\n• 下拉通知栏添加“保存剪贴板”快捷开关\n• 与电脑配对后，电脑上的复制会同步到这里"
            },
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            lineHeight = 22.sp,
        )
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun ItemRow(item: Item, onCopy: () -> Unit, onPin: () -> Unit, onShare: () -> Unit, onDelete: () -> Unit) {
    var menu by remember { mutableStateOf(false) }
    Card(
        modifier = Modifier.fillMaxWidth().clip(RoundedCornerShape(16.dp)).combinedClickable(onClick = onCopy, onLongClick = { menu = true }),
        shape = RoundedCornerShape(16.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerLow),
    ) {
        Row(Modifier.padding(14.dp), verticalAlignment = Alignment.CenterVertically) {
            if (item.isImage) {
                Thumb(item.id)
                Spacer(Modifier.width(12.dp))
            }
            Column(Modifier.weight(1f)) {
                if (item.isImage) {
                    Text("图片 ${item.width}×${item.height}", style = MaterialTheme.typography.bodyLarge)
                    item.ocr?.takeIf { it.isNotBlank() }?.let {
                        Text(it, maxLines = 2, overflow = TextOverflow.Ellipsis, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                } else {
                    Text(item.preview.trim(), maxLines = 3, overflow = TextOverflow.Ellipsis, style = MaterialTheme.typography.bodyLarge)
                }
                Spacer(Modifier.height(4.dp))
                Text(
                    listOfNotNull(item.source, relativeTime(item.created), if (!item.isImage && item.lines > 1) "${item.lines} 行" else null)
                        .joinToString(" · "),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            if (item.pinned) Icon(Icons.Default.Star, "已固定", tint = MaterialTheme.colorScheme.primary, modifier = Modifier.size(18.dp))
        }
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            DropdownMenuItem(text = { Text("复制") }, onClick = { menu = false; onCopy() })
            DropdownMenuItem(text = { Text(if (item.pinned) "取消固定" else "固定") }, onClick = { menu = false; onPin() })
            DropdownMenuItem(text = { Text("分享") }, onClick = { menu = false; onShare() })
            DropdownMenuItem(text = { Text("删除", color = MaterialTheme.colorScheme.error) }, onClick = { menu = false; onDelete() })
        }
    }
}

@Composable
private fun Thumb(id: Long) {
    val bmp by produceState<ImageBitmap?>(null, id) {
        value = withContext(Dispatchers.IO) {
            Native.thumb(id)?.let { BitmapFactory.decodeByteArray(it, 0, it.size)?.asImageBitmap() }
        }
    }
    Box(
        Modifier.size(64.dp).clip(RoundedCornerShape(10.dp)).background(MaterialTheme.colorScheme.surfaceVariant),
        contentAlignment = Alignment.Center,
    ) {
        bmp?.let { Image(it, null, contentScale = ContentScale.Crop, modifier = Modifier.fillMaxSize()) }
    }
}

private fun relativeTime(secs: Long): String {
    val d = System.currentTimeMillis() / 1000 - secs
    return when {
        d < 60 -> "刚刚"
        d < 3600 -> "${d / 60} 分钟前"
        d < 86400 -> "${d / 3600} 小时前"
        d < 7 * 86400 -> "${d / 86400} 天前"
        else -> SimpleDateFormat("yyyy-MM-dd", Locale.getDefault()).format(Date(secs * 1000))
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun SyncSheet(sync: SyncState, onDismiss: () -> Unit) {
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
