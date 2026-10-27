// The notifier: a small menu bar app the server runs while it is up. It posts macOS notifications
// for requests waiting on you, with the Quartermaster mark as their icon, and keeps the mark in the
// menu bar with a count of what's waiting.
//
// The server launches it through LaunchServices (`open`), which macOS requires before it shows a
// notification, with `--connect <socket>`: it connects back there and they speak JSON lines. The
// server sends `state` (everything waiting; it also withdraws the banners of what's gone) and
// `post` (one banner); it sends back `ready` or `denied`, and `open` for a click. When the
// connection closes (the server is gone) it quits.
//
// `--make-icon <file.icns>` draws the app icon and exits; the server runs it when it installs the
// bundle.

import AppKit
import UserNotifications

// MARK: The mark

/// The mark's shapes, in the SVG's coordinates (crates/qm-tui/assets/mark.svg: a viewBox of
/// 359.63 × 356.07, y down): a quarter ring and two squares.
enum Mark {
    static let size = CGSize(width: 359.63, height: 356.07)
    static let teal = NSColor(srgbRed: 0x25 / 255, green: 0xA4 / 255, blue: 0x93 / 255, alpha: 1)

    /// The mark filling `rect`, centred, its aspect kept; drawn into a context whose y runs up.
    static func path(in rect: CGRect) -> NSBezierPath {
        let scale = min(rect.width / size.width, rect.height / size.height)
        let left = rect.midX - size.width * scale / 2
        let top = rect.midY + size.height * scale / 2
        func p(_ x: CGFloat, _ y: CGFloat) -> CGPoint {
            CGPoint(x: left + x * scale, y: top - y * scale)
        }
        let path = NSBezierPath()
        path.move(to: p(239.63, 0))
        path.curve(to: p(169.44, 169.44), controlPoint1: p(239.63, 64.01), controlPoint2: p(214.70, 124.18))
        path.curve(to: p(0, 239.63), controlPoint1: p(124.18, 214.70), controlPoint2: p(64.01, 239.63))
        path.line(to: p(0, 119.63))
        path.curve(to: p(119.63, 0), controlPoint1: p(65.96, 119.63), controlPoint2: p(119.63, 65.96))
        path.close()
        for (x, y) in [(239.63, 116.07), (118.98, 236.07)] {
            let corner = p(x, y + 120)
            path.append(NSBezierPath(rect: CGRect(x: corner.x, y: corner.y, width: 120 * scale, height: 120 * scale)))
        }
        return path
    }

    /// The menu bar image: the mark alone, as a template so macOS tints it for the menu bar.
    static func menuBarImage() -> NSImage {
        let image = NSImage(size: NSSize(width: 16, height: 16), flipped: false) { rect in
            NSColor.black.setFill()
            path(in: rect).fill()
            return true
        }
        image.isTemplate = true
        return image
    }

    /// The app icon at `pixels` square: the teal mark on a dark rounded square, in the macOS icon
    /// grid (an 824 tile in 1024, corners 185).
    static func icon(pixels: Int) -> CGImage? {
        let space = CGColorSpace(name: CGColorSpace.sRGB)!
        guard let context = CGContext(
            data: nil, width: pixels, height: pixels, bitsPerComponent: 8, bytesPerRow: 0,
            space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
        ) else { return nil }
        let unit = CGFloat(pixels) / 1024
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(cgContext: context, flipped: false)
        let tile = CGRect(x: 100 * unit, y: 100 * unit, width: 824 * unit, height: 824 * unit)
        let shape = NSBezierPath(roundedRect: tile, xRadius: 185 * unit, yRadius: 185 * unit)
        // The palette's dark slate, a little lighter at the top.
        let gradient = NSGradient(
            starting: NSColor(srgbRed: 0x1E / 255, green: 0x2C / 255, blue: 0x37 / 255, alpha: 1),
            ending: NSColor(srgbRed: 0x10 / 255, green: 0x19 / 255, blue: 0x20 / 255, alpha: 1)
        )
        gradient?.draw(in: shape, angle: -90)
        teal.setFill()
        path(in: tile.insetBy(dx: 190 * unit, dy: 190 * unit)).fill()
        NSGraphicsContext.restoreGraphicsState()
        return context.makeImage()
    }

    /// Writes the app icon, every size macOS asks for, to `url`.
    static func writeIcon(to url: URL) -> Bool {
        let sizes = [16, 32, 64, 128, 256, 512, 1024]
        guard let out = CGImageDestinationCreateWithURL(url as CFURL, "com.apple.icns" as CFString, sizes.count, nil) else {
            return false
        }
        for size in sizes {
            guard let image = icon(pixels: size) else { return false }
            CGImageDestinationAddImage(out, image, nil)
        }
        return CGImageDestinationFinalize(out)
    }
}

// MARK: Messages

/// One request waiting on you, as the menu lists it.
struct Item: Decodable {
    let key: String
    let kind: String
    let title: String
    let from: String?
}

/// A line from the server.
struct Inbound: Decodable {
    let op: String
    /// `state`: everything waiting, the terminal apps you are looking at, the one to bring forward.
    let items: [Item]?
    let looking: [String]?
    let terminal: String?
    /// `post`: the request and its banner's text.
    let key: String?
    let title: String?
    let subtitle: String?
    let body: String?
}

/// The connection to the server.
enum Server {
    static var fd: Int32 = -1

    /// Connects to the Unix socket at `path`; false when it can't.
    static func connect(to path: String) -> Bool {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return false }
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8)
        guard bytes.count < MemoryLayout.size(ofValue: address.sun_path) else {
            close(fd)
            return false
        }
        withUnsafeMutableBytes(of: &address.sun_path) { raw in
            raw.copyBytes(from: bytes)
        }
        let connected = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard connected == 0 else {
            close(fd)
            return false
        }
        self.fd = fd
        return true
    }

    /// Calls `line` on the main thread for each line the server sends, then `ended` when the
    /// connection closes.
    static func read(line: @escaping (String) -> Void, ended: @escaping () -> Void) {
        let fd = self.fd
        Thread.detachNewThread {
            var pending = Data()
            var buffer = [UInt8](repeating: 0, count: 65536)
            while true {
                let count = Darwin.read(fd, &buffer, buffer.count)
                if count <= 0 { break }
                pending.append(contentsOf: buffer[0..<count])
                while let newline = pending.firstIndex(of: 0x0A) {
                    let text = String(decoding: pending[pending.startIndex..<newline], as: UTF8.self)
                    pending.removeSubrange(pending.startIndex...newline)
                    DispatchQueue.main.async { line(text) }
                }
            }
            DispatchQueue.main.async { ended() }
        }
    }

    /// Writes one line to the server.
    static func emit(_ fields: [String: String]) {
        guard fd >= 0,
              let data = try? JSONSerialization.data(withJSONObject: fields, options: [.sortedKeys])
        else { return }
        let line = data + Data("\n".utf8)
        line.withUnsafeBytes { raw in
            var sent = 0
            while sent < raw.count {
                let wrote = Darwin.write(fd, raw.baseAddress! + sent, raw.count - sent)
                if wrote <= 0 { return }
                sent += wrote
            }
        }
    }
}

/// A word for each kind of request, as the menu shows it.
func label(_ kind: String) -> String {
    switch kind {
    case "approval": return "Approval"
    case "decision": return "Decision"
    case "question": return "Question"
    case "plan": return "Plan"
    case "review": return "Review"
    case "report": return "Report"
    case "setup": return "Setup"
    default: return "Notice"
    }
}

// MARK: The app

final class Notifier: NSObject, NSApplicationDelegate, UNUserNotificationCenterDelegate {
    private var status: NSStatusItem?
    private var items: [Item] = []
    private var looking: Set<String> = []
    private var terminal: String?

    func applicationDidFinishLaunching(_ notification: Notification) {
        let center = UNUserNotificationCenter.current()
        center.delegate = self
        center.requestAuthorization(options: [.alert, .sound]) { granted, error in
            var fields = ["event": granted ? "ready" : "denied"]
            if let error { fields["error"] = error.localizedDescription }
            // Whether macOS shows a banner's text or only "Notification" (Show previews).
            center.getNotificationSettings { settings in
                switch settings.showPreviewsSetting {
                case .always: fields["previews"] = "always"
                case .whenAuthenticated: fields["previews"] = "when_unlocked"
                case .never: fields["previews"] = "never"
                @unknown default: break
                }
                DispatchQueue.main.async { Server.emit(fields) }
            }
        }
        let status = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        status.button?.image = Mark.menuBarImage()
        status.button?.imagePosition = .imageLeading
        status.button?.toolTip = "Quartermaster"
        self.status = status
        render()
        Server.read(
            line: { line in
                guard let message = try? JSONDecoder().decode(Inbound.self, from: Data(line.utf8)) else { return }
                self.handle(message)
            },
            ended: { NSApp.terminate(nil) }
        )
    }

    private func handle(_ message: Inbound) {
        switch message.op {
        case "state":
            items = message.items ?? []
            looking = Set(message.looking ?? [])
            terminal = message.terminal
            render()
            withdrawAnswered()
        case "post":
            guard let key = message.key else { return }
            post(key: key, title: message.title ?? "Quartermaster", subtitle: message.subtitle, body: message.body)
        default:
            break
        }
    }

    /// Takes back the banners of requests no longer waiting.
    private func withdrawAnswered() {
        let waiting = Set(items.map(\.key))
        let center = UNUserNotificationCenter.current()
        center.getDeliveredNotifications { delivered in
            let gone = delivered.map(\.request.identifier).filter { !waiting.contains($0) }
            if gone.isEmpty { return }
            center.removeDeliveredNotifications(withIdentifiers: gone)
            DispatchQueue.main.async { Server.emit(["event": "withdrawn", "count": String(gone.count)]) }
        }
    }

    /// Posts a banner, unless you are already looking at Quartermaster.
    private func post(key: String, title: String, subtitle: String?, body: String?) {
        if let front = NSWorkspace.shared.frontmostApplication?.bundleIdentifier, looking.contains(front) {
            Server.emit(["event": "held", "key": key])
            return
        }
        let content = UNMutableNotificationContent()
        content.title = title
        if let subtitle { content.subtitle = subtitle }
        if let body { content.body = body }
        content.sound = .default
        content.userInfo = ["key": key]
        let request = UNNotificationRequest(identifier: key, content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request) { error in
            var fields = ["event": "posted", "key": key]
            if let error { fields["error"] = error.localizedDescription }
            DispatchQueue.main.async { Server.emit(fields) }
        }
    }

    /// The menu bar mark, its count, and its menu.
    private func render() {
        guard let status else { return }
        status.button?.title = items.isEmpty ? "" : " \(items.count)"
        let menu = NSMenu()
        if items.isEmpty {
            menu.addItem(disabled("Nothing waiting on you"))
        } else {
            menu.addItem(disabled(items.count == 1 ? "1 waiting on you" : "\(items.count) waiting on you"))
            for item in items {
                let entry = NSMenuItem(title: item.title, action: #selector(openItem(_:)), keyEquivalent: "")
                entry.target = self
                entry.representedObject = item.key
                if #available(macOS 14.4, *) {
                    entry.subtitle = [label(item.kind), item.from].compactMap { $0 }.joined(separator: " · ")
                }
                menu.addItem(entry)
            }
        }
        menu.addItem(.separator())
        let open = NSMenuItem(title: "Open Quartermaster", action: #selector(openApp), keyEquivalent: "")
        open.target = self
        menu.addItem(open)
        let settings = NSMenuItem(title: "Notification Settings…", action: #selector(openSettings), keyEquivalent: "")
        settings.target = self
        menu.addItem(settings)
        status.menu = menu
    }

    private func disabled(_ title: String) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: nil, keyEquivalent: "")
        item.isEnabled = false
        return item
    }

    @objc private func openItem(_ sender: NSMenuItem) {
        open(key: sender.representedObject as? String)
    }

    @objc private func openApp() {
        open(key: nil)
    }

    @objc private func openSettings() {
        let id = Bundle.main.bundleIdentifier ?? ""
        if let url = URL(string: "x-apple.systempreferences:com.apple.Notifications-Settings.extension?id=\(id)") {
            NSWorkspace.shared.open(url)
        }
    }

    /// Brings your terminal forward and tells the server what to show.
    private func open(key: String?) {
        var fields = ["event": "open"]
        if let key { fields["key"] = key }
        Server.emit(fields)
        guard let terminal, let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: terminal)
        else { return }
        // A background app may not activate another one itself (macOS 14 refuses it); opening the
        // terminal through LaunchServices brings it forward, running or not.
        let configuration = NSWorkspace.OpenConfiguration()
        configuration.activates = true
        NSWorkspace.shared.openApplication(at: url, configuration: configuration)
    }

    // A click on a banner.
    func userNotificationCenter(
        _ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
        withCompletionHandler done: @escaping () -> Void
    ) {
        let key = response.notification.request.content.userInfo["key"] as? String
        DispatchQueue.main.async {
            self.open(key: key)
            done()
        }
    }

    // The helper is never the frontmost app, but a banner shows even if it were.
    func userNotificationCenter(
        _ center: UNUserNotificationCenter, willPresent notification: UNNotification,
        withCompletionHandler done: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        done([.banner, .list, .sound])
    }
}

let arguments = CommandLine.arguments
if arguments.count == 3, arguments[1] == "--make-icon" {
    exit(Mark.writeIcon(to: URL(fileURLWithPath: arguments[2])) ? 0 : 1)
}
guard arguments.count == 3, arguments[1] == "--connect", Server.connect(to: arguments[2]) else {
    FileHandle.standardError.write(Data("usage: quartermaster-notifier --connect <socket>\n".utf8))
    exit(2)
}
let app = NSApplication.shared
let notifier = Notifier()
app.delegate = notifier
app.setActivationPolicy(.accessory)
app.run()
