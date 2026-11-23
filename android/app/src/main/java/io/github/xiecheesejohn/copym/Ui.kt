package io.github.xiecheesejohn.copym

import android.graphics.BitmapFactory
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.focusable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Clear
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.Search
import androidx.compose.material.icons.filled.Star
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.Checkbox
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
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
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.input.key.Key
import androidx.compose.ui.input.key.KeyEvent
import androidx.compose.ui.input.key.KeyEventType
import androidx.compose.ui.input.key.isAltPressed
import androidx.compose.ui.input.key.isCtrlPressed
import androidx.compose.ui.input.key.isMetaPressed
import androidx.compose.ui.input.key.isShiftPressed
import androidx.compose.ui.input.key.key
import androidx.compose.ui.input.key.onPreviewKeyEvent
import androidx.compose.ui.input.key.type
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

private val DIGITS = listOf(Key.One, Key.Two, Key.Three, Key.Four, Key.Five, Key.Six, Key.Seven, Key.Eight, Key.Nine)

private fun loadGroups(): Groups =
    if (Core.ready) runCatching { parseGroups(Native.groups()) }.getOrNull() ?: Groups("", emptyList()) else Groups("", emptyList())

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun MainScreen(onBackgroundSyncChanged: (Boolean) -> Unit) {
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    val snackbar = remember { SnackbarHostState() }
    var query by remember { mutableStateOf("") }
    var regex by remember { mutableStateOf(false) }
    var groups by remember { mutableStateOf(loadGroups()) }
    var group by remember { mutableIntStateOf(groups.defaultIndex()) }
    var searchError by remember { mutableStateOf<String?>(null) }
    var showSync by remember { mutableStateOf(false) }
    var menu by remember { mutableStateOf(false) }
    var confirmClear by remember { mutableStateOf(false) }
    var bgSync by remember { mutableStateOf(Prefs(ctx).backgroundSync) }
    var preview by remember { mutableStateOf<Item?>(null) }
    var editor by remember { mutableStateOf<Int?>(null) }
    var chipMenu by remember { mutableStateOf<Int?>(null) }
    var selected by remember { mutableIntStateOf(0) }
    var keyboardNav by remember { mutableStateOf(false) }
    var searchFocused by remember { mutableStateOf(false) }
    val rootFocus = remember { FocusRequester() }
    val searchFocus = remember { FocusRequester() }
    val listState = rememberLazyListState()
    val chipState = rememberLazyListState()
    val version = if (Core.ready) rememberCoreVersion() else 0L

    val items by produceState(emptyList<Item>(), query, regex, group, version, groups) {
        if (!Core.ready) return@produceState
        value = withContext(Dispatchers.IO) {
            try {
                parseItems(Native.listGroup(query, regex, group, 300)).also { searchError = null }
            } catch (e: Exception) {
                searchError = e.message
                emptyList()
            }
        }
    }
    val counts by produceState(emptyList<Int?>(), query, regex, version, groups) {
        if (!Core.ready) return@produceState
        value = withContext(Dispatchers.IO) { runCatching { parseCounts(Native.groupCounts(query, regex)) }.getOrDefault(emptyList()) }
    }
    val sync by produceState(SyncState(false, false, "", emptyList(), null), version) {
        while (Core.ready) {
            value = withContext(Dispatchers.IO) { parseSync(Native.syncStatus()) }
            delay(2000)
        }
    }
    LaunchedEffect(query, regex, group) { selected = 0 }
    LaunchedEffect(group) { if (group in groups.list.indices) chipState.animateScrollToItem(group) }
    LaunchedEffect(items.size) { selected = selected.coerceIn(0, (items.size - 1).coerceAtLeast(0)) }
    LaunchedEffect(Unit) { runCatching { rootFocus.requestFocus() } }
    // keep the keyboard selection visible
    LaunchedEffect(selected, keyboardNav) {
        if (!keyboardNav || items.isEmpty()) return@LaunchedEffect
        val visible = listState.layoutInfo.visibleItemsInfo
        val first = visible.firstOrNull()?.index ?: 0
        val last = visible.lastOrNull()?.index ?: 0
        when {
            selected <= first -> listState.animateScrollToItem((selected - 1).coerceAtLeast(0))
            selected >= last -> listState.animateScrollToItem((selected - (last - first) + 2).coerceAtLeast(0))
        }
    }

    fun copy(item: Item) {
        scope.launch {
            val ok = withContext(Dispatchers.IO) { runCatching { Clip.copy(ctx, item) }.getOrDefault(false) }
            snackbar.showSnackbar(if (ok) "已复制" else "复制失败")
        }
    }

    fun io(block: () -> Unit) {
        scope.launch(Dispatchers.IO) { runCatching(block) }
    }

    fun saveGroups(g: Groups): String? = try {
        Native.setGroups(g.toJson())
        groups = g
        group = group.coerceIn(0, (g.list.size - 1).coerceAtLeast(0))
        null
    } catch (e: Exception) {
        e.message ?: "保存失败"
    }

    /**
     * Hardware keyboard: ↑↓, Vim (j/k outside the search box, Ctrl+J/K anywhere),
     * Emacs (Ctrl+N/P); Enter copies; Tab / Ctrl+1…9 switch groups; Alt+1…9 copy
     * the N-th item; Ctrl+S pin; Ctrl+D delete; / focuses the search box.
     */
    fun onKey(ev: KeyEvent): Boolean {
        if (ev.type != KeyEventType.KeyDown || preview != null || editor != null || showSync) return false
        val ctrl = ev.isCtrlPressed
        val alt = ev.isAltPressed
        val plain = !ctrl && !alt && !ev.isMetaPressed
        fun move(d: Int): Boolean {
            keyboardNav = true
            if (items.isNotEmpty()) selected = (selected + d).coerceIn(0, items.size - 1)
            return true
        }
        val digit = DIGITS.indexOf(ev.key).takeIf { it >= 0 }
        val current = items.getOrNull(selected)
        return when {
            ev.key == Key.DirectionDown -> move(1)
            ev.key == Key.DirectionUp -> move(-1)
            ctrl && (ev.key == Key.J || ev.key == Key.N) -> move(1)
            ctrl && (ev.key == Key.K || ev.key == Key.P) -> move(-1)
            !searchFocused && plain && ev.key == Key.J -> move(1)
            !searchFocused && plain && ev.key == Key.K -> move(-1)
            !searchFocused && plain && ev.key == Key.Slash -> {
                runCatching { searchFocus.requestFocus() }
                true
            }
            ev.key == Key.Enter || ev.key == Key.NumPadEnter -> {
                current?.let(::copy)
                true
            }
            ev.key == Key.Tab && groups.list.isNotEmpty() -> {
                val n = groups.list.size
                group = (group + if (ev.isShiftPressed) n - 1 else 1) % n
                true
            }
            ctrl && digit != null -> {
                if (digit in groups.list.indices) group = digit
                true
            }
            alt && digit != null -> {
                items.getOrNull(digit)?.let(::copy)
                true
            }
            ctrl && ev.key == Key.S -> {
                current?.let { io { Native.togglePin(it.id) } }
                true
            }
            (ctrl && ev.key == Key.D) || (ev.isShiftPressed && ev.key == Key.Delete) -> {
                current?.let { io { Native.delete(it.id) } }
                true
            }
            ev.key == Key.Escape -> {
                if (query.isNotEmpty()) query = "" else runCatching { rootFocus.requestFocus() }
                true
            }
            else -> false
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
                        DropdownMenuItem(text = { Text("管理分组…") }, onClick = { menu = false; editor = -1 })
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
        Column(
            Modifier
                .padding(padding)
                .fillMaxSize()
                .onPreviewKeyEvent(::onKey)
                .focusRequester(rootFocus)
                .focusable(),
        ) {
            Core.error?.let {
                Text("核心库启动失败：$it", color = MaterialTheme.colorScheme.error, modifier = Modifier.padding(16.dp))
            }
            OutlinedTextField(
                value = query,
                onValueChange = { query = it },
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 16.dp, vertical = 4.dp)
                    .focusRequester(searchFocus)
                    .onFocusChanged { searchFocused = it.isFocused },
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
                keyboardActions = KeyboardActions(onSearch = { items.getOrNull(selected)?.let(::copy) }),
            )
            // group tabs
            LazyRow(
                state = chipState,
                contentPadding = PaddingValues(horizontal = 16.dp, vertical = 4.dp),
                horizontalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                itemsIndexed(groups.list) { i, g ->
                    Box {
                        GroupChip(
                            name = g.name,
                            count = counts.getOrNull(i),
                            invalid = i < counts.size && counts[i] == null,
                            selected = i == group,
                            onClick = { group = i },
                            onLongClick = { chipMenu = i },
                        )
                        DropdownMenu(expanded = chipMenu == i, onDismissRequest = { chipMenu = null }) {
                            DropdownMenuItem(
                                text = { Text(if (groups.default == g.name) "已是默认分组" else "设为默认分组") },
                                enabled = groups.default != g.name,
                                onClick = {
                                    chipMenu = null
                                    val err = saveGroups(groups.copy(default = g.name))
                                    scope.launch { snackbar.showSnackbar(err ?: "打开时将默认显示「${g.name}」") }
                                },
                            )
                            DropdownMenuItem(text = { Text("编辑分组…") }, onClick = { chipMenu = null; editor = i })
                        }
                    }
                }
            }
            if (items.isEmpty()) {
                EmptyState(query.isNotEmpty(), group != 0)
            } else {
                LazyColumn(
                    state = listState,
                    contentPadding = PaddingValues(start = 16.dp, end = 16.dp, top = 6.dp, bottom = 24.dp),
                    verticalArrangement = Arrangement.spacedBy(8.dp),
                    modifier = Modifier.fillMaxSize(),
                ) {
                    itemsIndexed(items, key = { _, it -> it.id }) { i, item ->
                        ItemRow(
                            item = item,
                            highlighted = keyboardNav && i == selected,
                            onClick = { selected = i; copy(item) },
                            onLongClick = { selected = i; preview = item },
                        )
                    }
                }
            }
        }
    }

    preview?.let { item ->
        PreviewSheet(
            item = item,
            onDismiss = { preview = null },
            onCopy = { preview = null; copy(item) },
            onPin = { preview = null; io { Native.togglePin(item.id) } },
            onShare = { preview = null; io { Clip.share(ctx, item) } },
            onDelete = { preview = null; io { Native.delete(item.id) } },
        )
    }
    editor?.let { start ->
        GroupEditor(
            initial = groups,
            startAt = start,
            onSave = { g ->
                saveGroups(g).also {
                    if (it == null) {
                        editor = null
                        scope.launch { snackbar.showSnackbar("分组已保存") }
                    }
                }
            },
            onDismiss = { editor = null },
        )
    }
    if (confirmClear) {
        AlertDialog(
            onDismissRequest = { confirmClear = false },
            title = { Text("清空历史？") },
            text = { Text("固定的条目会保留。已配对的设备上对应的条目也会被删除。") },
            confirmButton = {
                TextButton(onClick = {
                    confirmClear = false
                    io { Native.clear() }
                }) { Text("清空") }
            },
            dismissButton = { TextButton(onClick = { confirmClear = false }) { Text("取消") } },
        )
    }
    if (showSync) SyncSheet(sync, onDismiss = { showSync = false })
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun GroupChip(name: String, count: Int?, invalid: Boolean, selected: Boolean, onClick: () -> Unit, onLongClick: () -> Unit) {
    val scheme = MaterialTheme.colorScheme
    Surface(
        shape = RoundedCornerShape(50),
        color = if (selected) scheme.primary else Color.Transparent,
        border = if (selected) null else BorderStroke(1.dp, scheme.outlineVariant),
        modifier = Modifier.clip(RoundedCornerShape(50)).combinedClickable(onClick = onClick, onLongClick = onLongClick),
    ) {
        Row(Modifier.padding(horizontal = 14.dp, vertical = 7.dp), verticalAlignment = Alignment.CenterVertically) {
            val fg = if (selected) scheme.onPrimary else scheme.onSurfaceVariant
            Text(name, color = fg, style = MaterialTheme.typography.labelLarge)
            val badge = if (invalid) "!" else count?.toString()
            if (badge != null) {
                Spacer(Modifier.width(6.dp))
                Text(badge, color = fg.copy(alpha = 0.7f), style = MaterialTheme.typography.labelMedium)
            }
        }
    }
}

@Composable
private fun EmptyState(searching: Boolean, inGroup: Boolean) {
    Box(Modifier.fillMaxSize().padding(32.dp), contentAlignment = Alignment.Center) {
        Text(
            when {
                searching -> "没有匹配的条目"
                inGroup -> "这个分组里还没有内容\n\n长按分组标签可以编辑分组规则"
                else -> "还没有历史记录\n\n• 复制内容后打开 copym 会自动保存\n• 在任意 App 中“分享 → 保存到 copym”\n• 下拉通知栏添加“保存剪贴板”快捷开关\n• 与电脑配对后，电脑上的复制会同步到这里"
            },
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            lineHeight = 22.sp,
        )
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun ItemRow(item: Item, highlighted: Boolean, onClick: () -> Unit, onLongClick: () -> Unit) {
    val scheme = MaterialTheme.colorScheme
    Card(
        modifier = Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(16.dp))
            .combinedClickable(onClick = onClick, onLongClick = onLongClick)
            .then(if (highlighted) Modifier.border(2.dp, scheme.primary, RoundedCornerShape(16.dp)) else Modifier),
        shape = RoundedCornerShape(16.dp),
        colors = CardDefaults.cardColors(containerColor = if (highlighted) scheme.surfaceContainerHighest else scheme.surfaceContainerLow),
    ) {
        Row(Modifier.padding(12.dp), verticalAlignment = Alignment.CenterVertically) {
            TypeTile(item, 44)
            Spacer(Modifier.width(12.dp))
            Column(Modifier.weight(1f)) {
                ItemTitle(item)
                Spacer(Modifier.height(3.dp))
                Text(
                    listOfNotNull(
                        item.label,
                        item.source,
                        relativeTime(item.created),
                        if (!item.isImage && item.lines > 1) "${item.lines} 行" else null,
                        if (item.isImage) "${item.width}×${item.height}" else null,
                    ).joinToString(" · "),
                    style = MaterialTheme.typography.labelSmall,
                    color = scheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            if (item.pinned) Icon(Icons.Default.Star, "已固定", tint = scheme.primary, modifier = Modifier.size(18.dp))
        }
    }
}

@Composable
private fun ItemTitle(item: Item) {
    val body = MaterialTheme.typography.bodyLarge
    val mono = MaterialTheme.typography.bodyMedium.copy(fontFamily = FontFamily.Monospace, fontSize = 13.sp)
    val dim = MaterialTheme.colorScheme.onSurfaceVariant
    when (item.type) {
        "image" -> {
            val ocr = item.ocr?.lineSequence()?.map { it.trim() }?.firstOrNull { it.isNotEmpty() }
            Text(ocr ?: "图片", style = body, maxLines = 1, overflow = TextOverflow.Ellipsis)
        }
        "url" -> {
            val (host, path) = splitUrl(item.preview.trim())
            Text(host, style = body, fontWeight = FontWeight.Medium, maxLines = 1, overflow = TextOverflow.Ellipsis)
            if (path.length > 1) Text(path, style = mono, color = dim, maxLines = 1, overflow = TextOverflow.Ellipsis)
        }
        "code", "json" -> Text(item.preview.trimEnd(), style = mono, maxLines = 3, overflow = TextOverflow.Ellipsis)
        "path", "email", "color" -> Text(item.preview.trim(), style = mono, maxLines = 2, overflow = TextOverflow.Ellipsis)
        else -> Text(item.preview.trim(), style = body, maxLines = 3, overflow = TextOverflow.Ellipsis)
    }
}

/** Leading tile: thumbnail, color swatch or a tinted type glyph. */
@Composable
internal fun TypeTile(item: Item, sizeDp: Int) {
    val shape = RoundedCornerShape(12.dp)
    val mod = Modifier.size(sizeDp.dp).clip(shape)
    when {
        item.isImage -> Thumb(item.id, mod)
        item.color != null -> Box(mod.background(Color(item.color)).border(1.dp, MaterialTheme.colorScheme.outlineVariant, shape))
        else -> {
            val tint = typeTint(item.type)
            val glyph = typeGlyph(item.type)
            Box(mod.background(tint.copy(alpha = 0.16f)), contentAlignment = Alignment.Center) {
                Text(glyph, color = tint, fontSize = if (glyph.length > 2) 12.sp else 16.sp, fontWeight = FontWeight.Medium)
            }
        }
    }
}

@Composable
private fun Thumb(id: Long, modifier: Modifier) {
    val bmp by produceState<ImageBitmap?>(null, id) {
        value = withContext(Dispatchers.IO) {
            runCatching { Native.thumb(id)?.let { BitmapFactory.decodeByteArray(it, 0, it.size)?.asImageBitmap() } }.getOrNull()
        }
    }
    Box(modifier.background(MaterialTheme.colorScheme.surfaceVariant)) {
        bmp?.let { Image(it, null, contentScale = ContentScale.Crop, modifier = Modifier.fillMaxSize()) }
    }
}

internal fun typeGlyph(type: String) = when (type) {
    "url" -> "↗"
    "email" -> "@"
    "path" -> "/"
    "color" -> "#"
    "json" -> "{ }"
    "code" -> "</>"
    "image" -> "▣"
    else -> "Aa"
}

@Composable
internal fun typeTint(type: String): Color {
    val bg = MaterialTheme.colorScheme.background
    val dark = 0.2126f * bg.red + 0.7152f * bg.green + 0.0722f * bg.blue < 0.5f
    val (l, d) = when (type) {
        "url" -> 0xFF2563EB to 0xFF60A5FA
        "email" -> 0xFF0E7490 to 0xFF22D3EE
        "code" -> 0xFF059669 to 0xFF34D399
        "json" -> 0xFFD97706 to 0xFFFBBF24
        "path" -> 0xFF7C3AED to 0xFFA78BFA
        "color" -> 0xFFDB2777 to 0xFFF472B6
        "image" -> 0xFF0D9488 to 0xFF2DD4BF
        else -> 0xFF64646F to 0xFFA0A1AF
    }
    return Color(if (dark) d else l)
}

internal fun splitUrl(url: String): Pair<String, String> {
    val after = url.substringAfter("://", url)
    val host = after.takeWhile { it != '/' && it != '?' && it != '#' }
    return host to after.substring(host.length)
}

internal fun relativeTime(secs: Long): String {
    val d = System.currentTimeMillis() / 1000 - secs
    return when {
        d < 60 -> "刚刚"
        d < 3600 -> "${d / 60} 分钟前"
        d < 86400 -> "${d / 3600} 小时前"
        d < 7 * 86400 -> "${d / 86400} 天前"
        else -> SimpleDateFormat("yyyy-MM-dd", Locale.getDefault()).format(Date(secs * 1000))
    }
}

internal fun humanSize(b: Long): String = when {
    b < 1024 -> "$b B"
    b < 1024 * 1024 -> "%.1f KB".format(b / 1024.0)
    else -> "%.1f MB".format(b / 1024.0 / 1024.0)
}
