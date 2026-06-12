package com.localfocus.local_focus_mobile

import android.accessibilityservice.AccessibilityService
import android.os.Handler
import android.os.Looper
import android.view.accessibility.AccessibilityEvent
import java.net.HttpURLConnection
import java.net.URL

/**
 * On-device app blocking — the Android equivalent of the Mac app force-quitting a
 * blocked app. It periodically reads the block rules and the master stop flag from
 * the embedded server's /api/state, and when the foreground app matches a rule it
 * bounces the user back to the home screen.
 *
 * App/keyword blocks work fully. Blocking a specific website inside a browser is
 * limited (it would require reading the address bar); that is a later enhancement.
 */
class LocalFocusAccessibilityService : AccessibilityService() {
    private val handler = Handler(Looper.getMainLooper())

    @Volatile private var blockedTargets: List<String> = emptyList()
    @Volatile private var stopped: Boolean = false
    private var lastBlockedPackage = ""
    private var lastBlockAt = 0L

    private val poller = object : Runnable {
        override fun run() {
            Thread { refreshState() }.start()
            handler.postDelayed(this, 4_000)
        }
    }

    override fun onServiceConnected() {
        super.onServiceConnected()
        handler.post(poller)
    }

    override fun onDestroy() {
        handler.removeCallbacks(poller)
        super.onDestroy()
    }

    override fun onInterrupt() {}

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {
        if (event == null || event.eventType != AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED) return
        if (stopped || blockedTargets.isEmpty()) return
        val pkg = event.packageName?.toString() ?: return
        if (pkg == packageName) return

        val haystack = "${appLabel(pkg)} $pkg".lowercase()
        val matched = blockedTargets.any { haystack.contains(it) }
        if (!matched) return

        val now = System.currentTimeMillis()
        if (pkg == lastBlockedPackage && now - lastBlockAt < 1_500) return
        lastBlockedPackage = pkg
        lastBlockAt = now
        performGlobalAction(GLOBAL_ACTION_HOME)
    }

    /** Pull the current block rules + stop flag from the local server. */
    private fun refreshState() {
        try {
            val connection = URL("${EmbeddedServer.BASE_URL}/api/state").openConnection() as HttpURLConnection
            connection.connectTimeout = 3_000
            connection.readTimeout = 3_000
            val text = connection.inputStream.bufferedReader().use { it.readText() }
            connection.disconnect()

            stopped = Regex("\"stopped\"\\s*:\\s*true").containsMatchIn(text)
            val blockSection = Regex("\"blockedRules\":\\[(.*?)]").find(text)?.groupValues?.get(1) ?: ""
            blockedTargets = Regex("\"target\":\"(.*?)\"").findAll(blockSection)
                .map { it.groupValues[1].lowercase().trim() }
                .filter { it.isNotEmpty() }
                .toList()
        } catch (_: Exception) {
            // Server not up yet, or transient failure — keep the last known rules.
        }
    }

    private fun appLabel(pkg: String): String = try {
        val info = packageManager.getApplicationInfo(pkg, 0)
        packageManager.getApplicationLabel(info).toString()
    } catch (_: Exception) {
        pkg
    }
}
