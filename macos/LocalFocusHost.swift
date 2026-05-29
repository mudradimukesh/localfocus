import Cocoa
import WebKit

@main
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

    func applicationWillTerminate(_ notification: Notification) {
        serverProcess?.terminate()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
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
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)

        self.window = window
        self.webView = webView
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
