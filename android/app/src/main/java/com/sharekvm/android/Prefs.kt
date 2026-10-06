package com.sharekvm.android

import android.content.Context
import android.os.Build
import org.json.JSONObject

/** Saved settings. The session starts automatically whenever these allow it. */
class Prefs(context: Context) {
    private val p = context.getSharedPreferences("sharekvm", Context.MODE_PRIVATE)
    private val dataDir = context.filesDir.absolutePath

    var enabled: Boolean
        get() = p.getBoolean("enabled", true)
        set(v) = p.edit().putBoolean("enabled", v).apply()
    var server: String
        get() = p.getString("server", "") ?: ""
        set(v) = p.edit().putString("server", v.trim()).apply()
    var serverId: String
        get() = p.getString("serverId", "") ?: ""
        set(v) = p.edit().putString("serverId", v).apply()
    /** Only needed for the first pairing; cleared once paired. */
    var code: String
        get() = p.getString("code", "") ?: ""
        set(v) = p.edit().putString("code", v.trim()).apply()
    var name: String
        get() = p.getString("name", null) ?: defaultName()
        set(v) = p.edit().putString("name", v.trim()).apply()

    val ready: Boolean get() = enabled && server.isNotBlank()

    fun configJson(): String = JSONObject()
        .put("dataDir", dataDir)
        .put("server", server)
        .put("serverId", serverId)
        .put("code", code)
        .put("name", name)
        .toString()

    companion object {
        fun defaultName(): String {
            val model = Build.MODEL ?: "Android"
            val maker = Build.MANUFACTURER ?: ""
            return if (model.startsWith(maker, ignoreCase = true)) model else "$maker $model".trim()
        }
    }
}
