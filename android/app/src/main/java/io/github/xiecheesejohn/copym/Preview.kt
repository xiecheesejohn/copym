package io.github.xiecheesejohn.copym

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import android.graphics.BitmapFactory
import android.net.Uri
import android.os.PersistableBundle
import android.widget.Toast
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.produceState
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
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

private const val MAX_PREVIEW_SIDE = 2048

/** Type-aware preview of one entry with its actions (long-press on an item). */
@OptIn(ExperimentalMaterial3Api::class, ExperimentalLayoutApi::class)
@Composable
fun PreviewSheet(
    item: Item,
    onDismiss: () -> Unit,
    onCopy: () -> Unit,
    onPin: () -> Unit,
    onShare: () -> Unit,
    onDelete: () -> Unit,
) {
    val ctx = LocalContext.current
    val detail by produceState<Detail?>(null, item.id) {
        value = withContext(Dispatchers.IO) { runCatching { parseDetail(Native.detail(item.id)) }.getOrNull() }
    }
    val image by produceState<ImageBitmap?>(null, item.id) {
        if (!item.isImage) return@produceState
        value = withContext(Dispatchers.IO) { runCatching { Native.image(item.id)?.let(::decodeScaled) }.getOrNull() }
    }
    val scheme = MaterialTheme.colorScheme

    ModalBottomSheet(onDismissRequest = onDismiss, sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)) {
        Column(Modifier.fillMaxWidth().padding(horizontal = 20.dp).navigationBarsPadding().padding(bottom = 12.dp)) {
            // header: type chip + facts
            Row(verticalAlignment = Alignment.CenterVertically) {
                val tint = typeTint(item.type)
                Box(Modifier.clip(RoundedCornerShape(8.dp)).background(tint.copy(alpha = 0.16f)).padding(horizontal = 10.dp, vertical = 3.dp)) {
                    Text(item.label, color = tint, style = MaterialTheme.typography.labelLarge)
                }
                Spacer(Modifier.width(10.dp))
                Text(
                    listOfNotNull(
                        if (item.isImage) "${item.width} × ${item.height}" else "${item.chars} 字符 · ${item.lines} 行",
                        humanSize(item.bytes),
                        item.source?.let { "来自 $it" },
                        relativeTime(item.created),
                    ).joinToString(" · "),
                    style = MaterialTheme.typography.labelMedium,
                    color = scheme.onSurfaceVariant,
                )
            }
            Spacer(Modifier.height(14.dp))

            Box(Modifier.fillMaxWidth().heightIn(min = 60.dp, max = 460.dp)) {
                val d = detail
                when {
                    item.isImage -> ImagePreview(image, item)
                    d == null -> Text("加载中…", color = scheme.onSurfaceVariant)
                    d.type == "color" && d.colorArgb != null -> ColorPreview(d)
                    d.type == "url" -> UrlPreview(d.text.trim())
                    d.type == "json" || d.type == "code" -> MonoBlock(d.pretty ?: d.text)
                    d.type == "path" || d.type == "email" -> SelectionContainer {
                        Text(d.text.trim(), fontFamily = FontFamily.Monospace, fontSize = 15.sp)
                    }
                    else -> Column(Modifier.verticalScroll(rememberScrollState())) {
                        SelectionContainer { Text(d.text, style = MaterialTheme.typography.bodyLarge) }
                    }
                }
            }
            Spacer(Modifier.height(16.dp))

            FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                Button(onClick = onCopy) { Text("复制") }
                val d = detail
                if (d?.type == "url") {
                    FilledTonalButton(onClick = { open(ctx, Intent(Intent.ACTION_VIEW, Uri.parse(d.text.trim()))) }) { Text("在浏览器打开") }
                }
                if (d?.type == "email") {
                    FilledTonalButton(onClick = { open(ctx, Intent(Intent.ACTION_SENDTO, Uri.parse("mailto:" + d.text.trim()))) }) { Text("写邮件") }
                }
                OutlinedButton(onClick = onPin) { Text(if (item.pinned) "取消固定" else "固定") }
                OutlinedButton(onClick = onShare) { Text("分享") }
                OutlinedButton(onClick = onDelete) { Text("删除", color = scheme.error) }
            }
        }
    }
}

private fun open(ctx: android.content.Context, intent: Intent) {
    try {
        ctx.startActivity(intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
    } catch (_: Exception) {
        Toast.makeText(ctx, "没有可以打开它的应用", Toast.LENGTH_SHORT).show()
    }
}

/** Decode with subsampling so huge screenshots don't blow up memory. */
private fun decodeScaled(bytes: ByteArray): ImageBitmap? {
    val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
    BitmapFactory.decodeByteArray(bytes, 0, bytes.size, bounds)
    var sample = 1
    while (maxOf(bounds.outWidth, bounds.outHeight) / sample > MAX_PREVIEW_SIDE) sample *= 2
    return BitmapFactory.decodeByteArray(bytes, 0, bytes.size, BitmapFactory.Options().apply { inSampleSize = sample })?.asImageBitmap()
}

@Composable
private fun ImagePreview(image: ImageBitmap?, item: Item) {
    Column(Modifier.verticalScroll(rememberScrollState())) {
        Box(
            Modifier.fillMaxWidth().heightIn(min = 120.dp, max = 360.dp).clip(RoundedCornerShape(12.dp)).background(MaterialTheme.colorScheme.surfaceVariant),
            contentAlignment = Alignment.Center,
        ) {
            if (image != null) {
                Image(image, null, contentScale = ContentScale.Fit, modifier = Modifier.fillMaxWidth().heightIn(max = 360.dp))
            } else {
                Text("加载中…", color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
        }
        item.ocr?.takeIf { it.isNotBlank() }?.let {
            Spacer(Modifier.height(10.dp))
            Text("图中文字", style = MaterialTheme.typography.labelLarge, color = MaterialTheme.colorScheme.onSurfaceVariant)
            SelectionContainer { Text(it, style = MaterialTheme.typography.bodyMedium) }
        }
    }
}

@Composable
private fun ColorPreview(d: Detail) {
    val ctx = LocalContext.current
    val shape = RoundedCornerShape(14.dp)
    Column {
        Box(
            Modifier.fillMaxWidth().height(120.dp).clip(shape).background(Color(d.colorArgb!!)).border(1.dp, MaterialTheme.colorScheme.outlineVariant, shape),
        )
        Spacer(Modifier.height(12.dp))
        d.colorValues.forEach { (k, v) ->
            Row(
                Modifier.fillMaxWidth().clip(RoundedCornerShape(8.dp)).clickable { copyPlain(ctx, v) }.padding(vertical = 8.dp, horizontal = 4.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(k, modifier = Modifier.width(48.dp), color = MaterialTheme.colorScheme.onSurfaceVariant, style = MaterialTheme.typography.labelLarge)
                Text(v, fontFamily = FontFamily.Monospace, fontSize = 15.sp, modifier = Modifier.weight(1f))
                Text("复制", color = MaterialTheme.colorScheme.primary, style = MaterialTheme.typography.labelMedium)
            }
        }
    }
}

@Composable
private fun UrlPreview(url: String) {
    val (host, path) = splitUrl(url)
    Column(Modifier.verticalScroll(rememberScrollState())) {
        Text(host, fontSize = 22.sp, fontWeight = FontWeight.SemiBold)
        if (path.length > 1) {
            Spacer(Modifier.height(4.dp))
            Text(path, fontFamily = FontFamily.Monospace, fontSize = 13.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        Spacer(Modifier.height(10.dp))
        SelectionContainer { Text(url, fontFamily = FontFamily.Monospace, fontSize = 12.sp, color = MaterialTheme.colorScheme.outline) }
    }
}

@Composable
private fun MonoBlock(text: String) {
    Box(
        Modifier.fillMaxWidth().clip(RoundedCornerShape(12.dp)).background(MaterialTheme.colorScheme.surfaceContainerHighest)
            .verticalScroll(rememberScrollState()).horizontalScroll(rememberScrollState()).padding(12.dp),
    ) {
        SelectionContainer { Text(text.take(50_000), fontFamily = FontFamily.Monospace, fontSize = 13.sp, softWrap = false) }
    }
}

/** Copy a derived value (e.g. a color notation) without recording it again. */
private fun copyPlain(ctx: android.content.Context, text: String) {
    val cm = ctx.getSystemService(ClipboardManager::class.java) ?: return
    val clip = ClipData.newPlainText("copym", text)
    clip.description.extras = PersistableBundle().apply { putBoolean("io.github.xiecheesejohn.copym.OWN", true) }
    cm.setPrimaryClip(clip)
    Toast.makeText(ctx, "已复制 $text", Toast.LENGTH_SHORT).show()
}
