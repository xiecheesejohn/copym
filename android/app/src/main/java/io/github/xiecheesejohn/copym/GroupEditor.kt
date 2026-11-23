package io.github.xiecheesejohn.copym

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.KeyboardArrowUp
import androidx.compose.material3.Checkbox
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext

/**
 * Full-screen editor for the groups (same rules as the desktop app).
 * [onSave] returns an error message, or null on success.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun GroupEditor(initial: Groups, startAt: Int?, onSave: (Groups) -> String?, onDismiss: () -> Unit) {
    var draft by remember { mutableStateOf(initial) }
    var editing by remember { mutableStateOf(startAt?.takeIf { it in initial.list.indices }) }
    var error by remember { mutableStateOf<String?>(null) }

    Dialog(onDismissRequest = onDismiss, properties = DialogProperties(usePlatformDefaultWidth = false)) {
        Scaffold(
            topBar = {
                TopAppBar(
                    title = { Text(if (editing == null) "管理分组" else "编辑分组") },
                    navigationIcon = {
                        if (editing == null) {
                            IconButton(onClick = onDismiss) { Icon(Icons.Default.Close, "关闭") }
                        } else {
                            IconButton(onClick = { editing = null }) { Icon(Icons.AutoMirrored.Filled.ArrowBack, "返回") }
                        }
                    },
                    actions = {
                        TextButton(onClick = { error = onSave(draft) }) { Text("保存") }
                    },
                )
            },
        ) { padding ->
            Column(Modifier.padding(padding).fillMaxSize().imePadding()) {
                error?.let {
                    Text(it, color = MaterialTheme.colorScheme.error, modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp))
                }
                val idx = editing
                if (idx == null || idx !in draft.list.indices) {
                    GroupList(
                        groups = draft,
                        onEdit = { editing = it },
                        onChange = { draft = it; error = null },
                    )
                } else {
                    GroupForm(
                        groups = draft,
                        index = idx,
                        onChange = { draft = it; error = null },
                    )
                }
            }
        }
    }
}

@Composable
private fun GroupList(groups: Groups, onEdit: (Int) -> Unit, onChange: (Groups) -> Unit) {
    LazyColumn(Modifier.fillMaxSize()) {
        item {
            Text(
                "分组按顺序显示在顶部（外接键盘时对应 Ctrl+1…9）。一个条目可以同时属于多个分组。",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(16.dp),
            )
        }
        itemsIndexed(groups.list) { i, g ->
            Row(
                Modifier.fillMaxWidth().clickable { onEdit(i) }.padding(start = 16.dp, end = 4.dp, top = 6.dp, bottom = 6.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Column(Modifier.weight(1f)) {
                    Text(
                        g.name + if (groups.default == g.name) "  · 默认" else "",
                        style = MaterialTheme.typography.titleMedium,
                    )
                    Text(
                        describe(g),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        maxLines = 1,
                    )
                }
                IconButton(enabled = i > 0, onClick = { onChange(groups.copy(list = groups.list.swap(i, i - 1))) }) {
                    Icon(Icons.Default.KeyboardArrowUp, "上移")
                }
                IconButton(enabled = i < groups.list.size - 1, onClick = { onChange(groups.copy(list = groups.list.swap(i, i + 1))) }) {
                    Icon(Icons.Default.KeyboardArrowDown, "下移")
                }
                IconButton(enabled = groups.list.size > 1, onClick = { onChange(groups.copy(list = groups.list.filterIndexed { j, _ -> j != i })) }) {
                    Icon(Icons.Default.Delete, "删除")
                }
            }
            HorizontalDivider()
        }
        item {
            Row(Modifier.padding(16.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedButton(onClick = {
                    val g = groups.copy(list = groups.list + GroupDef(name = "分组 ${groups.list.size + 1}"))
                    onChange(g)
                    onEdit(g.list.size - 1)
                }) {
                    Icon(Icons.Default.Add, null)
                    Spacer(Modifier.width(6.dp))
                    Text("新建分组")
                }
                TextButton(onClick = { onChange(DEFAULT_GROUPS) }) { Text("恢复默认") }
            }
        }
    }
}

@Composable
private fun GroupForm(groups: Groups, index: Int, onChange: (Groups) -> Unit) {
    val g = groups.list[index]
    // multi-line text fields keep their own text (blank lines allowed while typing)
    var patterns by remember(index) { mutableStateOf(g.patterns.joinToString("\n")) }
    var apps by remember(index) { mutableStateOf(g.apps.joinToString("\n")) }
    fun update(n: GroupDef, default: String = if (groups.default == g.name) n.name else groups.default) {
        onChange(groups.copy(default = default, list = groups.list.mapIndexed { i, o -> if (i == index) n else o }))
    }
    fun lines(s: String) = s.lines().map { it.trim() }.filter { it.isNotEmpty() }

    // live count (with a short debounce); -1 = invalid regex
    val count by produceState<Int?>(null, groups, index) {
        delay(250)
        value = withContext(Dispatchers.IO) { runCatching { Native.draftCount(groups.toJson(), index) }.getOrNull() }
    }

    Column(Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
        OutlinedTextField(value = g.name, onValueChange = { update(g.copy(name = it)) }, label = { Text("名称") }, singleLine = true, modifier = Modifier.fillMaxWidth())
        Text("内容类型", style = MaterialTheme.typography.labelLarge)
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            for ((v, l) in listOf("" to "全部", "text" to "文本", "image" to "图片")) {
                FilterChip(selected = g.kind == v, onClick = { update(g.copy(kind = v)) }, label = { Text(l) })
            }
        }
        SwitchRow("只包含固定的条目", g.pinned) { update(g.copy(pinned = it)) }
        SwitchRow("“其他”分组：不属于任何带规则分组的条目", g.rest) { update(g.copy(rest = it)) }
        OutlinedTextField(
            value = patterns,
            onValueChange = { patterns = it; update(g.copy(patterns = lines(it))) },
            label = { Text("内容正则（每行一条，匹配任意一条）") },
            placeholder = { Text("例：(验证码|code)\\D{0,12}\\d{4,8}") },
            textStyle = MaterialTheme.typography.bodyMedium.copy(fontFamily = FontFamily.Monospace),
            minLines = 2,
            modifier = Modifier.fillMaxWidth(),
        )
        OutlinedTextField(
            value = apps,
            onValueChange = { apps = it; update(g.copy(apps = lines(it))) },
            label = { Text("来源正则（同步设备名或电脑上的来源应用）") },
            placeholder = { Text("例：(?i)macbook|fedora") },
            textStyle = MaterialTheme.typography.bodyMedium.copy(fontFamily = FontFamily.Monospace),
            minLines = 1,
            modifier = Modifier.fillMaxWidth(),
        )
        Row(verticalAlignment = Alignment.CenterVertically) {
            Checkbox(checked = groups.default == g.name, onCheckedChange = { if (it) update(g, default = g.name) })
            Text("打开 App 时默认显示这个分组")
        }
        Text(
            when (count) {
                null -> ""
                -1 -> "正则表达式无效"
                else -> "当前历史中有 $count 条属于这个分组"
            },
            color = if (count == -1) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.primary,
            style = MaterialTheme.typography.bodyMedium,
        )
        Text(
            "正则开头加 (?i) 忽略大小写。规则与电脑版相同，可以把电脑上 groups.toml 里的规则直接抄过来。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.height(24.dp))
    }
}

@Composable
private fun SwitchRow(label: String, checked: Boolean, onChange: (Boolean) -> Unit) {
    Row(Modifier.fillMaxWidth().clickable { onChange(!checked) }, verticalAlignment = Alignment.CenterVertically) {
        Text(label, Modifier.weight(1f))
        Switch(checked = checked, onCheckedChange = onChange)
    }
}

private fun describe(g: GroupDef): String {
    val parts = mutableListOf<String>()
    when (g.kind) {
        "text" -> parts += "文本"
        "image" -> parts += "图片"
    }
    if (g.pinned) parts += "固定"
    if (g.rest) parts += "其他"
    if (g.patterns.isNotEmpty()) parts += "内容：" + g.patterns.joinToString(" | ")
    if (g.apps.isNotEmpty()) parts += "来源：" + g.apps.joinToString(" | ")
    return parts.joinToString(" · ").ifEmpty { "全部条目" }
}

private fun <T> List<T>.swap(a: Int, b: Int): List<T> = toMutableList().also { val t = it[a]; it[a] = it[b]; it[b] = t }

private val DEFAULT_GROUPS = Groups(
    default = "全部",
    list = listOf(
        GroupDef("全部"),
        GroupDef("文本", kind = "text"),
        GroupDef("图片", kind = "image"),
        GroupDef("链接", patterns = listOf("(?i)^\\s*(https?|ftp)://\\S+\\s*$")),
        GroupDef("固定", pinned = true),
    ),
)
