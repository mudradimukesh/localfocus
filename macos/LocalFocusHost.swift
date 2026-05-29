import Cocoa
import WebKit

@main
struct LocalFocusHost {
    static func main() {
        let app = NSApplication.shared
        let delegate = AppDelegate()
        app.delegate = delegate
        app.setActivationPolicy(.regular)
        app.activate(ignoringOtherApps: true)
        app.run()
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    private var window: NSWindow?
    private var webView: WKWebView?
    private var serverProcess: Process?
    private let dashboardURL = URL(string: "http://127.0.0.1:4799/")!

    func applicationDidFinishLaunching(_ notification: Notification) {
        startLocalServer()
        openWindow()
        loadDashboardWhenReady()
    }

    func applicationDidBecomeActive(_ notification: Notification) {
        if window == nil || window?.isVisible == false {
            openWindow()
            loadDashboardWhenReady()
        } else {
            bringWindowForward()
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        serverProcess?.terminate()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        if !flag {
            openWindow()
            loadDashboardWhenReady()
        } else {
            bringWindowForward()
        }
        return true
    }

    private func startLocalServer() {
        guard let executableDirectory = Bundle.main.executableURL?.deletingLastPathComponent() else {
            return
        }

        let serverURL = executableDirectory.appendingPathComponent("local-focus-bin")
        let process = Process()
        process.executableURL = serverURL
        process.arguments = ["serve"]
        process.standardOutput = Pipe()
        process.standardError = Pipe()

        do {
            try process.run()
            serverProcess = process
        } catch {
            showError("Could not start Local Focus: \(error.localizedDescription)")
        }
    }

    private func openWindow() {
        let configuration = WKWebViewConfiguration()
        let webView = WKWebView(frame: .zero, configuration: configuration)
        webView.autoresizingMask = [.width, .height]

        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1180, height: 820),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "Local Focus"
        window.center()
        window.contentView = webView

        self.window = window
        self.webView = webView
        bringWindowForward()
        bringWindowForwardAfterLaunch()
    }

    private func bringWindowForward() {
        NSApp.setActivationPolicy(.regular)
        NSApp.unhide(nil)
        NSApp.activate(ignoringOtherApps: true)
        window?.makeKeyAndOrderFront(nil)
        window?.orderFrontRegardless()
    }

    private func bringWindowForwardAfterLaunch() {
        for delay in [0.2, 0.8, 1.6] {
            DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in
                self?.bringWindowForward()
            }
        }
    }

    private func loadDashboardWhenReady(attempt: Int = 0) {
        let request = URLRequest(url: dashboardURL, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 1)
        URLSession.shared.dataTask(with: request) { [weak self] _, response, _ in
            let ready = (response as? HTTPURLResponse)?.statusCode == 200
            DispatchQueue.main.async {
                if ready {
                    self?.webView?.load(URLRequest(url: self?.dashboardURL ?? URL(string: "http://127.0.0.1:4799/")!))
                } else if attempt < 20 {
                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.25) {
                        self?.loadDashboardWhenReady(attempt: attempt + 1)
                    }
                } else {
                    self?.showError("Local Focus started, but the dashboard did not become available.")
                }
            }
        }.resume()
    }

    private func showError(_ message: String) {
        let alert = NSAlert()
        alert.messageText = "Local Focus"
        alert.informativeText = message
        alert.alertStyle = .warning
        alert.runModal()
    }
}
