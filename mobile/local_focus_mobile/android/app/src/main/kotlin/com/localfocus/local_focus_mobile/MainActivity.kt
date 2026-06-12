package com.localfocus.local_focus_mobile

import android.Manifest
import android.app.AppOpsManager
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.usage.UsageEvents
import android.app.usage.UsageStatsManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.PowerManager
import android.provider.Settings
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel

class MainActivity : FlutterActivity() {
    private val channelName = "local_focus/native"
    private val notificationChannelId = "local_focus_alerts"

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        MethodChannel(flutterEngine.dartExecutor.binaryMessenger, channelName).setMethodCallHandler { call, result ->
            when (call.method) {
                "deviceName" -> result.success(Build.MODEL ?: "Android phone")
                "serverUrl" -> result.success(EmbeddedServer.BASE_URL)
                "startServer" -> {
                    EmbeddedServer.start(applicationContext)
                    result.success(true)
                }
                "serverReady" -> result.success(EmbeddedServer.ready())
                "accessibilityEnabled" -> result.success(accessibilityEnabled())
                "openAccessibilitySettings" -> {
                    startActivity(Intent(Settings.ACTION_ACCESSIBILITY_SETTINGS))
                    result.success(null)
                }
                "requestBatteryExemption" -> {
                    requestBatteryExemption()
                    result.success(null)
                }
                "usageAccessGranted" -> result.success(usageAccessGranted())
                "requestUsageAccess" -> {
                    startActivity(Intent(Settings.ACTION_USAGE_ACCESS_SETTINGS))
                    result.success(null)
                }
                "latestActivity" -> result.success(latestActivity())
                "showNotification" -> {
                    val title = call.argument<String>("title") ?: "Local Focus"
                    val message = call.argument<String>("message") ?: "Focus alert"
                    showLocalNotification(title, message)
                    result.success(null)
                }
                "startPhoneTracking" -> {
                    val serverUrl = call.argument<String>("serverUrl") ?: ""
                    val deviceName = call.argument<String>("deviceName") ?: "Android phone"
                    val endpoint = call.argument<String>("endpoint") ?: ""
                    val intent = Intent(this, PhoneTrackingService::class.java)
                        .putExtra("serverUrl", serverUrl)
                        .putExtra("deviceName", deviceName)
                        .putExtra("endpoint", endpoint)
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                        startForegroundService(intent)
                    } else {
                        startService(intent)
                    }
                    result.success(null)
                }
                "stopPhoneTracking" -> {
                    stopService(Intent(this, PhoneTrackingService::class.java))
                    result.success(null)
                }
                else -> result.notImplemented()
            }
        }
    }

    private fun accessibilityEnabled(): Boolean {
        val enabled = Settings.Secure.getString(
            contentResolver,
            Settings.Secure.ENABLED_ACCESSIBILITY_SERVICES
        ) ?: return false
        return enabled.split(':').any { entry ->
            entry.contains("$packageName/", ignoreCase = true) &&
                entry.endsWith("LocalFocusAccessibilityService", ignoreCase = true)
        }
    }

    private fun requestBatteryExemption() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) return
        val powerManager = getSystemService(Context.POWER_SERVICE) as PowerManager
        if (powerManager.isIgnoringBatteryOptimizations(packageName)) return
        try {
            startActivity(
                Intent(
                    Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                    Uri.parse("package:$packageName")
                )
            )
        } catch (_: Exception) {
            startActivity(Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))
        }
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

    private fun latestActivity(): Map<String, Any?>? {
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
        if (lastPackage.isEmpty()) return null
        val label = appLabel(lastPackage)
        return mapOf(
            "app" to label,
            "title" to label,
            "source" to "android:$lastPackage",
            "packageName" to lastPackage,
            "timestamp" to (lastTimestamp / 1000L)
        )
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

    private fun showLocalNotification(title: String, message: String) {
        val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 4799)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            manager.createNotificationChannel(
                NotificationChannel(notificationChannelId, "Local Focus alerts", NotificationManager.IMPORTANCE_HIGH)
            )
        }
        val notification = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            Notification.Builder(this, notificationChannelId)
        } else {
            @Suppress("DEPRECATION")
            Notification.Builder(this)
        }
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setContentTitle(title)
            .setContentText(message)
            .setStyle(Notification.BigTextStyle().bigText(message))
            .setAutoCancel(true)
            .build()
        manager.notify(System.currentTimeMillis().toInt(), notification)
    }
}
