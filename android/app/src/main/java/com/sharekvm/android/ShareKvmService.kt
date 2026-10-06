package com.sharekvm.android

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.GestureDescription
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.res.Configuration
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Path
import android.graphics.PixelFormat
import android.graphics.Rect
import android.graphics.RectF
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.DisplayMetrics
import android.util.Log
import android.view.Gravity
import android.view.View
import android.view.WindowManager
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import android.view.accessibility.AccessibilityNodeInfo.AccessibilityAction
import org.json.JSONObject
import kotlin.math.abs
import kotlin.math.hypot

/**
 * Carries out what the computer's mouse and keyboard do, using only the
 * Accessibility APIs (no root):
 *  - a pointer drawn over everything,
 *  - clicks and drags as taps and swipes,
 *  - typing into the focused field,
 *  - arrow keys moving focus like a TV remote's D-pad; Esc = Back, Cmd/Win = Home.
 */
class ShareKvmService : AccessibilityService(), Native.Callbacks {

    private val main = Handler(Looper.getMainLooper())
    private lateinit var prefs: Prefs
    private lateinit var wm: WindowManager
    private var pointer: PointerView? = null
    private val pointerParams = WindowManager.LayoutParams(
        WindowManager.LayoutParams.WRAP_CONTENT,
        WindowManager.LayoutParams.WRAP_CONTENT,
        WindowManager.LayoutParams.TYPE_ACCESSIBILITY_OVERLAY,
        WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or
            WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE or
            WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN or
            WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS,
        PixelFormat.TRANSLUCENT,
    ).apply { gravity = Gravity.TOP or Gravity.START }

    // Latest pointer position from the engine; applied at most once per frame.
    @Volatile private var px = 0f
    @Volatile private var py = 0f
    @Volatile private var movePending = false

    // Button held down on the computer: where and when, to tell a tap from a drag.
    private var downX = 0f
    private var downY = 0f
    private var downAt = 0L
    private var downButton: String? = null

    // Arrow-key selection (TV-remote style), with our own highlight ring.
    private var selected: AccessibilityNodeInfo? = null
    private var ring: RingView? = null
    private val ringParams = WindowManager.LayoutParams(
        0, 0,
        WindowManager.LayoutParams.TYPE_ACCESSIBILITY_OVERLAY,
        WindowManager.LayoutParams.FLAG_NOT_FOCUSABLE or
            WindowManager.LayoutParams.FLAG_NOT_TOUCHABLE or
            WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN or
            WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS,
        PixelFormat.TRANSLUCENT,
    ).apply { gravity = Gravity.TOP or Gravity.START }

    // Scroll-wheel steps are batched into one swipe.
    private var scrollDx = 0
    private var scrollDy = 0
    private var scrollAt = Pair(0f, 0f)

    override fun onServiceConnected() {
        instance = this
        prefs = Prefs(this)
        wm = getSystemService(Context.WINDOW_SERVICE) as WindowManager
        pushScreenSize()
        restart()
    }

    override fun onDestroy() {
        Native.stop()
        removePointer()
        clearSelection()
        if (instance === this) instance = null
        Status.update(JSONObject().put("state", "stopped"))
        super.onDestroy()
    }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        pushScreenSize() // rotation changes the screen's width and height
    }

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {
        // A new screen: the old selection no longer applies.
        if (event?.eventType == AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED) clearSelection()
    }
    override fun onInterrupt() {}

    /** (Re)connects with the saved settings, or stops if sharing is off. */
    fun restart() {
        Native.stop()
        if (prefs.ready) {
            Native.start(prefs.configJson(), this)
        } else {
            Status.update(JSONObject().put("state", "stopped"))
        }
    }

    private fun pushScreenSize() {
        val (w, h) = screenSize()
        // Desktop mouse movement comes in points; scale it so the pointer speed feels natural.
        val density = resources.displayMetrics.density
        Native.setScreen(w, h, density)
    }

    @Suppress("DEPRECATION")
    private fun screenSize(): Pair<Int, Int> =
        if (Build.VERSION.SDK_INT >= 30) {
            val b = wm.currentWindowMetrics.bounds
            b.width() to b.height()
        } else {
            val m = DisplayMetrics()
            wm.defaultDisplay.getRealMetrics(m)
            m.widthPixels to m.heightPixels
        }

    // ---------------------------------------------------------------- engine callbacks

    override fun onCursor(x: Float, y: Float) {
        px = x
        py = y
        if (!movePending) {
            movePending = true
            main.post {
                movePending = false
                movePointer(px, py)
            }
        }
    }

    override fun onEvent(json: String) {
        val e = try { JSONObject(json) } catch (_: Exception) { return }
        main.post { handle(e) }
    }

    private fun handle(e: JSONObject) {
        when (e.optString("t")) {
            "show" -> showPointer()
            "hide" -> { hidePointer(); clearSelection() }
            "press" -> onPress(e)
            "release" -> onRelease(e)
            "release_all" -> downButton = null
            "scroll" -> onScroll(e)
            "text" -> typeText(e.optString("s"))
            "key" -> onKey(e.optString("k"))
            "paste" -> focusedEditable()?.performAction(AccessibilityNodeInfo.ACTION_PASTE)
            "clipboard" -> setClipboard(e.optString("s"))
            "status" -> onStatus(e)
            "log" -> Log.i(TAG, e.optString("s"))
        }
    }

    private fun onStatus(e: JSONObject) {
        Log.i(TAG, "status ${e.optString("state")} ${e.optString("peer")} ${e.optString("message")}".trim())
        // Remember who we're connected to, and drop the code once the key is saved.
        if (e.optString("state") == "connected") {
            e.optString("serverId").takeIf { it.isNotEmpty() }?.let { prefs.serverId = it }
            if (e.optBoolean("newlyPaired")) prefs.code = ""
        }
        Status.update(e)
    }

    // ---------------------------------------------------------------- pointer

    private fun showPointer() {
        if (pointer == null) {
            val v = PointerView(this)
            try {
                wm.addView(v, pointerParams)
                pointer = v
            } catch (ex: Exception) {
                Log.w(TAG, "can't show pointer", ex)
                return
            }
        }
        pointer?.visibility = View.VISIBLE
        movePointer(px, py)
    }

    private fun hidePointer() {
        pointer?.visibility = View.GONE
    }

    private fun removePointer() {
        pointer?.let { runCatching { wm.removeView(it) } }
        pointer = null
    }

    private fun movePointer(x: Float, y: Float) {
        val v = pointer ?: return
        pointerParams.x = x.toInt()
        pointerParams.y = y.toInt()
        runCatching { wm.updateViewLayout(v, pointerParams) }
    }

    // ---------------------------------------------------------------- mouse

    private fun onPress(e: JSONObject) {
        clearSelection() // using the mouse again
        downButton = e.optString("b")
        downX = e.optDouble("x").toFloat()
        downY = e.optDouble("y").toFloat()
        downAt = SystemClock.uptimeMillis()
    }

    private fun onRelease(e: JSONObject) {
        val button = downButton ?: return
        downButton = null
        val x = e.optDouble("x").toFloat()
        val y = e.optDouble("y").toFloat()
        val held = SystemClock.uptimeMillis() - downAt
        val moved = hypot(x - downX, y - downY) > TAP_SLOP
        when {
            button == "right" -> gesture(downX, downY, downX, downY, LONG_PRESS_MS) // context menu
            moved -> gesture(downX, downY, x, y, held.coerceIn(120L, 2000L)) // drag / swipe
            held >= LONG_PRESS_MS -> gesture(x, y, x, y, held.coerceAtMost(3000L)) // long press
            else -> tap(x, y)
        }
    }

    private fun onScroll(e: JSONObject) {
        val first = scrollDx == 0 && scrollDy == 0
        scrollDx += e.optInt("dx")
        scrollDy += e.optInt("dy")
        scrollAt = e.optDouble("x").toFloat() to e.optDouble("y").toFloat()
        if (first) main.postDelayed(::flushScroll, SCROLL_BATCH_MS)
    }

    private fun flushScroll() {
        val (x, y) = scrollAt
        val (w, h) = screenSize()
        val step = resources.displayMetrics.density * 60
        // Wheel up (positive) shows earlier content: the finger moves down.
        val dy = (scrollDy * step).coerceIn(-h * 0.45f, h * 0.45f)
        val dx = (-scrollDx * step).coerceIn(-w * 0.45f, w * 0.45f)
        scrollDx = 0
        scrollDy = 0
        val sx = x.coerceIn(1f, w - 2f)
        val sy = y.coerceIn(1f, h - 2f)
        gesture(sx, sy, (sx + dx).coerceIn(1f, w - 2f), (sy + dy).coerceIn(1f, h - 2f), 180)
    }

    private fun tap(x: Float, y: Float) = gesture(x, y, x, y, 40)

    private fun gesture(x1: Float, y1: Float, x2: Float, y2: Float, ms: Long) {
        val path = Path().apply {
            moveTo(x1.coerceAtLeast(0f), y1.coerceAtLeast(0f))
            if (abs(x2 - x1) > 0.5f || abs(y2 - y1) > 0.5f) lineTo(x2.coerceAtLeast(0f), y2.coerceAtLeast(0f))
        }
        val g = GestureDescription.Builder()
            .addStroke(GestureDescription.StrokeDescription(path, 0, ms.coerceAtLeast(1)))
            .build()
        dispatchGesture(g, null, null)
    }

    // ---------------------------------------------------------------- keyboard

    private fun onKey(k: String) {
        val edit = focusedEditable()
        when (k) {
            "back" -> performGlobalAction(GLOBAL_ACTION_BACK)
            "home" -> performGlobalAction(GLOBAL_ACTION_HOME)
            "backspace" -> edit?.let { editText(it, deleteBefore = true) }
            "delete" -> edit?.let { editText(it, deleteBefore = false) }
            "enter" -> enter(edit)
            "left", "right" ->
                if (edit != null) moveCaret(edit, if (k == "left") -1 else 1) else moveFocus(k)
            "up", "down" -> moveFocus(k)
            "tab" -> moveFocus("down")
            "page_up", "page_down" -> scrollable(focused())?.performAction(
                if (k == "page_up") AccessibilityNodeInfo.ACTION_SCROLL_BACKWARD else AccessibilityNodeInfo.ACTION_SCROLL_FORWARD,
            )
        }
    }

    private fun focused(): AccessibilityNodeInfo? =
        findFocus(AccessibilityNodeInfo.FOCUS_INPUT) ?: findFocus(AccessibilityNodeInfo.FOCUS_ACCESSIBILITY)

    private fun focusedEditable(): AccessibilityNodeInfo? =
        findFocus(AccessibilityNodeInfo.FOCUS_INPUT)?.takeIf { it.isEditable }
            ?: selected?.takeIf { it.isEditable && it.refresh() }

    /**
     * D-pad style navigation that works in touch mode too (where apps refuse to
     * give buttons keyboard focus): pick the nearest actionable item in that
     * direction, highlight it, and give it real focus when the app allows.
     */
    private fun moveFocus(k: String, scrolled: Boolean = false) {
        val nodes = actionable()
        if (nodes.isEmpty()) return
        val current = selected?.takeIf { it.refresh() && it.isVisibleToUser }
            ?: focused()?.takeIf { it.isVisibleToUser }
        val next = if (current == null) {
            nodes.minByOrNull { val r = bounds(it); r.top * 10_000 + r.left }
        } else {
            nearest(bounds(current), nodes.filter { bounds(it) != bounds(current) }, k)
        }
        if (next == null) {
            // Nothing further this way on screen: scroll, then look again (once).
            if (!scrolled && (k == "up" || k == "down")) {
                val scroller = scrollable(current) ?: firstScrollable(rootInActiveWindow)
                val action = if (k == "down") AccessibilityNodeInfo.ACTION_SCROLL_FORWARD else AccessibilityNodeInfo.ACTION_SCROLL_BACKWARD
                if (scroller?.performAction(action) == true) main.postDelayed({ moveFocus(k, scrolled = true) }, 250)
            }
            return
        }
        select(next)
    }

    /** Clickable, focusable or editable items the user could act on, on screen. */
    private fun actionable(): List<AccessibilityNodeInfo> {
        val out = mutableListOf<AccessibilityNodeInfo>()
        fun walk(n: AccessibilityNodeInfo?) {
            n ?: return
            if (!n.isVisibleToUser) return
            val r = bounds(n)
            // Scrolling containers hold the items; they aren't items themselves.
            val container = n.isScrollable && !n.isClickable && !n.isEditable
            if (!container && n.isEnabled && (n.isClickable || n.isEditable || n.isCheckable || n.isFocusable) &&
                r.width() > 4 && r.height() > 4
            ) {
                out += n
                if (!n.isFocusable || n.isClickable) return // a button's children are part of it
            }
            for (i in 0 until n.childCount) walk(n.getChild(i))
        }
        walk(rootInActiveWindow)
        return out
    }

    private fun bounds(n: AccessibilityNodeInfo) = Rect().also { n.getBoundsInScreen(it) }

    /** Android's FocusFinder scoring: strongly prefer straight ahead, then closeness. */
    private fun nearest(from: Rect, nodes: List<AccessibilityNodeInfo>, k: String): AccessibilityNodeInfo? =
        nodes.mapNotNull { n ->
            val r = bounds(n)
            val (major, minor) = when (k) {
                "up" -> (from.top - r.bottom).takeIf { r.centerY() < from.centerY() } to abs(r.centerX() - from.centerX())
                "down" -> (r.top - from.bottom).takeIf { r.centerY() > from.centerY() } to abs(r.centerX() - from.centerX())
                "left" -> (from.left - r.right).takeIf { r.centerX() < from.centerX() } to abs(r.centerY() - from.centerY())
                else -> (r.left - from.right).takeIf { r.centerX() > from.centerX() } to abs(r.centerY() - from.centerY())
            }
            major?.let { n to (13L * maxOf(it, 0) * maxOf(it, 0) + minor.toLong() * minor) }
        }.minByOrNull { it.second }?.first

    private fun select(n: AccessibilityNodeInfo) {
        selected = n
        n.performAction(AccessibilityAction.ACTION_SHOW_ON_SCREEN.id) // scroll it into view
        n.performAction(AccessibilityNodeInfo.ACTION_FOCUS) // real focus, where the app allows it
        main.postDelayed({ if (selected === n) drawRing(n) }, 120) // after any scrolling
    }

    private fun drawRing(n: AccessibilityNodeInfo) {
        n.refresh()
        // If the app shows its own focus (not in touch mode), don't add a second highlight.
        if (n.isFocused && !n.isEditable) { ring?.visibility = View.GONE; return }
        val r = bounds(n)
        val pad = (4 * resources.displayMetrics.density).toInt()
        ringParams.x = r.left - pad
        ringParams.y = r.top - pad
        ringParams.width = r.width() + pad * 2
        ringParams.height = r.height() + pad * 2
        val v = ring ?: RingView(this).also { runCatching { wm.addView(it, ringParams) }; ring = it }
        v.visibility = View.VISIBLE
        runCatching { wm.updateViewLayout(v, ringParams) }
    }

    private fun clearSelection() {
        selected = null
        ring?.visibility = View.GONE
    }

    private fun enter(edit: AccessibilityNodeInfo?) {
        // An item chosen with the arrow keys: click it (or focus it, if it's a text field).
        selected?.takeIf { it.refresh() }?.let { n ->
            if (n.isEditable) {
                n.performAction(AccessibilityNodeInfo.ACTION_CLICK)
                n.performAction(AccessibilityNodeInfo.ACTION_FOCUS)
                return
            }
            var c: AccessibilityNodeInfo? = n
            while (c != null && !c.isClickable) c = c.parent
            (c ?: n).performAction(AccessibilityNodeInfo.ACTION_CLICK)
            return
        }
        if (edit != null) {
            // "Done/Go/Search" on the field's keyboard action, where supported.
            if (Build.VERSION.SDK_INT >= 30 && edit.performAction(AccessibilityAction.ACTION_IME_ENTER.id)) return
        }
        var n = focused()
        while (n != null && !n.isClickable) n = n.parent
        n?.performAction(AccessibilityNodeInfo.ACTION_CLICK)
    }

    private fun firstScrollable(n: AccessibilityNodeInfo?): AccessibilityNodeInfo? {
        n ?: return null
        if (n.isScrollable && n.isVisibleToUser) return n
        for (i in 0 until n.childCount) firstScrollable(n.getChild(i))?.let { return it }
        return null
    }

    private fun scrollable(start: AccessibilityNodeInfo?): AccessibilityNodeInfo? {
        var n = start
        while (n != null && !n.isScrollable) n = n.parent
        return n
    }

    /** Text currently in the field, ignoring a hint shown when it's empty. */
    private fun currentText(node: AccessibilityNodeInfo): String {
        if (Build.VERSION.SDK_INT >= 26 && node.isShowingHintText) return ""
        return node.text?.toString() ?: ""
    }

    private fun selection(node: AccessibilityNodeInfo, len: Int): Pair<Int, Int> {
        var s = node.textSelectionStart
        var e = node.textSelectionEnd
        if (s < 0 || e < 0) { s = len; e = len }
        return minOf(s, e).coerceIn(0, len) to maxOf(s, e).coerceIn(0, len)
    }

    private fun typeText(s: String) {
        val node = focusedEditable() ?: return
        val text = currentText(node)
        val (a, b) = selection(node, text.length)
        setText(node, text.substring(0, a) + s + text.substring(b), a + s.length)
    }

    private fun editText(node: AccessibilityNodeInfo, deleteBefore: Boolean) {
        val text = currentText(node)
        val (a, b) = selection(node, text.length)
        val (from, to) = when {
            a != b -> a to b
            deleteBefore && a > 0 -> a - 1 to a
            !deleteBefore && b < text.length -> b to b + 1
            else -> return
        }
        setText(node, text.substring(0, from) + text.substring(to), from)
    }

    private fun moveCaret(node: AccessibilityNodeInfo, by: Int) {
        val len = currentText(node).length
        val (a, _) = selection(node, len)
        val pos = (a + by).coerceIn(0, len)
        node.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, Bundle().apply {
            putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, pos)
            putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, pos)
        })
    }

    private fun setText(node: AccessibilityNodeInfo, text: String, caret: Int) {
        node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, Bundle().apply {
            putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, text)
        })
        node.refresh()
        node.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, Bundle().apply {
            putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, caret)
            putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, caret)
        })
    }

    private fun setClipboard(text: String) {
        val cm = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        runCatching { cm.setPrimaryClip(ClipData.newPlainText("ShareKVM", text)) }
    }

    /** The highlight around the item chosen with the arrow keys. */
    private class RingView(context: Context) : View(context) {
        private val d = context.resources.displayMetrics.density
        private val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = 0xFF60A5FA.toInt(); style = Paint.Style.STROKE; strokeWidth = 3 * d
        }
        override fun onDraw(canvas: Canvas) {
            val inset = paint.strokeWidth / 2
            canvas.drawRoundRect(RectF(inset, inset, width - inset, height - inset), 8 * d, 8 * d, paint)
        }
    }

    /** A classic arrow pointer with an outline, visible on light and dark screens. */
    private class PointerView(context: Context) : View(context) {
        private val d = context.resources.displayMetrics.density
        private val size = (22 * d).toInt()
        private val path = Path().apply {
            val s = size.toFloat()
            moveTo(1f, 1f)
            lineTo(1f, s * 0.86f)
            lineTo(s * 0.26f, s * 0.64f)
            lineTo(s * 0.42f, s * 0.98f)
            lineTo(s * 0.56f, s * 0.92f)
            lineTo(s * 0.40f, s * 0.58f)
            lineTo(s * 0.70f, s * 0.58f)
            close()
        }
        private val fill = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = 0xFFFFFFFF.toInt(); style = Paint.Style.FILL }
        private val stroke = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = 0xFF000000.toInt(); style = Paint.Style.STROKE; strokeWidth = 1.5f * d; strokeJoin = Paint.Join.ROUND
        }

        override fun onMeasure(w: Int, h: Int) = setMeasuredDimension(size + 2, size + 2)
        override fun onDraw(canvas: Canvas) {
            canvas.drawPath(path, fill)
            canvas.drawPath(path, stroke)
        }
    }

    companion object {
        private const val TAG = "ShareKVM"
        private const val TAP_SLOP = 12f
        private const val LONG_PRESS_MS = 600L
        private const val SCROLL_BATCH_MS = 60L

        /** The running service, if the user has turned it on. */
        @Volatile var instance: ShareKvmService? = null
            private set
    }
}
