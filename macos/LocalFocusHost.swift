import Cocoa
import WebKit

@main
struct LocalFocusHost {
    static func main() {
        let app = NSApplication.shared
        let delegate = AppDelegate()
        app.delegate = delegate
        app.mainMenu = AppMenu.build()
        app.setActivationPolicy(.regular)
        app.activate(ignoringOtherApps: true)
        app.run()
    }
}

enum AppMenu {
    static func build() -> NSMenu {
        let mainMenu = NSMenu()
        mainMenu.addItem(appMenuItem())
        mainMenu.addItem(editMenuItem())
        mainMenu.addItem(viewMenuItem())
        mainMenu.addItem(windowMenuItem())
        return mainMenu
    }

    private static func appMenuItem() -> NSMenuItem {
        let appName = appDisplayName()
        let item = NSMenuItem(title: appName, action: nil, keyEquivalent: "")
        let menu = NSMenu()

        menu.addItem(NSMenuItem(title: "About \(appName)", action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)), keyEquivalent: ""))
        menu.addItem(.separator())

        let services = NSMenu()
        let servicesItem = NSMenuItem(title: "Services", action: nil, keyEquivalent: "")
        servicesItem.submenu = services
        menu.addItem(servicesItem)
        NSApplication.shared.servicesMenu = services

        menu.addItem(.separator())
        menu.addItem(NSMenuItem(title: "Hide \(appName)", action: #selector(NSApplication.hide(_:)), keyEquivalent: "h"))

        let hideOthers = NSMenuItem(title: "Hide Others", action: #selector(NSApplication.hideOtherApplications(_:)), keyEquivalent: "h")
        hideOthers.keyEquivalentModifierMask = [.command, .option]
        menu.addItem(hideOthers)

        menu.addItem(NSMenuItem(title: "Show All", action: #selector(NSApplication.unhideAllApplications(_:)), keyEquivalent: ""))
        menu.addItem(.separator())
        menu.addItem(NSMenuItem(title: "Quit \(appName)", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q"))

        item.submenu = menu
        return item
    }

    private static func editMenuItem() -> NSMenuItem {
        let item = NSMenuItem(title: "Edit", action: nil, keyEquivalent: "")
        let menu = NSMenu(title: "Edit")

        menu.addItem(NSMenuItem(title: "Undo", action: Selector(("undo:")), keyEquivalent: "z"))

        let redo = NSMenuItem(title: "Redo", action: Selector(("redo:")), keyEquivalent: "Z")
        redo.keyEquivalentModifierMask = [.command, .shift]
        menu.addItem(redo)

        menu.addItem(.separator())
        menu.addItem(NSMenuItem(title: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x"))
        menu.addItem(NSMenuItem(title: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c"))
        menu.addItem(NSMenuItem(title: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v"))

        let pasteAndMatchStyle = NSMenuItem(title: "Paste and Match Style", action: Selector(("pasteAsPlainText:")), keyEquivalent: "V")
        pasteAndMatchStyle.keyEquivalentModifierMask = [.command, .option, .shift]
        menu.addItem(pasteAndMatchStyle)

        menu.addItem(NSMenuItem(title: "Delete", action: #selector(NSText.delete(_:)), keyEquivalent: ""))
        menu.addItem(.separator())
        menu.addItem(NSMenuItem(title: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a"))

        menu.addItem(.separator())
        let findMenuItem = NSMenuItem(title: "Find", action: nil, keyEquivalent: "")
        let findMenu = NSMenu(title: "Find")
        findMenu.addItem(NSMenuItem(title: "Find...", action: Selector(("performFindPanelAction:")), keyEquivalent: "f"))

        let findNext = NSMenuItem(title: "Find Next", action: Selector(("performFindPanelAction:")), keyEquivalent: "g")
        findNext.tag = NSTextFinder.Action.nextMatch.rawValue
        findMenu.addItem(findNext)

        let findPrevious = NSMenuItem(title: "Find Previous", action: Selector(("performFindPanelAction:")), keyEquivalent: "G")
        findPrevious.keyEquivalentModifierMask = [.command, .shift]
        findPrevious.tag = NSTextFinder.Action.previousMatch.rawValue
        findMenu.addItem(findPrevious)

        findMenuItem.submenu = findMenu
        menu.addItem(findMenuItem)

        item.submenu = menu
        return item
    }

    private static func viewMenuItem() -> NSMenuItem {
        let item = NSMenuItem(title: "View", action: nil, keyEquivalent: "")
        let menu = NSMenu(title: "View")
        menu.addItem(NSMenuItem(title: "Reload", action: #selector(WKWebView.reload(_:)), keyEquivalent: "r"))
        item.submenu = menu
        return item
    }

    private static func windowMenuItem() -> NSMenuItem {
        let item = NSMenuItem(title: "Window", action: nil, keyEquivalent: "")
        let menu = NSMenu(title: "Window")
        menu.addItem(NSMenuItem(title: "Minimize", action: #selector(NSWindow.miniaturize(_:)), keyEquivalent: "m"))
        menu.addItem(NSMenuItem(title: "Zoom", action: #selector(NSWindow.performZoom(_:)), keyEquivalent: ""))
        menu.addItem(.separator())
        menu.addItem(NSMenuItem(title: "Close", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w"))
        item.submenu = menu
        NSApplication.shared.windowsMenu = menu
        return item
    }

    private static func appDisplayName() -> String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleDisplayName") as? String
            ?? Bundle.main.object(forInfoDictionaryKey: "CFBundleName") as? String
            ?? "Local Focus"
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

        stopExistingDashboardServer()

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

    private func stopExistingDashboardServer() {
        let lsof = Process()
        let output = Pipe()
        lsof.executableURL = URL(fileURLWithPath: "/usr/sbin/lsof")
        lsof.arguments = ["-tiTCP:4799", "-sTCP:LISTEN"]
        lsof.standardOutput = output
        lsof.standardError = Pipe()

        do {
            try lsof.run()
            lsof.waitUntilExit()
        } catch {
            return
        }

        let data = output.fileHandleForReading.readDataToEndOfFile()
        let pids = String(data: data, encoding: .utf8)?
            .split(whereSeparator: \.isNewline)
            .map(String.init) ?? []

        for pid in pids where Int32(pid) != getpid() {
            let kill = Process()
            kill.executableURL = URL(fileURLWithPath: "/bin/kill")
            kill.arguments = ["-TERM", pid]
            kill.standardOutput = Pipe()
            kill.standardError = Pipe()
            try? kill.run()
            kill.waitUntilExit()
        }

        if !pids.isEmpty {
            Thread.sleep(forTimeInterval: 0.25)
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
