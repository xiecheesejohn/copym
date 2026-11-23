package io.github.xiecheesejohn.copym

/**
 * Bindings to the Rust core (`crates/copym-android`). Errors are thrown as
 * [RuntimeException]; structured results are JSON strings (see [Model.kt]).
 * [join] blocks for up to ~10 s, everything else is fast.
 */
object Native {
    init {
        System.loadLibrary("copym_android")
    }

    @JvmStatic external fun open(dataDir: String, deviceName: String, peers: String)
    @JvmStatic external fun version(): Long
    @JvmStatic external fun list(query: String, regex: Boolean, filter: Int, limit: Int): String
    @JvmStatic external fun text(id: Long): String?
    @JvmStatic external fun image(id: Long): ByteArray?
    @JvmStatic external fun thumb(id: Long): ByteArray?
    @JvmStatic external fun addText(text: String, source: String?): Boolean
    @JvmStatic external fun addImage(png: ByteArray, source: String?): Boolean
    @JvmStatic external fun touch(id: Long)
    @JvmStatic external fun delete(id: Long)
    @JvmStatic external fun togglePin(id: Long)
    @JvmStatic external fun clear()
    @JvmStatic external fun syncStatus(): String
    @JvmStatic external fun pairStart(): String
    @JvmStatic external fun join(code: String): String
    @JvmStatic external fun leave()

    // groups (same model and groups.toml format as the desktop app)
    @JvmStatic external fun groups(): String
    @JvmStatic external fun setGroups(json: String)
    @JvmStatic external fun listGroup(query: String, regex: Boolean, group: Int, limit: Int): String
    @JvmStatic external fun groupCounts(query: String, regex: Boolean): String
    /** Items an unsaved draft group would contain; -1 = invalid rule. */
    @JvmStatic external fun draftCount(groupsJson: String, index: Int): Int
    /** Full content prepared for the preview (type, text, pretty JSON, color values). */
    @JvmStatic external fun detail(id: Long): String
}
