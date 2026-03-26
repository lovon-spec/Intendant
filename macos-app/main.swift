import Cocoa
import WebKit

class AppDelegate: NSObject, NSApplicationDelegate {
    var window: NSWindow!
    var webView: WKWebView!
    var backendProcess: Process?
    let port: Int = 8765

    func applicationDidFinishLaunching(_ notification: Notification) {
        startBackend()
        createWindow()
        pollUntilReady()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        return true
    }

    func applicationWillTerminate(_ notification: Notification) {
        backendProcess?.terminate()
        backendProcess?.waitUntilExit()
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
        process.arguments = ["--web", "--web-port", "\(port)"]

        // Inherit environment + ensure Homebrew PATH
        var env = ProcessInfo.processInfo.environment
        let extraPaths = ["/opt/homebrew/bin", "/usr/local/bin"]
        let currentPath = env["PATH"] ?? "/usr/bin:/bin:/usr/sbin:/sbin"
        let missing = extraPaths.filter { !currentPath.contains($0) && FileManager.default.fileExists(atPath: $0) }
        if !missing.isEmpty {
            env["PATH"] = missing.joined(separator: ":") + ":" + currentPath
        }
        process.environment = env

        // Set working directory to the project if .env exists nearby
        let appDir = URL(fileURLWithPath: bundle.bundlePath).deletingLastPathComponent()
        process.currentDirectoryURL = appDir

        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice

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

        webView = WKWebView(frame: .zero, configuration: config)
        webView.customUserAgent = "Intendant/1.0"

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
        window.title = "Intendant"
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

        let url = URL(string: "http://localhost:\(port)/")!
        var request = URLRequest(url: url, timeoutInterval: 1)
        request.httpMethod = "HEAD"
        URLSession.shared.dataTask(with: request) { _, response, error in
            if let http = response as? HTTPURLResponse, http.statusCode == 200 {
                DispatchQueue.main.async {
                    self.webView.load(URLRequest(url: url))
                }
            } else {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                    self.poll(attempts: attempts + 1)
                }
            }
        }.resume()
    }
}

// MARK: - Main

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.activate(ignoringOtherApps: true)
app.run()
