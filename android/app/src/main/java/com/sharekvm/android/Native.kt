package com.sharekvm.android

/** The Rust engine (libsharekvm_android.so): pairing, encryption and the session. */
object Native {
    init {
        System.loadLibrary("sharekvm_android")
    }

    /** Called from the engine's thread. */
    interface Callbacks {
        fun onCursor(x: Float, y: Float)
        fun onEvent(json: String)
    }

    /** Starts (or restarts) the session. `config`: {dataDir, server, serverId, code, name}. */
    @JvmStatic external fun start(config: String, callbacks: Callbacks)
    @JvmStatic external fun stop()
    /** Screen size in pixels, and density (pixels per desktop point of mouse movement). */
    @JvmStatic external fun setScreen(width: Int, height: Int, density: Float)
    @JvmStatic external fun deviceId(dataDir: String): String
    /** Remembered computers as a JSON array: [{id, name, address, lastSeen}]. */
    @JvmStatic external fun paired(dataDir: String): String
    @JvmStatic external fun forget(dataDir: String, id: String): Boolean
}
