package io.github.xiecheesejohn.copym

import org.json.JSONArray
import org.json.JSONObject

data class Item(
    val id: Long,
    /** plain / url / email / path / color / json / code / image */
    val type: String,
    val label: String,
    /** ARGB of a color item. */
    val color: Long?,
    val isImage: Boolean,
    val preview: String,
    val chars: Int,
    val lines: Int,
    val bytes: Long,
    val width: Int,
    val height: Int,
    val created: Long,
    val pinned: Boolean,
    val source: String?,
    val ocr: String?,
)

data class Peer(val name: String, val addr: String)

data class SyncState(
    val enabled: Boolean,
    val paired: Boolean,
    val name: String,
    val peers: List<Peer>,
    val code: String?,
)

data class Pairing(val code: String, val addrs: List<String>, val expiresAt: Long)

private fun JSONObject.str(key: String): String? = if (!has(key) || isNull(key)) null else getString(key)

fun parseItems(json: String): List<Item> {
    val a = JSONArray(json)
    return List(a.length()) { i ->
        val o = a.getJSONObject(i)
        Item(
            id = o.getLong("id"),
            type = o.optString("type", "plain"),
            label = o.optString("label", "文本"),
            color = if (o.has("color") && !o.isNull("color")) o.getLong("color") else null,
            isImage = o.getString("kind") == "image",
            preview = o.getString("preview"),
            chars = o.getInt("chars"),
            lines = o.getInt("lines"),
            bytes = o.getLong("bytes"),
            width = o.getInt("w"),
            height = o.getInt("h"),
            created = o.getLong("created"),
            pinned = o.getBoolean("pinned"),
            source = o.str("source"),
            ocr = o.str("ocr"),
        )
    }
}

fun parseSync(json: String): SyncState {
    val o = JSONObject(json)
    if (!o.optBoolean("enabled")) return SyncState(false, false, "", emptyList(), null)
    val peers = o.getJSONArray("peers")
    return SyncState(
        enabled = true,
        paired = o.getBoolean("paired"),
        name = o.getString("name"),
        peers = List(peers.length()) { peers.getJSONObject(it).let { p -> Peer(p.getString("name"), p.getString("addr")) } },
        code = o.str("code"),
    )
}

fun parsePairing(json: String): Pairing {
    val o = JSONObject(json)
    val addrs = o.getJSONArray("addrs")
    return Pairing(
        code = o.getString("code"),
        addrs = List(addrs.length()) { addrs.getString(it) },
        expiresAt = System.currentTimeMillis() + o.getLong("expires") * 1000,
    )
}

// ------------------------------------------------------------------ groups --

data class GroupDef(
    val name: String,
    /** "", "text" or "image" */
    val kind: String = "",
    val pinned: Boolean = false,
    val patterns: List<String> = emptyList(),
    /** Regexes on the source: app name or the name of the device it was synced from. */
    val apps: List<String> = emptyList(),
    /** "Everything else": not caught by any group that has patterns/apps. */
    val rest: Boolean = false,
)

data class Groups(val default: String, val list: List<GroupDef>) {
    fun defaultIndex(): Int = list.indexOfFirst { it.name == default }.coerceAtLeast(0)
}

private fun JSONArray?.strings(): List<String> = if (this == null) emptyList() else List(length()) { getString(it) }

fun parseGroups(json: String): Groups {
    val o = JSONObject(json)
    val a = o.optJSONArray("group") ?: JSONArray()
    return Groups(
        default = o.optString("default", ""),
        list = List(a.length()) { i ->
            val g = a.getJSONObject(i)
            GroupDef(
                name = g.optString("name"),
                kind = g.optString("kind", ""),
                pinned = g.optBoolean("pinned"),
                patterns = g.optJSONArray("patterns").strings(),
                apps = g.optJSONArray("apps").strings(),
                rest = g.optBoolean("rest"),
            )
        },
    )
}

fun Groups.toJson(): String = JSONObject().apply {
    put("default", default)
    put("group", JSONArray().apply {
        list.forEach { g ->
            put(JSONObject().apply {
                put("name", g.name)
                put("kind", g.kind)
                put("pinned", g.pinned)
                put("patterns", JSONArray(g.patterns))
                put("apps", JSONArray(g.apps))
                put("rest", g.rest)
            })
        }
    })
}.toString()

fun parseCounts(json: String): List<Int?> {
    val a = JSONArray(json)
    return List(a.length()) { if (a.isNull(it)) null else a.getInt(it) }
}

// ----------------------------------------------------------------- preview --

data class Detail(
    val type: String,
    val label: String,
    val text: String,
    val pretty: String?,
    val colorArgb: Long?,
    val colorValues: List<Pair<String, String>>,
)

fun parseDetail(json: String): Detail {
    val o = JSONObject(json)
    val color = o.optJSONObject("color")
    val values = color?.optJSONArray("values")
    return Detail(
        type = o.getString("type"),
        label = o.optString("label", "图片"),
        text = o.optString("text", ""),
        pretty = o.str("pretty"),
        colorArgb = color?.getLong("argb"),
        colorValues = if (values == null) emptyList() else List(values.length()) {
            values.getJSONArray(it).let { p -> p.getString(0) to p.getString(1) }
        },
    )
}
