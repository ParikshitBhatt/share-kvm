package com.sharekvm.android

import android.app.Activity
import android.content.ComponentName
import android.content.Intent
import android.graphics.Typeface
import android.graphics.drawable.GradientDrawable
import android.os.Bundle
import android.provider.Settings
import android.text.InputType
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.Button
import android.widget.EditText
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Switch
import android.widget.TextView
import org.json.JSONArray
import org.json.JSONObject

/**
 * Settings, for phones (touch) and Android TV (D-pad). Everything else happens
 * in [ShareKvmService]; this screen is only needed to set things up or change them.
 */
class MainActivity : Activity() {
    private lateinit var prefs: Prefs
    private lateinit var discovery: Discovery
    private lateinit var statusText: TextView
    private lateinit var statusDetail: TextView
    private lateinit var accessState: TextView
    private lateinit var accessButton: Button
    private lateinit var sharing: Switch
    private lateinit var computers: LinearLayout
    private lateinit var computersEmpty: TextView
    private lateinit var remembered: LinearLayout
    private lateinit var address: EditText
    private lateinit var code: EditText
    private lateinit var name: EditText
    private var found: List<Computer> = emptyList()
    private val onStatus: (JSONObject) -> Unit = { showStatus(it) }

    private val dp get() = resources.displayMetrics.density
    private fun px(v: Int) = (v * dp).toInt()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        prefs = Prefs(this)
        discovery = Discovery(this) { found = it; showComputers() }
        setContentView(build())
    }

    override fun onResume() {
        super.onResume()
        Status.listen(onStatus)
        discovery.start()
        refresh()
    }

    override fun onPause() {
        Status.unlisten(onStatus)
        discovery.stop()
        saveTyped()
        super.onPause()
    }

    // ---------------------------------------------------------------- layout

    private fun build(): View {
        val col = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(px(24), px(20), px(24), px(32))
        }

        col += title("ShareKVM", 28f)
        statusText = text("", 18f, bold = true)
        statusDetail = text("", 14f, muted = true)
        col += statusText
        col += statusDetail

        col += heading("1 · Let ShareKVM control this device")
        accessState = text("", 15f)
        col += accessState
        accessButton = button("Open Accessibility settings") {
            startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
        }
        col += accessButton
        col += text(
            "Turn on “ShareKVM pointer and keyboard”. It shows a pointer, taps where you click and types what you type, " +
                "only while your computer's cursor is on this screen.",
            13f, muted = true,
        )

        col += heading("2 · Pick the computer to share with")
        sharing = Switch(this).apply {
            text = "Sharing"
            textSize = 17f
            setPadding(0, px(8), 0, px(8))
            setOnCheckedChangeListener { _, on ->
                if (prefs.enabled != on) {
                    prefs.enabled = on
                    ShareKvmService.instance?.restart()
                }
            }
        }
        col += sharing
        col += text("Computers on your network", 15f, bold = true)
        computers = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        col += computers
        computersEmpty = text(
            "Looking… On the computer, open ShareKVM and choose “Share my mouse & keyboard”. Both must be on the same Wi-Fi.",
            13f, muted = true,
        )
        col += computersEmpty

        col += text("Or connect by address", 15f, bold = true).apply { setPadding(0, px(16), 0, 0) }
        address = field("Address, e.g. 192.168.1.20", InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_URI)
        code = field("Pairing code (first time only)", InputType.TYPE_CLASS_NUMBER)
        col += address
        col += code
        col += button("Connect") { connect(address.text.toString(), "") }

        col += heading("This device")
        name = field("Name shown on the computer", InputType.TYPE_CLASS_TEXT)
        col += name

        col += heading("Remembered computers")
        remembered = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        col += remembered

        col += heading("Using it")
        col += text(
            "Move your computer's cursor off the edge you chose in ShareKVM on the computer. " +
                "Click = tap · drag = swipe · right-click = long press · scroll wheel = scroll\n" +
                "Arrow keys move the selection (like a TV remote) · Enter = select · Esc = Back · tap Cmd/Win alone = Home\n" +
                "Typing goes into the selected text field · Cmd/Ctrl+V pastes text copied on the computer",
            13f, muted = true,
        )

        return ScrollView(this).apply {
            addView(col)
            isFillViewport = true
        }
    }

    // ---------------------------------------------------------------- state

    private fun refresh() {
        val on = isServiceOn()
        accessState.text = if (on) "✓ On" else "Off: ShareKVM can't control this device yet."
        accessState.setTextColor(if (on) GOOD else BAD)
        accessButton.text = if (on) "Accessibility settings" else "Turn on in Accessibility settings"
        sharing.isChecked = prefs.enabled
        if (!address.hasFocus()) address.setText(prefs.server)
        if (!code.hasFocus()) code.setText(prefs.code)
        if (!name.hasFocus()) name.setText(prefs.name)
        showComputers()
        showRemembered()
        showStatus(Status.current)
    }

    private fun isServiceOn(): Boolean {
        if (ShareKvmService.instance != null) return true
        val me = ComponentName(this, ShareKvmService::class.java).flattenToString()
        val enabled = Settings.Secure.getString(contentResolver, Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES) ?: ""
        return enabled.split(':').any { it.equals(me, ignoreCase = true) }
    }

    private fun showStatus(s: JSONObject) {
        val peer = s.optString("peer").ifEmpty { "the computer" }
        val (line, color) = when (s.optString("state")) {
            "connecting" -> "Connecting to ${s.optString("addr").ifEmpty { prefs.server }}…" to WAIT
            "connected" -> "Connected to $peer" to GOOD
            "active" -> "Being controlled by $peer" to GOOD
            "error" -> "Needs attention" to BAD
            else -> when {
                !isServiceOn() -> "Not set up yet" to MUTED
                !prefs.enabled -> "Sharing is off" to MUTED
                prefs.server.isBlank() -> "Pick a computer below" to MUTED
                else -> "Starting…" to WAIT
            }
        }
        statusText.text = line
        statusText.setTextColor(color)
        statusDetail.text = s.optString("message")
        statusDetail.visibility = if (statusDetail.text.isNullOrEmpty()) View.GONE else View.VISIBLE
        if (s.optString("state") == "connected") showRemembered()
    }

    private fun showComputers() {
        computers.removeAllViews()
        computersEmpty.visibility = if (found.isEmpty()) View.VISIBLE else View.GONE
        val pairedIds = pairedList().map { it.optString("id") }.toSet()
        for (c in found) {
            val tag = when {
                c.version != PROTOCOL_VERSION -> "different version: update ShareKVM"
                c.id in pairedIds -> "paired"
                else -> "new: needs its pairing code"
            }
            val current = c.id == prefs.serverId
            computers += button("${if (current) "● " else ""}${c.name}   ·   ${c.host}   ·   $tag") {
                if (c.version != PROTOCOL_VERSION) return@button
                val addr = if (c.port == DEFAULT_PORT) c.host else "${c.host}:${c.port}"
                address.setText(addr)
                if (c.id !in pairedIds && code.text.isBlank()) {
                    prefs.server = addr
                    prefs.serverId = c.id
                    statusDetail.text = "Enter the pairing code shown on ${c.name}, then press Connect."
                    statusDetail.visibility = View.VISIBLE
                    code.requestFocus()
                } else {
                    connect(addr, c.id)
                }
            }.apply { gravity = Gravity.START or Gravity.CENTER_VERTICAL; isAllCaps = false }
        }
    }

    private fun pairedList(): List<JSONObject> = runCatching {
        val a = JSONArray(Native.paired(filesDir.absolutePath))
        (0 until a.length()).map { a.getJSONObject(it) }
    }.getOrDefault(emptyList())

    private fun showRemembered() {
        remembered.removeAllViews()
        val list = pairedList()
        if (list.isEmpty()) {
            remembered += text("None yet. After the first connection, this device reconnects without a code.", 13f, muted = true)
        }
        for (p in list) {
            val row = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL; gravity = Gravity.CENTER_VERTICAL }
            row.addView(text("${p.optString("name")}  ·  ${p.optString("address").removeSuffix(":$DEFAULT_PORT")}", 15f),
                LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
            row.addView(button("Forget") {
                Native.forget(filesDir.absolutePath, p.optString("id"))
                if (prefs.serverId == p.optString("id")) prefs.serverId = ""
                ShareKvmService.instance?.restart()
                showRemembered()
                showComputers()
            })
            remembered += row
        }
    }

    private fun saveTyped() {
        prefs.code = code.text.toString()
        name.text.toString().takeIf { it.isNotBlank() && it != prefs.name }?.let {
            prefs.name = it
            ShareKvmService.instance?.restart()
        }
    }

    private fun connect(addr: String, id: String) {
        if (addr.isBlank()) {
            address.error = "Enter the computer's address"
            return
        }
        if (addr.trim() != prefs.server) prefs.serverId = id // typed by hand: not tied to a found computer
        if (id.isNotEmpty()) prefs.serverId = id
        prefs.server = addr
        prefs.code = code.text.toString()
        name.text.toString().takeIf { it.isNotBlank() }?.let { prefs.name = it }
        prefs.enabled = true
        sharing.isChecked = true
        val service = ShareKvmService.instance
        if (service == null) {
            statusDetail.text = "Saved. Now turn on ShareKVM in Accessibility settings (step 1) and it connects automatically."
            statusDetail.visibility = View.VISIBLE
            accessButton.requestFocus()
        } else {
            service.restart()
        }
        showComputers()
    }

    // ---------------------------------------------------------------- tiny view helpers

    private operator fun LinearLayout.plusAssign(v: View) {
        addView(v, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT).apply {
            topMargin = px(6)
        })
    }

    private fun title(s: String, size: Float) = text(s, size, bold = true)

    private fun heading(s: String) = text(s, 13f, bold = true, muted = true).apply {
        setPadding(0, px(22), 0, px(2))
        letterSpacing = 0.05f
    }

    private fun text(s: String, size: Float, bold: Boolean = false, muted: Boolean = false) = TextView(this).apply {
        text = s
        setTextSize(TypedValue.COMPLEX_UNIT_SP, size)
        if (bold) typeface = Typeface.DEFAULT_BOLD
        if (muted) setTextColor(MUTED)
    }

    private fun field(hint: String, type: Int) = EditText(this).apply {
        this.hint = hint
        inputType = type
        isSingleLine = true
    }

    private fun button(label: String, onClick: () -> Unit) = Button(this).apply {
        text = label
        isAllCaps = false
        setOnClickListener { onClick() }
        // Make the D-pad focus obvious on TV.
        setOnFocusChangeListener { v, has ->
            v.background?.alpha = if (has) 255 else 200
            (v as Button).typeface = if (has) Typeface.DEFAULT_BOLD else Typeface.DEFAULT
        }
        background?.let { if (it is GradientDrawable) it.cornerRadius = px(10).toFloat() }
    }

    companion object {
        /** Must match the desktop engine's protocol version. */
        const val PROTOCOL_VERSION = 4
        const val DEFAULT_PORT = 24801
        private const val GOOD = 0xFF22C55E.toInt()
        private const val BAD = 0xFFF87171.toInt()
        private const val WAIT = 0xFFFBBF24.toInt()
        private const val MUTED = 0xFF9AA1B1.toInt()
    }
}
