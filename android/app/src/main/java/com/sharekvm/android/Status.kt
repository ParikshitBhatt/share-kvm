package com.sharekvm.android

import android.os.Handler
import android.os.Looper
import org.json.JSONObject

/** Latest engine status, shared between the service and the settings screen (main thread). */
object Status {
    var current: JSONObject = JSONObject().put("state", "stopped")
        private set
    private val listeners = mutableSetOf<(JSONObject) -> Unit>()
    private val main = Handler(Looper.getMainLooper())

    fun update(status: JSONObject) = main.post {
        current = status
        listeners.toList().forEach { it(status) }
    }

    fun listen(l: (JSONObject) -> Unit) {
        listeners += l
        l(current)
    }

    fun unlisten(l: (JSONObject) -> Unit) {
        listeners -= l
    }
}
