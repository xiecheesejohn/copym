package io.github.xiecheesejohn.copym

import android.app.Application
import android.content.ClipData
import android.content.ClipDescription
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.Uri
import android.net.wifi.WifiManager
import android.os.Build
import android.os.PersistableBundle
import android.provider.Settings
import androidx.core.content.FileProvider
import java.io.ByteArrayOutputStream
import java.io.File

class CopymApp : Application() {
    override fun onCreate() {
        super.onCreate()
        Core.init(this)
    }
}

class Prefs(ctx: Context) {
    private val p = ctx.getSharedPreferences("copym", Context.MODE_PRIVATE)
    var backgroundSync: Boolean
        get() = p.getBoolean("background_sync", false)
        set(v) = p.edit().putBoolean("background_sync", v).apply()
    /** Manual peer addresses (IP or IP:port), for networks that block mDNS. */
    var peers: String
        get() = p.getString("peers", "") ?: ""
        set(v) = p.edit().putString("peers", v).apply()
    var lastClipStamp: Long
        get() = p.getLong("last_clip_stamp", 0)
        set(v) = p.edit().putLong("last_clip_stamp", v).apply()
}

object Core {
    @Volatile var error: String? = null
        private set
    @Volatile var ready = false
        private set
    private var multicast: WifiManager.MulticastLock? = null

    @Synchronized
    fun init(ctx: Context) {
        if (ready) return
        val app = ctx.applicationContext
        // Android drops multicast packets unless an app holds this lock (mDNS).
        multicast = app.getSystemService(WifiManager::class.java)
            ?.createMulticastLock("copym-mdns")
            ?.apply { setReferenceCounted(false); acquire() }
        try {
            Native.open(File(app.filesDir, "copym").absolutePath, deviceName(app), Prefs(app).peers)
            ready = true
        } catch (e: Throwable) {
            error = e.message ?: e.toString()
        }
    }

    fun deviceName(ctx: Context): String {
        val named = Settings.Global.getString(ctx.contentResolver, "device_name")
        if (!named.isNullOrBlank()) return named
        val maker = Build.MANUFACTURER.replaceFirstChar { it.uppercase() }
        return if (Build.MODEL.startsWith(maker, ignoreCase = true)) Build.MODEL else "$maker ${Build.MODEL}"
    }
}

/** Clipboard read/write. Android only lets the focused app read the clipboard. */
object Clip {
    private const val OWN = "io.github.xiecheesejohn.copym.OWN"
    private const val SENSITIVE = "android.content.extra.IS_SENSITIVE"
    private const val MAX_IMAGE_SIDE = 4096

    /** Snapshot taken on the main thread while focused; processed later. */
    class Snapshot(val clip: ClipData?, val stamp: Long)

    fun snapshot(ctx: Context): Snapshot {
        val cm = ctx.getSystemService(ClipboardManager::class.java)
        val clip = try { cm?.primaryClip } catch (_: SecurityException) { null }
        return Snapshot(clip, clip?.description?.timestamp ?: 0)
    }

    /**
     * Save the snapshot into history. `force` = user explicitly asked (tile),
     * otherwise unchanged clipboards are skipped. Returns a message to show,
     * or null for "nothing to say". Call off the main thread.
     */
    fun save(ctx: Context, snap: Snapshot, force: Boolean): String? {
        val clip = snap.clip ?: return if (force) "剪贴板是空的" else null
        val desc = clip.description
        val extras = desc.extras
        val prefs = Prefs(ctx)
        if (!force && snap.stamp != 0L && snap.stamp == prefs.lastClipStamp) return null
        prefs.lastClipStamp = snap.stamp
        if (extras?.getBoolean(OWN) == true) return if (force) "已经在历史中" else null
        if (extras?.getBoolean(SENSITIVE) == true) return "已跳过敏感内容（如密码）"
        if (clip.itemCount == 0) return if (force) "剪贴板是空的" else null
        val item = clip.getItemAt(0)
        return try {
            val uri = item.uri
            if (uri != null && (desc.hasMimeType("image/*") || ctx.contentResolver.getType(uri)?.startsWith("image/") == true)) {
                val png = readImageAsPng(ctx, uri) ?: return "无法读取图片"
                if (Native.addImage(png, null)) "已保存图片" else "图片太大，未保存"
            } else {
                val text = item.text?.toString() ?: item.coerceToText(ctx)?.toString().orEmpty()
                if (Native.addText(text, null)) "已保存到 copym" else if (force) "没有可保存的文字" else null
            }
        } catch (e: Exception) {
            "保存失败：${e.message}"
        }
    }

    fun saveText(text: String): String =
        try { if (Native.addText(text, null)) "已保存到 copym" else "没有可保存的文字" } catch (e: Exception) { "保存失败：${e.message}" }

    fun saveImage(ctx: Context, uri: Uri): String = try {
        val png = readImageAsPng(ctx, uri)
        when {
            png == null -> "无法读取图片"
            Native.addImage(png, null) -> "已保存图片"
            else -> "图片太大，未保存"
        }
    } catch (e: Exception) {
        "保存失败：${e.message}"
    }

    /** Decode any image the system understands and re-encode as PNG. */
    fun readImageAsPng(ctx: Context, uri: Uri): ByteArray? {
        val raw = ctx.contentResolver.openInputStream(uri)?.use { it.readBytes() } ?: return null
        if (raw.size > 8 && raw[0] == 0x89.toByte() && raw[1] == 'P'.code.toByte()) return raw
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        BitmapFactory.decodeByteArray(raw, 0, raw.size, bounds)
        var sample = 1
        while (maxOf(bounds.outWidth, bounds.outHeight) / sample > MAX_IMAGE_SIDE) sample *= 2
        val bmp = BitmapFactory.decodeByteArray(raw, 0, raw.size, BitmapFactory.Options().apply { inSampleSize = sample })
            ?: return null
        return ByteArrayOutputStream().use { out ->
            bmp.compress(Bitmap.CompressFormat.PNG, 100, out)
            bmp.recycle()
            out.toByteArray()
        }
    }

    private fun imageUri(ctx: Context, item: Item): Uri? {
        val png = Native.image(item.id) ?: return null
        val dir = File(ctx.cacheDir, "clips").apply { mkdirs() }
        dir.listFiles()?.forEach { it.delete() }
        val f = File(dir, "copym-${item.id}.png").apply { writeBytes(png) }
        return FileProvider.getUriForFile(ctx, "${ctx.packageName}.files", f)
    }

    /** Put an entry on the system clipboard and move it to the top. */
    fun copy(ctx: Context, item: Item): Boolean {
        val cm = ctx.getSystemService(ClipboardManager::class.java) ?: return false
        val clip = if (item.isImage) {
            ClipData.newUri(ctx.contentResolver, "copym", imageUri(ctx, item) ?: return false)
        } else {
            ClipData.newPlainText("copym", Native.text(item.id) ?: return false)
        }
        clip.description.extras = PersistableBundle().apply { putBoolean(OWN, true) }
        cm.setPrimaryClip(clip)
        Native.touch(item.id)
        return true
    }

    fun share(ctx: Context, item: Item) {
        val send = Intent(Intent.ACTION_SEND)
        if (item.isImage) {
            val uri = imageUri(ctx, item) ?: return
            send.type = "image/png"
            send.putExtra(Intent.EXTRA_STREAM, uri)
            send.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        } else {
            send.type = ClipDescription.MIMETYPE_TEXT_PLAIN
            send.putExtra(Intent.EXTRA_TEXT, Native.text(item.id) ?: return)
        }
        ctx.startActivity(Intent.createChooser(send, null).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
    }
}
