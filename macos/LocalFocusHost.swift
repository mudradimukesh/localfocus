import Cocoa
import UserNotifications
import WebKit

/// Talks to the local server the app already runs. Everything the menu bar and
/// the Focus menu do goes through the same HTTP surface the dashboard uses, so
/// there is exactly one implementation of each action.
enum LocalFocusAPI {
    static let base = URL(string: "http://127.0.0.1:4799")!

    static func call(_ path: String, completion: ((Data?) -> Void)? = nil) {
        guard let url = URL(string: path, relativeTo: base) else { return }
        var request = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 5)
        request.httpMethod = "GET"
        URLSession.shared.dataTask(with: request) { data, _, _ in
            completion?(data)
        }.resume()
    }
}

/// The current session, as far as the menu bar needs to know.
struct FocusSnapshot {
    var running = false
    var paused = false
    var stopped = false
    var task = ""
    var remainingSeconds = 0

    var statusText: String {
        if stopped { return "Off" }
        if !running { return "Idle" }
        if paused { return "Paused" }
        return FocusSnapshot.clock(remainingSeconds)
    }

    static func clock(_ seconds: Int) -> String {
        let total = max(0, seconds)
        let hours = total / 3600
        let minutes = (total % 3600) / 60
        let rest = total % 60
        if hours > 0 {
            return String(format: "%d:%02d:%02d", hours, minutes, rest)
        }
        return String(format: "%02d:%02d", minutes, rest)
    }
}

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
        mainMenu.addItem(focusMenuItem())
        mainMenu.addItem(viewMenuItem())
        mainMenu.addItem(windowMenuItem())
        return mainMenu
    }

    /// The app's actual verbs, with keyboard shortcuts. Before this the menu
    /// bar only offered WebKit boilerplate and none of what the app does.
    private static func focusMenuItem() -> NSMenuItem {
        let item = NSMenuItem(title: "Focus", action: nil, keyEquivalent: "")
        let menu = NSMenu(title: "Focus")

        let start = NSMenuItem(title: "Start focus", action: #selector(AppDelegate.startFocus(_:)), keyEquivalent: "\r")
        menu.addItem(start)

        let pause = NSMenuItem(title: "Pause or resume session", action: #selector(AppDelegate.togglePause(_:)), keyEquivalent: "p")
        pause.keyEquivalentModifierMask = [.command, .shift]
        menu.addItem(pause)

        menu.addItem(.separator())

        let turnOff = NSMenuItem(title: "Turn off Local Focus", action: #selector(AppDelegate.turnOff(_:)), keyEquivalent: "l")
        turnOff.keyEquivalentModifierMask = [.command, .shift]
        menu.addItem(turnOff)

        let turnOn = NSMenuItem(title: "Turn on Local Focus", action: #selector(AppDelegate.turnOn(_:)), keyEquivalent: "l")
        turnOn.keyEquivalentModifierMask = [.command, .option]
        menu.addItem(turnOn)

        item.submenu = menu
        return item
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

    private var statusItem: NSStatusItem?
    private var statusTimer: Timer?
    private var snapshot = FocusSnapshot()
    private var notificationsAuthorized = false

    func applicationDidFinishLaunching(_ notification: Notification) {
        startLocalServer()
        openWindow()
        loadDashboardWhenReady()
        setUpStatusItem()
        requestNotificationAccess()
        startStatusPolling()
    }

    // MARK: - Menu bar extra

    /// Session status without raising the window — the whole point of a focus
    /// timer is being able to glance at it.
    private func setUpStatusItem() {
        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        item.button?.title = "Local Focus"
        item.button?.image = NSImage(systemSymbolName: "target", accessibilityDescription: "Local Focus")
        item.button?.imagePosition = .imageLeading
        item.menu = buildStatusMenu()
        statusItem = item
    }

    private func buildStatusMenu() -> NSMenu {
        let menu = NSMenu()

        let status = NSMenuItem(title: "Idle", action: nil, keyEquivalent: "")
        status.isEnabled = false
        status.tag = 1
        menu.addItem(status)
        menu.addItem(.separator())

        menu.addItem(NSMenuItem(title: "Start focus", action: #selector(startFocus(_:)), keyEquivalent: ""))
        let pause = NSMenuItem(title: "Pause session", action: #selector(togglePause(_:)), keyEquivalent: "")
        pause.tag = 2
        menu.addItem(pause)
        let power = NSMenuItem(title: "Turn off Local Focus", action: #selector(toggleStopped(_:)), keyEquivalent: "")
        power.tag = 3
        menu.addItem(power)

        menu.addItem(.separator())
        menu.addItem(NSMenuItem(title: "Open dashboard", action: #selector(openDashboard(_:)), keyEquivalent: ""))
        menu.addItem(.separator())
        menu.addItem(NSMenuItem(title: "Quit Local Focus", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q"))

        for item in menu.items where item.action != nil {
            item.target = item.action == #selector(NSApplication.terminate(_:)) ? nil : self
        }
        return menu
    }

    private func startStatusPolling() {
        let timer = Timer.scheduledTimer(withTimeInterval: 2.0, repeats: true) { [weak self] _ in
            self?.pollState()
            self?.drainNotifications()
        }
        RunLoop.main.add(timer, forMode: .common)
        statusTimer = timer
        pollState()
    }

    private func pollState() {
        LocalFocusAPI.call("/api/state") { [weak self] data in
            guard let data,
                  let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return }
            var next = FocusSnapshot()
            next.stopped = json["stopped"] as? Bool ?? false
            if let focus = json["focus"] as? [String: Any] {
                next.running = true
                next.paused = focus["paused"] as? Bool ?? false
                next.task = focus["task"] as? String ?? ""
                next.remainingSeconds = focus["remainingSeconds"] as? Int ?? 0
            }
            DispatchQueue.main.async { self?.apply(next) }
        }
    }

    private func apply(_ next: FocusSnapshot) {
        snapshot = next
        statusItem?.button?.title = next.statusText
        guard let menu = statusItem?.menu else { return }
        menu.item(withTag: 1)?.title = next.stopped ? "Local Focus is off"
            : !next.running ? "No session running"
            : next.paused ? "Paused — \(next.task)"
            : "\(FocusSnapshot.clock(next.remainingSeconds)) left — \(next.task)"
        menu.item(withTag: 2)?.title = next.paused ? "Resume session" : "Pause session"
        menu.item(withTag: 2)?.isEnabled = next.running && !next.stopped
        menu.item(withTag: 3)?.title = next.stopped ? "Turn on Local Focus" : "Turn off Local Focus"
    }

    // MARK: - Notifications

    /// Post banners as Local Focus rather than as osascript, so they carry the
    /// app's name and icon and can be managed in System Settings. If access is
    /// denied we simply never heartbeat, and the server keeps its own fallback.
    private func requestNotificationAccess() {
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound]) { [weak self] granted, _ in
            DispatchQueue.main.async { self?.notificationsAuthorized = granted }
        }
    }

    private func drainNotifications() {
        // Only claim the heartbeat (?host=1) once macOS has actually granted
        // permission. If it hasn't, the server must keep its own fallback
        // rather than queue banners this process cannot post.
        guard notificationsAuthorized else { return }
        LocalFocusAPI.call("/api/mac/notifications?host=1") { data in
            guard let data,
                  let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                  let items = json["notifications"] as? [[String: Any]] else { return }
            for item in items {
                let content = UNMutableNotificationContent()
                content.title = item["title"] as? String ?? "Local Focus"
                content.body = item["message"] as? String ?? ""
                content.sound = .default
                let request = UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil)
                UNUserNotificationCenter.current().add(request)
            }
        }
    }

    // MARK: - Actions

    @objc func startFocus(_ sender: Any?) {
        LocalFocusAPI.call("/api/focus/start") { [weak self] _ in
            DispatchQueue.main.async { self?.pollState() }
        }
    }

    @objc func togglePause(_ sender: Any?) {
        LocalFocusAPI.call("/api/focus/pause") { [weak self] _ in
            DispatchQueue.main.async { self?.pollState() }
        }
    }

    @objc func turnOff(_ sender: Any?) {
        LocalFocusAPI.call("/api/focus/stop") { [weak self] _ in
            DispatchQueue.main.async { self?.pollState() }
        }
    }

    @objc func turnOn(_ sender: Any?) {
        LocalFocusAPI.call("/api/app/resume") { [weak self] _ in
            DispatchQueue.main.async { self?.pollState() }
        }
    }

    @objc func toggleStopped(_ sender: Any?) {
        if snapshot.stopped {
            turnOn(sender)
        } else {
            turnOff(sender)
        }
    }

    @objc func openDashboard(_ sender: Any?) {
        if window == nil || window?.isVisible == false {
            openWindow()
            loadDashboardWhenReady()
        }
        bringWindowForward()
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
        window.contentView = webView
        // Come back the size and place the user left it, rather than always
        // recentering at a fixed size.
        window.setFrameAutosaveName("LocalFocusMainWindow")
        if window.frame.origin == .zero {
            window.center()
        }

        self.window = window
        self.webView = webView
        bringWindowForward()
    }

    /// Bring the window up once, politely. This used to re-activate on a timer
    /// three times after launch, which fought the window server and stole focus
    /// from whatever the user was doing.
    private func bringWindowForward() {
        NSApp.unhide(nil)
        NSApp.activate(ignoringOtherApps: false)
        window?.makeKeyAndOrderFront(nil)
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
