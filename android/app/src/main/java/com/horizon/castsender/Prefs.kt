package com.horizon.castsender

import android.content.Context
import org.json.JSONArray
import org.json.JSONObject

/** Settings and the small histories the three tabs share. */
object Prefs {
    private const val FILE = "horizon"
    private const val KEY_HISTORY = "history"
    private var app: Context? = null

    fun init(context: Context) {
        app = context.applicationContext
    }

    private fun sp() = requireNotNull(app) { "Prefs.init first" }.getSharedPreferences(FILE, Context.MODE_PRIVATE)

    /** Long edge of the mirrored screen, in pixels. */
    var maxEdge: Int
        get() = sp().getInt("maxEdge", 1920)
        set(value) = sp().edit().putInt("maxEdge", value).apply()

    /** Capture rate the relay holds before the encoder. */
    var fps: Int
        get() = sp().getInt("fps", 30)
        set(value) = sp().edit().putInt("fps", value).apply()

    var lastHub: String
        get() = sp().getString("lastHub", "").orEmpty()
        set(value) = sp().edit().putString("lastHub", value).apply()

    fun history(): List<HistoryEntry> {
        val raw = sp().getString(KEY_HISTORY, "[]").orEmpty()
        return try {
            val array = JSONArray(raw)
            (0 until array.length()).mapNotNull { index ->
                val item = array.optJSONObject(index) ?: return@mapNotNull null
                HistoryEntry(
                    kind = item.optString("kind"),
                    name = item.optString("name"),
                    address = item.optString("address"),
                    at = item.optLong("at"),
                )
            }
        } catch (_: Exception) {
            emptyList()
        }
    }

    fun history(kind: String): List<HistoryEntry> = history().filter { it.kind == kind }

    fun record(kind: String, name: String, address: String) {
        val entry = HistoryEntry(kind, name, address, System.currentTimeMillis())
        val kept = history()
            .filterNot { it.kind == kind && it.address == address && (kind != "transfer" || it.name == name) }
            .toMutableList()
        kept.add(0, entry)
        val list = JSONArray()
        kept.take(24).forEach { item ->
            list.put(JSONObject().apply {
                put("kind", item.kind)
                put("name", item.name)
                put("address", item.address)
                put("at", item.at)
            })
        }
        sp().edit().putString(KEY_HISTORY, list.toString()).apply()
    }

    fun clearHistory() {
        sp().edit().remove(KEY_HISTORY).apply()
    }
}

data class HistoryEntry(val kind: String, val name: String, val address: String, val at: Long)
