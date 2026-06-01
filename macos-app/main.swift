import Cocoa
import WebKit

// MARK: - Scheme Handler

/// Proxies requests from the custom `intendant://` scheme to the local HTTP
/// backend. WKWebView does not treat `http://localhost` as a secure context,
/// so navigator.mediaDevices (mic/camera) is unavailable. Loading the page
/// from a custom scheme registered via setURLSchemeHandler restores secure
/// context status.
class BackendSchemeHandler: NSObject, WKURLSchemeHandler {
    let port: Int
    private var stopped = Set<Int>()
    private let lock = NSLock()
    /// Ephemeral session — no disk or memory cache, so WASM/JS always loads fresh.
    private let session = URLSession(configuration: .ephemeral)

    init(port: Int) {
        self.port = port
    }

    func webView(_ webView: WKWebView, start urlSchemeTask: any WKURLSchemeTask) {
        guard let originalURL = urlSchemeTask.request.url,
              var components = URLComponents(url: originalURL, resolvingAgainstBaseURL: false) else {
            urlSchemeTask.didFailWithError(URLError(.badURL))
            return
        }
        components.scheme = "http"
        components.host = "127.0.0.1"
        components.port = port

        guard let backendURL = components.url else {
            urlSchemeTask.didFailWithError(URLError(.badURL))
            return
        }

        var request = URLRequest(url: backendURL, cachePolicy: .reloadIgnoringLocalCacheData)
        request.httpMethod = urlSchemeTask.request.httpMethod
        request.httpBody = urlSchemeTask.request.httpBody
        if let headers = urlSchemeTask.request.allHTTPHeaderFields {
            for (key, value) in headers {
                request.setValue(value, forHTTPHeaderField: key)
            }
        }

        let taskHash = ObjectIdentifier(urlSchemeTask as AnyObject).hashValue

        session.dataTask(with: request) { [weak self] data, response, error in
            guard let self = self else { return }
            self.lock.lock()
            let wasStopped = self.stopped.remove(taskHash) != nil
            self.lock.unlock()
            if wasStopped { return }

            if let error = error {
                urlSchemeTask.didFailWithError(error)
                return
            }
            if let response = response {
                urlSchemeTask.didReceive(response)
            }
            if let data = data {
                urlSchemeTask.didReceive(data)
            }
            urlSchemeTask.didFinish()
        }.resume()
    }

    func webView(_ webView: WKWebView, stop urlSchemeTask: any WKURLSchemeTask) {
        let taskHash = ObjectIdentifier(urlSchemeTask as AnyObject).hashValue
        lock.lock()
        stopped.insert(taskHash)
        lock.unlock()
    }
}

// MARK: - App Delegate

class AppDelegate: NSObject, NSApplicationDelegate, WKUIDelegate, WKNavigationDelegate {
    var window: NSWindow!
    var webView: WKWebView!
    var backendProcess: Process?
    var healthTimer: Timer?
    var port: Int = 8765
    let portSearchLimit = 20

    func applicationDidFinishLaunching(_ notification: Notification) {
        let preferredPort = port
        if let availablePort = findAvailablePort(startingAt: preferredPort) {
            port = availablePort
            if port != preferredPort {
                NSLog("Port \(preferredPort) in use — using port \(port)")
            }
        } else {
            let lastPort = preferredPort + portSearchLimit - 1
            NSLog("No available port found in range \(preferredPort)-\(lastPort)")
        }
        // Check permissions BEFORE creating the window so system prompts
        // aren't hidden behind it. AXIsProcessTrustedWithOptions is the
        // official way to trigger the Accessibility prompt.
        checkPermissions()
        startBackend()
        createWindow()
        pollUntilReady()
    }

    func checkPermissions() {
        // Request permissions via Apple APIs. These calls REGISTER the app
        // in System Settings (so it appears in the permission lists) and
        // may trigger system prompts. We then check the result and show
        // our own alert if anything is still missing.
        let hasScreenRecording = CGRequestScreenCaptureAccess()
        let accessibilityOpts = [kAXTrustedCheckOptionPrompt.takeUnretainedValue(): true] as CFDictionary
        let hasAccessibility = AXIsProcessTrustedWithOptions(accessibilityOpts)
        NSLog("Permissions: accessibility=\(hasAccessibility), screenRecording=\(hasScreenRecording)")

        // Both granted — nothing to do
        if hasAccessibility && hasScreenRecording { return }

        // Give system prompts a moment to appear and be dismissed
        RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.5))

        // Re-check after system prompts
        let finalAccessibility = AXIsProcessTrusted()
        let finalScreenRecording = CGPreflightScreenCaptureAccess()
        if finalAccessibility && finalScreenRecording { return }

        var missing: [String] = []
        if !finalAccessibility { missing.append("Accessibility (for mouse/keyboard control)") }
        if !finalScreenRecording { missing.append("Screen Recording (for screenshots and display capture)") }

        let alert = NSAlert()
        alert.messageText = "Permissions Required"
        alert.informativeText = "Intendant needs these permissions to work properly:\n\n"
            + missing.enumerated().map { "\($0.offset + 1). \($0.element)" }.joined(separator: "\n")
            + "\n\nOpen System Settings > Privacy & Security and toggle each one ON for Intendant."
            + "\n\nIf already toggled on, toggle OFF then ON again (macOS may need a refresh after recompiling)."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Open Settings")
        alert.addButton(withTitle: "Continue Anyway")

        let response = alert.runModal()
        if response == .alertFirstButtonReturn {
            if !finalAccessibility {
                NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")!)
            } else {
                NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")!)
            }
        }
    }

    func isPortAvailable(_ p: Int) -> Bool {
        let sock = socket(AF_INET, SOCK_STREAM, 0)
        guard sock >= 0 else { return false }
        defer { close(sock) }
        // Allow binding even when TIME_WAIT connections linger from a previous
        // session — the backend uses SO_REUSEADDR too, so this matches.
        var reuse: Int32 = 1
        setsockopt(sock, SOL_SOCKET, SO_REUSEADDR, &reuse, socklen_t(MemoryLayout<Int32>.size))
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_addr.s_addr = inet_addr("0.0.0.0")  // match backend bind address
        addr.sin_port = UInt16(p).bigEndian
        let result = withUnsafePointer(to: &addr) { ptr in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.bind(sock, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) }
        }
        return result == 0
    }

    func findAvailablePort(startingAt preferred: Int) -> Int? {
        let lastPort = min(Int(UInt16.max), preferred + portSearchLimit - 1)
        guard preferred > 0 && preferred <= lastPort else { return nil }
        for candidate in preferred...lastPort {
            if isPortAvailable(candidate) {
                return candidate
            }
        }
        return nil
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        return true
    }

    func applicationWillTerminate(_ notification: Notification) {
        healthTimer?.invalidate()
        guard let proc = backendProcess, proc.isRunning else { return }
        proc.terminate()
        // Wait up to 3 seconds, then force-kill to avoid hanging on quit
        let deadline = Date().addingTimeInterval(3.0)
        while proc.isRunning && Date() < deadline {
            Thread.sleep(forTimeInterval: 0.1)
        }
        if proc.isRunning {
            kill(proc.processIdentifier, SIGKILL)
        }
    }

    // MARK: - WKUIDelegate (JS alert/confirm/prompt)

    func webView(_ webView: WKWebView,
                 runJavaScriptAlertPanelWithMessage message: String,
                 initiatedByFrame frame: WKFrameInfo,
                 completionHandler: @escaping () -> Void) {
        let alert = NSAlert()
        alert.messageText = message
        alert.addButton(withTitle: "OK")
        alert.runModal()
        completionHandler()
    }

    func webView(_ webView: WKWebView,
                 runJavaScriptConfirmPanelWithMessage message: String,
                 initiatedByFrame frame: WKFrameInfo,
                 completionHandler: @escaping (Bool) -> Void) {
        let alert = NSAlert()
        alert.messageText = message
        alert.addButton(withTitle: "OK")
        alert.addButton(withTitle: "Cancel")
        completionHandler(alert.runModal() == .alertFirstButtonReturn)
    }

    func webView(_ webView: WKWebView,
                 runJavaScriptTextInputPanelWithPrompt prompt: String,
                 defaultText: String?,
                 initiatedByFrame frame: WKFrameInfo,
                 completionHandler: @escaping (String?) -> Void) {
        let alert = NSAlert()
        alert.messageText = prompt
        alert.addButton(withTitle: "OK")
        alert.addButton(withTitle: "Cancel")
        let input = NSTextField(frame: NSRect(x: 0, y: 0, width: 260, height: 24))
        input.stringValue = defaultText ?? ""
        alert.accessoryView = input
        completionHandler(alert.runModal() == .alertFirstButtonReturn ? input.stringValue : nil)
    }

    // MARK: - WKNavigationDelegate

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        // macOS killed the web process (memory pressure). Reload the dashboard.
        NSLog("Web content process terminated — reloading")
        webView.load(URLRequest(url: intendantBackendURL()))
    }

    // MARK: - Backend

    func startBackend() {
        let bundle = Bundle.main
        let binPath = bundle.bundlePath + "/Contents/MacOS/intendant-bin"

        guard FileManager.default.fileExists(atPath: binPath) else {
            NSLog("intendant-bin not found at \(binPath)")
            return
        }

        let process = Process()
        process.executableURL = URL(fileURLWithPath: binPath)
        // Forward any extra CLI arguments (e.g. --agent codex) to the backend
        var args = ["--web", String(port)]
        let extra = Array(ProcessInfo.processInfo.arguments.dropFirst())
        args.append(contentsOf: extra)
        process.arguments = args

        // Inherit environment + ensure Homebrew PATH
        var env = ProcessInfo.processInfo.environment
        let extraPaths = ["/opt/homebrew/bin", "/usr/local/bin"]
        let currentPath = env["PATH"] ?? "/usr/bin:/bin:/usr/sbin:/sbin"
        let missing = extraPaths.filter { !currentPath.contains($0) && FileManager.default.fileExists(atPath: $0) }
        if !missing.isEmpty {
            env["PATH"] = missing.joined(separator: ":") + ":" + currentPath
        }
        process.environment = env

        // Set working directory to the directory containing the .app bundle.
        // For ~/projects/intendant/target/Intendant.app this gives ~/projects/intendant/target/
        // Then walk up to find the project root (directory with .env or Cargo.toml).
        //
        // When the .app is installed (the common case — /Applications/Intendant.app),
        // the walk-up from /Applications never finds a Cargo.toml / .env marker and
        // terminates at `/`. If we then set cwd=/, the Rust daemon's FileWatcher::new
        // recursively indexes the entire root filesystem (every /System, /Library,
        // /Volumes mount) and blocks main forever — web_gateway never spawns, the
        // app's WKWebView sits on "Waiting for backend…" until it gives up.
        //
        // Fallback: when no project marker is found after the walk, use ~/.intendant
        // as the cwd. That directory is small, bounded, and already the home of the
        // daemon's logs + config — so FileWatcher completes in milliseconds rather
        // than hanging on /System.
        var dir = URL(fileURLWithPath: bundle.bundlePath).deletingLastPathComponent()
        var foundProjectMarker = false
        for _ in 0..<5 {
            if FileManager.default.fileExists(atPath: dir.appendingPathComponent("Cargo.toml").path) ||
               FileManager.default.fileExists(atPath: dir.appendingPathComponent(".env").path) {
                foundProjectMarker = true
                break
            }
            let parent = dir.deletingLastPathComponent()
            if parent.path == dir.path { break }
            dir = parent
        }
        if !foundProjectMarker {
            // Bandaid until we land a proper "no project open" story
            // (tracked separately): give the daemon a fresh, empty
            // workspace under ~/projects/ as its cwd. NOT ~/.intendant
            // — that's where the daemon writes its own logs + snapshots,
            // so setting cwd there makes FileWatcher + file_snapshots
            // watch the daemon's own output and loop forever.
            //
            // ~/projects/intendant-workspace is empty on first launch,
            // persists across launches (so the agent's edits stay
            // visible), and is out of the daemon's state dir. The
            // sustainable fix makes project_root optional on the Rust
            // side so there's no cwd-is-project-root fallback at all.
            let workspace = FileManager.default.homeDirectoryForCurrentUser
                .appendingPathComponent("projects/intendant-workspace")
            try? FileManager.default.createDirectory(
                at: workspace, withIntermediateDirectories: true)
            dir = workspace
        }
        process.currentDirectoryURL = dir
        NSLog("Working directory: \(dir.path)")

        // Log backend output for debugging (append mode — preserves crash info
        // from previous sessions; the Rust panic hook writes per-session panic.log
        // files for structured auditing, this is the fallback for pre-session crashes)
        let logDir = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".intendant")
        try? FileManager.default.createDirectory(at: logDir, withIntermediateDirectories: true)
        let logFile = logDir.appendingPathComponent("app-backend.log")
        if !FileManager.default.fileExists(atPath: logFile.path) {
            FileManager.default.createFile(atPath: logFile.path, contents: nil)
        }
        let logHandle = FileHandle(forWritingAtPath: logFile.path)
        logHandle?.seekToEndOfFile()
        // Write launch separator
        let sep = "\n--- Launch \(ISO8601DateFormatter().string(from: Date())) ---\n"
        logHandle?.write(sep.data(using: .utf8) ?? Data())
        process.standardOutput = logHandle ?? FileHandle.nullDevice
        process.standardError = logHandle ?? FileHandle.nullDevice

        do {
            try process.run()
            backendProcess = process
            NSLog("Started intendant-bin (PID \(process.processIdentifier)) on port \(port)")
        } catch {
            NSLog("Failed to start intendant-bin: \(error)")
        }
    }

    // MARK: - Window

    func createWindow() {
        let config = WKWebViewConfiguration()
        config.preferences.setValue(true, forKey: "developerExtrasEnabled")

        // Allow media autoplay (for voice features)
        config.mediaTypesRequiringUserActionForPlayback = []

        // Use a non-persistent data store so WKWebView never caches WASM/JS
        // across app launches. Without this, recompiled WASM may not load.
        config.websiteDataStore = WKWebsiteDataStore.nonPersistent()

        // Serve pages from a custom scheme so WKWebView grants a secure
        // context (required for navigator.mediaDevices / getUserMedia).
        config.setURLSchemeHandler(BackendSchemeHandler(port: port), forURLScheme: "intendant")

        // Inject backend port so JS can build WebSocket URLs (WebSocket
        // connections bypass the scheme handler and need the real address).
        let script = WKUserScript(
            source: "window.__intendantPort = \(port);",
            injectionTime: .atDocumentStart,
            forMainFrameOnly: true
        )
        config.userContentController.addUserScript(script)

        webView = WKWebView(frame: .zero, configuration: config)
        webView.uiDelegate = self
        webView.navigationDelegate = self
        webView.customUserAgent = "Intendant/1.0"

        // Starting in macOS 13.3, the legacy `developerExtrasEnabled` KVC
        // trick above is a no-op for release-signed builds; Safari's Web
        // Inspector only attaches to a WKWebView when `isInspectable` is
        // explicitly set to `true`. Without this, Safari → Develop →
        // [Mac name] silently omits the Intendant process — which blocks
        // any WebRTC diagnostics that rely on Safari Web Inspector
        // (ICE candidate events, iceConnectionState, getStats output).
        if #available(macOS 13.3, *) {
            webView.isInspectable = true
        }

        let screen = NSScreen.main ?? NSScreen.screens[0]
        let screenFrame = screen.visibleFrame
        let width = min(1400.0, screenFrame.width * 0.85)
        let height = min(900.0, screenFrame.height * 0.85)
        let x = screenFrame.origin.x + (screenFrame.width - width) / 2
        let y = screenFrame.origin.y + (screenFrame.height - height) / 2

        window = NSWindow(
            contentRect: NSRect(x: x, y: y, width: width, height: height),
            styleMask: [.titled, .closable, .resizable, .miniaturizable],
            backing: .buffered,
            defer: false
        )
        window.title = port == 8765 ? "Intendant" : "Intendant (port \(port))"
        window.contentView = webView
        window.minSize = NSSize(width: 600, height: 400)
        window.makeKeyAndOrderFront(nil)

        // Dark title bar to match Catppuccin Mocha theme
        window.titlebarAppearsTransparent = true
        window.backgroundColor = NSColor(red: 30/255, green: 30/255, blue: 46/255, alpha: 1.0)
        window.appearance = NSAppearance(named: .darkAqua)
    }

    // MARK: - Polling

    func pollUntilReady() {
        webView.loadHTMLString("""
            <html>
            <body style="background:#1e1e2e;color:#cdd6f4;font-family:-apple-system;display:flex;align-items:center;justify-content:center;height:100vh;margin:0">
            <div style="text-align:center">
                <div style="font-size:24px;margin-bottom:8px">Starting Intendant...</div>
                <div style="font-size:14px;color:#6c7086">Waiting for backend on port \(port)</div>
            </div>
            </body>
            </html>
            """, baseURL: nil)

        poll(attempts: 0)
    }

    func poll(attempts: Int) {
        if attempts > 30 {
            webView.loadHTMLString("""
                <html>
                <body style="background:#1e1e2e;color:#f38ba8;font-family:-apple-system;display:flex;align-items:center;justify-content:center;height:100vh;margin:0">
                <div>Failed to connect to backend on port \(port)</div>
                </body>
                </html>
                """, baseURL: nil)
            return
        }

        // Poll the HTTP backend directly
        let healthURL = URL(string: "http://127.0.0.1:\(port)/")!
        var request = URLRequest(url: healthURL, timeoutInterval: 1)
        request.httpMethod = "HEAD"
        URLSession.shared.dataTask(with: request) { _, response, error in
            if let http = response as? HTTPURLResponse, http.statusCode == 200 {
                DispatchQueue.main.async {
                    // Load via custom scheme for secure context
                    self.webView.load(URLRequest(url: intendantBackendURL()))
                    self.startHealthCheck()
                }
            } else {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                    self.poll(attempts: attempts + 1)
                }
            }
        }.resume()
    }

    // MARK: - Health Check

    func startHealthCheck() {
        healthTimer = Timer.scheduledTimer(withTimeInterval: 5.0, repeats: true) { [weak self] _ in
            guard let self = self else { return }
            // Check if the backend process is still alive
            if let proc = self.backendProcess, !proc.isRunning {
                self.healthTimer?.invalidate()
                self.showBackendCrash()
                return
            }
            // Also ping the HTTP endpoint
            let url = URL(string: "http://127.0.0.1:\(self.port)/")!
            var req = URLRequest(url: url, timeoutInterval: 2)
            req.httpMethod = "HEAD"
            URLSession.shared.dataTask(with: req) { _, response, _ in
                let ok = (response as? HTTPURLResponse)?.statusCode == 200
                if !ok {
                    DispatchQueue.main.async {
                        self.healthTimer?.invalidate()
                        self.showBackendCrash()
                    }
                }
            }.resume()
        }
    }

    func showBackendCrash() {
        NSLog("Backend process is no longer running")
        webView.loadHTMLString("""
            <html>
            <body style="background:#1e1e2e;color:#cdd6f4;font-family:-apple-system;display:flex;align-items:center;justify-content:center;height:100vh;margin:0">
            <div style="text-align:center">
                <div style="font-size:20px;color:#f38ba8;margin-bottom:12px">Backend process exited</div>
                <div style="font-size:14px;color:#6c7086;margin-bottom:16px">Check ~/.intendant/app-backend.log for details</div>
                <button onclick="window.webkit.messageHandlers.restart && window.webkit.messageHandlers.restart.postMessage(null)"
                        style="padding:8px 24px;border:1px solid #89b4fa;border-radius:6px;background:transparent;color:#89b4fa;font-size:14px;cursor:pointer">
                    Restart
                </button>
            </div>
            </body>
            </html>
            """, baseURL: nil)
    }
}

// MARK: - Helpers

/// Resolve the URL the WKWebView loads on initial entry and on
/// web-content-process restart. Setting `INTENDANT_DIAG=1` in the
/// environment appends `?diag=1` so the dashboard's visual-freshness
/// sampler activates from page load. Off by default — used only for
/// harness/smoke runs (see `docs/smoke-display.md` §9). Routes through
/// the same `intendant://backend/` custom scheme so the WKWebView keeps
/// its secure context (mic, custom URL scheme handler).
func intendantBackendURL() -> URL {
    let diag = ProcessInfo.processInfo.environment["INTENDANT_DIAG"] == "1"
    let raw = diag ? "intendant://backend/?diag=1" : "intendant://backend/"
    return URL(string: raw)!
}

// MARK: - Main

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.activate(ignoringOtherApps: true)
app.run()
