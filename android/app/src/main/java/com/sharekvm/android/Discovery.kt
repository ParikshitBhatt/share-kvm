package com.sharekvm.android

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.net.wifi.WifiManager
import android.os.Handler
import android.os.Looper
import java.net.Inet4Address

/** A computer sharing its mouse, seen on the network. */
data class Computer(val id: String, val name: String, val host: String, val port: Int, val version: Int)

/**
 * Finds ShareKVM computers with Android's built-in mDNS/DNS-SD (`_sharekvm._tcp`).
 * Results arrive on the main thread.
 */
class Discovery(context: Context, private val onChange: (List<Computer>) -> Unit) {
    private val nsd = context.getSystemService(Context.NSD_SERVICE) as NsdManager
    private val wifi = context.applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
    private val main = Handler(Looper.getMainLooper())
    private val found = linkedMapOf<String, Computer>() // by service name
    private val toResolve = ArrayDeque<NsdServiceInfo>()
    private var resolving = false
    private var lock: WifiManager.MulticastLock? = null
    private var listener: NsdManager.DiscoveryListener? = null

    fun start() {
        if (listener != null) return
        // Some devices drop multicast packets unless an app holds this lock.
        lock = wifi.createMulticastLock("sharekvm").apply { setReferenceCounted(false); acquire() }
        val l = object : NsdManager.DiscoveryListener {
            override fun onServiceFound(info: NsdServiceInfo) = main.post { queue(info) }.let { }
            override fun onServiceLost(info: NsdServiceInfo) = main.post {
                if (found.remove(info.serviceName) != null) publish()
            }.let { }
            override fun onDiscoveryStarted(type: String) {}
            override fun onDiscoveryStopped(type: String) {}
            override fun onStartDiscoveryFailed(type: String, code: Int) {}
            override fun onStopDiscoveryFailed(type: String, code: Int) {}
        }
        listener = l
        nsd.discoverServices(SERVICE, NsdManager.PROTOCOL_DNS_SD, l)
    }

    fun stop() {
        listener?.let { runCatching { nsd.stopServiceDiscovery(it) } }
        listener = null
        lock?.let { runCatching { it.release() } }
        lock = null
    }

    // Older Android resolves one service at a time, so queue them.
    private fun queue(info: NsdServiceInfo) {
        toResolve.addLast(info)
        next()
    }

    @Suppress("DEPRECATION")
    private fun next() {
        if (resolving) return
        val info = toResolve.removeFirstOrNull() ?: return
        resolving = true
        nsd.resolveService(info, object : NsdManager.ResolveListener {
            override fun onResolveFailed(info: NsdServiceInfo, code: Int) = main.post { resolving = false; next() }.let { }
            override fun onServiceResolved(info: NsdServiceInfo) = main.post {
                resolving = false
                toComputer(info)?.let { found[info.serviceName] = it; publish() }
                next()
            }.let { }
        })
    }

    @Suppress("DEPRECATION")
    private fun toComputer(info: NsdServiceInfo): Computer? {
        val attr = info.attributes
        val text = { k: String -> attr[k]?.let { String(it, Charsets.UTF_8) } }
        val id = text("id")?.takeIf { it.length == 32 } ?: return null
        val addr = info.host ?: return null
        val host = if (addr is Inet4Address) addr.hostAddress else "[${addr.hostAddress?.substringBefore('%')}]"
        return Computer(id, text("name") ?: info.serviceName, host ?: return null, info.port, text("v")?.toIntOrNull() ?: 0)
    }

    private fun publish() = onChange(found.values.sortedBy { it.name.lowercase() })

    companion object {
        private const val SERVICE = "_sharekvm._tcp"
    }
}
