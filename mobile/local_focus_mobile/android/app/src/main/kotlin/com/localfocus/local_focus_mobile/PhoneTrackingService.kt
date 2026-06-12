package com.localfocus.local_focus_mobile

import android.app.AppOpsManager
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.app.usage.UsageEvents
import android.app.usage.UsageStatsManager
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import java.net.HttpURLConnection
import java.net.URL

class PhoneTrackingService : Service() {
    private val handler = Handler(Looper.getMainLooper())
    private val channelId = "local_focus_tracking"
    private var serverUrl = ""
    private var deviceName = "Android phone"
    private var endpoint = ""

    private val sampler = object : Runnable {
        override fun run() {
            sampleAndPost()
            handler.postDelayed(this, 5_000)
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        serverUrl = intent?.getStringExtra("serverUrl")?.trim()?.trimEnd('/') ?: serverUrl
        deviceName = intent?.getStringExtra("deviceName") ?: deviceName
        endpoint = intent?.getStringExtra("endpoint") ?: endpoint
        startForeground(4799, trackingNotification())
        handler.removeCallbacks(sampler)
        handler.post(sampler)
        return START_STICKY
    }

    override fun onDestroy() {
        handler.removeCallbacks(sampler)
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private fun sampleAndPost() {
        if (serverUrl.isEmpty()) return
        val activity = latestActivity() ?: return
        Thread {
            postActivity(activity)
        }.start()
    }

    private fun latestActivity(): Map<String, String>? {
        val powerManager = getSystemService(Context.POWER_SERVICE) as PowerManager
        if (!powerManager.isInteractive) {
            return mapOf(
                "app" to "Phone idle",
                "title" to "Screen off",
                "source" to "mobile:${Build.MODEL}",
                "category" to "idle"
            )
        }
        if (!usageAccessGranted()) return null
        val usageStats = getSystemService(Context.USAGE_STATS_SERVICE) as UsageStatsManager
        val end = System.currentTimeMillis()
        val events = usageStats.queryEvents(end - 120_000, end)
        val event = UsageEvents.Event()
        var lastPackage = ""
        var lastTimestamp = 0L
        while (events.hasNextEvent()) {
            events.getNextEvent(event)
            if (isForegroundEvent(event) && event.timeStamp >= lastTimestamp) {
                lastPackage = event.packageName ?: ""
                lastTimestamp = event.timeStamp
            }
        }
        if (lastPackage.isEmpty() || lastPackage == packageName) return null
        val label = appLabel(lastPackage)
        return mapOf(
            "app" to label,
            "title" to label,
            "source" to "android:$lastPackage"
        )
    }

    private fun postActivity(activity: Map<String, String>) {
        try {
            val url = URL("$serverUrl/api/mobile/activity")
            val connection = url.openConnection() as HttpURLConnection
            connection.requestMethod = "POST"
            connection.connectTimeout = 4_000
            connection.readTimeout = 4_000
            connection.doOutput = true
            connection.setRequestProperty("Content-Type", "application/json")
            val body = buildJson(activity)
            connection.outputStream.use { stream ->
                stream.write(body.toByteArray(Charsets.UTF_8))
            }
            connection.inputStream.close()
            connection.disconnect()
        } catch (_: Exception) {}
    }

    private fun buildJson(activity: Map<String, String>): String {
        val pairs = mutableListOf(
            "\"device\":\"${escapeJson(deviceName)}\"",
            "\"endpoint\":\"${escapeJson(endpoint)}\"",
            "\"app\":\"${escapeJson(activity["app"] ?: "Phone activity")}\"",
            "\"title\":\"${escapeJson(activity["title"] ?: "")}\"",
            "\"source\":\"${escapeJson(activity["source"] ?: "mobile:$deviceName")}\"",
            "\"timestamp\":${System.currentTimeMillis() / 1000L}"
        )
        val category = activity["category"]
        if (!category.isNullOrBlank()) {
            pairs.add("\"category\":\"${escapeJson(category)}\"")
        }
        return "{${pairs.joinToString(",")}}"
    }

    private fun trackingNotification(): Notification {
        val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            manager.createNotificationChannel(
                NotificationChannel(channelId, "Local Focus tracking", NotificationManager.IMPORTANCE_LOW)
            )
        }
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            Notification.Builder(this, channelId)
        } else {
            @Suppress("DEPRECATION")
            Notification.Builder(this)
        }
            .setSmallIcon(android.R.drawable.ic_menu_recent_history)
            .setContentTitle("Local Focus is tracking this phone")
            .setContentText("Phone activity is sent only to the QR-connected Local Focus laptop.")
            .setOngoing(true)
            .build()
    }

    private fun usageAccessGranted(): Boolean {
        val appOps = getSystemService(Context.APP_OPS_SERVICE) as AppOpsManager
        val mode = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            appOps.unsafeCheckOpNoThrow(AppOpsManager.OPSTR_GET_USAGE_STATS, android.os.Process.myUid(), packageName)
        } else {
            @Suppress("DEPRECATION")
            appOps.checkOpNoThrow(AppOpsManager.OPSTR_GET_USAGE_STATS, android.os.Process.myUid(), packageName)
        }
        return mode == AppOpsManager.MODE_ALLOWED
    }

    private fun isForegroundEvent(event: UsageEvents.Event): Boolean {
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            event.eventType == UsageEvents.Event.ACTIVITY_RESUMED
        } else {
            @Suppress("DEPRECATION")
            event.eventType == UsageEvents.Event.MOVE_TO_FOREGROUND
        }
    }

    private fun appLabel(packageName: String): String {
        return try {
            val appInfo = packageManager.getApplicationInfo(packageName, 0)
            packageManager.getApplicationLabel(appInfo).toString()
        } catch (_: Exception) {
            packageName
        }
    }

    private fun escapeJson(value: String): String {
        return value
            .replace("\\", "\\\\")
            .replace("\"", "\\\"")
            .replace("\n", "\\n")
            .replace("\r", "\\r")
            .replace("\t", "\\t")
    }
}
