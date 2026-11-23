package io.github.xiecheesejohn.copym

import org.json.JSONArray
import org.json.JSONObject

data class Item(
    val id: Long,
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
