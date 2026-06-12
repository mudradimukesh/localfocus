package com.localfocus.local_focus_mobile

import android.content.Context
import java.io.File
import java.net.InetSocketAddress
import java.net.Socket

/**
 * Runs the exact Local Focus Rust core on the phone. The core is cross-compiled
 * for every ABI and shipped as `liblocalfocus.so`; Android extracts it into the
 * app's native-library dir, from which we exec it as the on-device server.
 *
 * The phone then serves the identical dashboard at 127.0.0.1:4799, just like the
 * Mac app. Data lives in the app's private files dir (LOCAL_FOCUS_DATA).
 */
object EmbeddedServer {
    const val PORT = 4799
    const val BASE_URL = "http://127.0.0.1:4799"

    @Volatile private var process: Process? = null

    @Synchronized
    fun start(context: Context) {
        if (running()) return
        val binary = File(context.applicationInfo.nativeLibraryDir, "liblocalfocus.so")
        if (!binary.exists()) return
        val dataDir = File(context.filesDir, "local-focus").apply { mkdirs() }
        try {
            val builder = ProcessBuilder(binary.absolutePath, "serve")
            builder.environment()["LOCAL_FOCUS_DATA"] = dataDir.absolutePath
            builder.redirectErrorStream(true)
            val proc = builder.start()
            process = proc
            // Drain stdout so the OS pipe buffer never fills and stalls the server.
            Thread {
                try {
                    proc.inputStream.bufferedReader().forEachLine { /* discard */ }
                } catch (_: Exception) {
                }
            }.start()
        } catch (_: Exception) {
            process = null
        }
    }

    @Synchronized
    fun stop() {
        process?.destroy()
        process = null
    }

    /** True if our child process is still alive. */
    private fun running(): Boolean {
        val proc = process ?: return false
        return try {
            proc.exitValue()
            false
        } catch (_: IllegalThreadStateException) {
            true
        }
    }

    /**
     * True once the server is actually accepting connections on the port. The
     * socket probe runs on a background thread because Android forbids network
     * on the main thread (the method-channel handler runs on the main thread).
     */
    fun ready(): Boolean {
        val connected = java.util.concurrent.atomic.AtomicBoolean(false)
        val probe = Thread {
            try {
                Socket().use { socket ->
                    socket.connect(InetSocketAddress("127.0.0.1", PORT), 400)
                    connected.set(true)
                }
            } catch (_: Exception) {
            }
        }
        probe.start()
        probe.join(800)
        return connected.get()
    }
}
