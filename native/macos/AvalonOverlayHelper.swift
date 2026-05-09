import AppKit
import Carbon.HIToolbox
import CoreGraphics
import Foundation

let overlayResizeDebug = ProcessInfo.processInfo.environment["AVALON_OVERLAY_RESIZE_DEBUG"] == "1"
let mapOverlayMinWidth: CGFloat = 260
let mapOverlayMinHeight: CGFloat = 220
let mapOverlayMaxWidth: CGFloat = 800
let mapOverlayMaxHeight: CGFloat = 700

struct OverlayResult: Encodable {
    let x: Int
    let y: Int
    let width: Int
    let height: Int
    let display_id: String?
    let scale_factor: Double?
    let anchor_x: Double?
    let anchor_y: Double?
    let destination_x: Int?
    let destination_y: Int?
    let destination_width: Int?
    let destination_height: Int?
    let timer_x: Int?
    let timer_y: Int?
    let timer_width: Int?
    let timer_height: Int?
    let cancelled: Bool
}

struct MapOverlayBounds: Codable {
    let x: Int
    let y: Int
    let width: Int
    let height: Int
}

struct AvalonChestInfo: Codable {
    let color: String
    let size: String
    let count: Int
}

struct MapLocation: Codable {
    let id: Int
    let name: String
    let zone_type: String?
    let x: Double?
    let y: Double?
    let avalon_tiers: [Int]?
    let avalon_components: [String]?
    let avalon_chests: [AvalonChestInfo]?
}

struct RouteOverlayLocation: Codable {
    let id: Int
    let name: String
    let normalized_name: String
    let zone_type: String
    let x: Double?
    let y: Double?
}

struct RouteOverlayEdge: Codable {
    let id: Int
    let from_location_id: Int
    let to_location_id: Int
    let from_location_name: String
    let to_location_name: String
    let source: String
}

struct HotkeyBinding: Codable {
    let key_code: UInt32
    let modifiers: UInt32
    let label: String
}

func colorForZoneType(_ zoneType: String?, isCurrent: Bool) -> NSColor {
    if isCurrent {
        return NSColor(calibratedRed: 0.13, green: 0.77, blue: 0.37, alpha: 1.0)
    }

    switch zoneType ?? "unknown" {
    case "avalon":
        return NSColor(calibratedRed: 0.55, green: 0.36, blue: 0.96, alpha: 1.0)
    case "blue", "yellow":
        return NSColor(calibratedRed: 0.23, green: 0.51, blue: 0.96, alpha: 1.0)
    case "red":
        return NSColor(calibratedRed: 0.94, green: 0.27, blue: 0.27, alpha: 1.0)
    case "outlands_black":
        return NSColor(calibratedRed: 0.05, green: 0.07, blue: 0.10, alpha: 1.0)
    default:
        return NSColor(calibratedRed: 0.61, green: 0.64, blue: 0.69, alpha: 1.0)
    }
}

struct MapEdge: Codable {
    let id: Int
    let source: String?
    let from_location_id: Int
    let to_location_id: Int
    let from_location_name: String
    let to_location_name: String
    let last_seen_at: String
    let status: String?
}

struct MapOverlayData: Codable {
    let route_expires_at: String?
    let route_edges_count: Int?
    let bridge_locations: [RouteOverlayLocation]
    let bridge_edges: [RouteOverlayEdge]
    let current_location: String?
    let last_portal_destination: String?
    let last_portal_expires_in_seconds: Int?
    let locations: [MapLocation]
    let edges: [MapEdge]
    let route_locations: [RouteOverlayLocation]
    let route_edges: [RouteOverlayEdge]
    let last_capture_status: String
    let capture_mode: String
    let ocr_mode: String
    let db_status: String
    let known_locations_count: Int?
    let known_edges_count: Int?
}

struct OverlayCommand: Decodable {
    let type: String
    let enabled: Bool?
    let bounds: MapOverlayBounds?
    let data: MapOverlayData?
    let toggle_overlay: HotkeyBinding?
    let capture_current: HotkeyBinding?
    let capture_portal: HotkeyBinding?
}

struct CaptureOcrOutput: Encodable {
    let text: String
    let confidence: Double?
    let engine: String
    let image_path: String
    let width: Int
    let height: Int
    let duration_ms: Int
    let screen_recording_permission: Bool
    let lines: [CaptureOcrLine]
}

struct CaptureOcrLine: Encodable {
    let text: String
    let confidence: Double?
    let bbox: CaptureOcrBbox?
}

struct CaptureOcrBbox: Encodable {
    let x: Double
    let y: Double
    let width: Double
    let height: Double
}
struct CaptureServerRequest: Decodable {
    let cmd: String
    let kind: String
    let x: Int
    let y: Int
    let width: Int
    let height: Int
    let display_id: String?
    let center_cursor: Bool?
    let portal_anchor_x: Double?
    let portal_anchor_y: Double?
    let portal_x: Int?
    let portal_y: Int?
    let portal_width: Int?
    let portal_height: Int?
    let output_dir: String
}
final class SelectionPanel: NSPanel {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }
}

final class MapPanel: NSPanel {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }
}



final class SelectionOverlayController: NSObject, NSApplicationDelegate {
    private var windows: [SelectionPanel] = []
    private let mode: String

    init(mode: String) {
        self.mode = mode
        super.init()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
        NSApp.activate(ignoringOtherApps: true)

        let screens = NSScreen.screens
        if screens.isEmpty {
            finish(cancelled: true)
            return
        }

        for screen in screens {
            let panel = SelectionPanel(
                contentRect: screen.frame,
                styleMask: [.borderless, .nonactivatingPanel],
                backing: .buffered,
                defer: false,
                screen: screen
            )
            panel.isReleasedWhenClosed = false
            panel.isOpaque = false
            panel.backgroundColor = .clear
            panel.hasShadow = false
            panel.level = .screenSaver
            panel.ignoresMouseEvents = false
            panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .stationary, .ignoresCycle]

            let view = SelectionOverlayView(frame: NSRect(origin: .zero, size: screen.frame.size), screen: screen, mode: mode)
            view.onComplete = { [weak self] result in self?.printAndQuit(result) }
            view.onCancel = { [weak self] in self?.finish(cancelled: true) }
            panel.contentView = view
            panel.orderFrontRegardless()
            windows.append(panel)
        }

        windows.first?.makeKeyAndOrderFront(nil)
    }

    private func finish(cancelled: Bool) {
        printAndQuit(
            OverlayResult(
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                display_id: nil,
                scale_factor: nil,
                anchor_x: nil,
                anchor_y: nil,
                destination_x: nil,
                destination_y: nil,
                destination_width: nil,
                destination_height: nil,
                timer_x: nil,
                timer_y: nil,
                timer_width: nil,
                timer_height: nil,
                cancelled: cancelled
            )
        )
    }

    private func printAndQuit(_ result: OverlayResult) {
        let encoder = JSONEncoder()
        if let data = try? encoder.encode(result), let json = String(data: data, encoding: .utf8) {
            print(json)
        }
        NSApp.terminate(nil)
    }
}

final class SelectionOverlayView: NSView {
    var onComplete: ((OverlayResult) -> Void)?
    var onCancel: (() -> Void)?

    private let targetScreen: NSScreen
    private let mode: String
    private var startPoint: NSPoint?
    private var currentPoint: NSPoint?
    private var anchorPoint: NSPoint?
    private var frozenImage: CGImage?
    private var portalStripStep = 0
    private var portalDestinationRect: NSRect?
    private let labelAttributes: [NSAttributedString.Key: Any] = [
        .font: NSFont.systemFont(ofSize: 14, weight: .medium),
        .foregroundColor: NSColor.white
    ]

    init(frame frameRect: NSRect, screen: NSScreen, mode: String) {
        self.targetScreen = screen
        self.mode = mode
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.backgroundColor = NSColor.clear.cgColor
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    override var acceptsFirstResponder: Bool { true }
    override var isOpaque: Bool { false }

    override func viewDidMoveToWindow() {
        window?.makeFirstResponder(self)
    }

    override func draw(_ dirtyRect: NSRect) {
        if let frozenImage {
            NSImage(cgImage: frozenImage, size: bounds.size).draw(in: bounds)
        } else {
            NSColor.clear.setFill()
            dirtyRect.fill()
        }
        drawInstructionLabel()
        drawCrosshair()

        if let portalDestinationRect {
            drawSelectionRect(portalDestinationRect, color: .systemGreen)
        }

        guard let startPoint, let currentPoint else { return }
        let rect = normalizedRect(from: startPoint, to: currentPoint)
        if rect.width < 1 || rect.height < 1 { return }

        drawSelectionRect(rect, color: .systemTeal)
    }

    override func mouseDown(with event: NSEvent) {
        if mode == "portal-strips" && frozenImage == nil {
            return
        }
        if mode != "portal-strips" {
            anchorPoint = convert(event.locationInWindow, from: nil)
        }
        startPoint = convert(event.locationInWindow, from: nil)
        currentPoint = startPoint
        needsDisplay = true
    }

    override func rightMouseDown(with event: NSEvent) {
        guard mode == "portal-strips" else {
            super.rightMouseDown(with: event)
            return
        }
        anchorPoint = convert(event.locationInWindow, from: nil)
        freezeCurrentScreen()
        portalStripStep = 1
        startPoint = nil
        currentPoint = nil
        needsDisplay = true
    }

    override func mouseDragged(with event: NSEvent) {
        if mode == "portal-strips" && frozenImage == nil {
            return
        }
        currentPoint = convert(event.locationInWindow, from: nil)
        needsDisplay = true
    }

    override func mouseUp(with event: NSEvent) {
        currentPoint = convert(event.locationInWindow, from: nil)
        guard let dragStart = startPoint, let dragEnd = currentPoint else { return }
        let rect = normalizedRect(from: dragStart, to: dragEnd)
        if rect.width < 4 || rect.height < 4 {
            onCancel?()
            return
        }
        if mode == "portal-strips" {
            if portalStripStep == 1 {
                portalDestinationRect = rect
                portalStripStep = 2
                self.startPoint = nil
                self.currentPoint = nil
                needsDisplay = true
                return
            }
            if portalStripStep == 2, let destination = portalDestinationRect {
                onComplete?(portalStripResult(destination: destination, timer: rect))
                return
            }
        }
        onComplete?(result(for: rect))
    }

    override func keyDown(with event: NSEvent) {
        if event.keyCode == 53 {
            onCancel?()
            return
        }
        super.keyDown(with: event)
    }

    private func drawInstructionLabel() {
        let message: String
        if mode == "portal-size" {
            message = "Outline the portal tooltip plaque. Escape to cancel."
        } else if mode == "portal-strips" {
            if frozenImage == nil {
                message = "Right-click the portal tooltip to freeze the game screen. Escape to cancel."
            } else if portalStripStep == 1 {
                message = "Drag the destination-name strip. Escape to cancel."
            } else {
                message = "Drag the timer strip. Escape to cancel."
            }
        } else if mode == "diagnostic" {
            message = "Transparent overlay test. Drag anywhere. Escape to cancel."
        } else {
            message = "Drag current-location text region. Escape to cancel."
        }

        let padding: CGFloat = 10
        let textSize = message.size(withAttributes: labelAttributes)
        let box = NSRect(x: 18, y: bounds.height - textSize.height - 28, width: textSize.width + padding * 2, height: textSize.height + padding)
        NSColor.black.withAlphaComponent(0.72).setFill()
        NSBezierPath(roundedRect: box, xRadius: 6, yRadius: 6).fill()
        message.draw(at: NSPoint(x: box.minX + padding, y: box.minY + padding / 2), withAttributes: labelAttributes)
    }

    private func drawSelectionRect(_ rect: NSRect, color: NSColor) {
        color.withAlphaComponent(0.08).setFill()
        rect.fill()
        let border = NSBezierPath(rect: rect)
        color.setStroke()
        border.lineWidth = 2
        border.stroke()

        let sizeText = "\(Int(rect.width)) x \(Int(rect.height))"
        sizeText.draw(
            at: NSPoint(x: rect.minX + 8, y: max(rect.minY + 8, 8)),
            withAttributes: labelAttributes
        )
    }

    private func freezeCurrentScreen() {
        guard let windowNumber = window?.windowNumber else { return }
        let windowId = CGWindowID(windowNumber)
        let displayId = displayID(for: targetScreen)
        let displayBounds = displayId.map { CGDisplayBounds($0) } ?? targetScreen.frame
        frozenImage = CGWindowListCreateImage(
            displayBounds,
            .optionOnScreenBelowWindow,
            windowId,
            [.bestResolution, .nominalResolution]
        )
    }

    private func drawCrosshair() {
        guard let location = window?.mouseLocationOutsideOfEventStream else { return }
        let point = convert(location, from: nil)
        let path = NSBezierPath()
        path.move(to: NSPoint(x: point.x - 12, y: point.y))
        path.line(to: NSPoint(x: point.x + 12, y: point.y))
        path.move(to: NSPoint(x: point.x, y: point.y - 12))
        path.line(to: NSPoint(x: point.x, y: point.y + 12))
        NSColor.systemYellow.setStroke()
        path.lineWidth = 1
        path.stroke()
    }

    private func normalizedRect(from start: NSPoint, to end: NSPoint) -> NSRect {
        NSRect(x: min(start.x, end.x), y: min(start.y, end.y), width: abs(start.x - end.x), height: abs(start.y - end.y))
    }

    private func result(for rect: NSRect) -> OverlayResult {
        result(for: rect, destination: nil, timer: nil)
    }

    private func portalStripResult(destination: NSRect, timer: NSRect) -> OverlayResult {
        result(for: destination.union(timer), destination: destination, timer: timer)
    }

    private func result(for rect: NSRect, destination: NSRect?, timer: NSRect?) -> OverlayResult {
        let displayId = displayID(for: targetScreen)
        let scale = targetScreen.backingScaleFactor
        let mainCaptureRect = makeCaptureRect(for: rect)
        let anchor = anchorPoint.map { point in
            capturePoint(for: point)
        }
        let destinationCapture = destination.map { makeCaptureRect(for: $0) }
        let timerCapture = timer.map { makeCaptureRect(for: $0) }

        return OverlayResult(
            x: Int(mainCaptureRect.minX.rounded()),
            y: Int(mainCaptureRect.minY.rounded()),
            width: Int(mainCaptureRect.width.rounded()),
            height: Int(mainCaptureRect.height.rounded()),
            display_id: displayId.map { String($0) },
            scale_factor: Double(scale),
            anchor_x: anchor.map { Double($0.x) },
            anchor_y: anchor.map { Double($0.y) },
            destination_x: destinationCapture.map { Int($0.minX.rounded()) },
            destination_y: destinationCapture.map { Int($0.minY.rounded()) },
            destination_width: destinationCapture.map { Int($0.width.rounded()) },
            destination_height: destinationCapture.map { Int($0.height.rounded()) },
            timer_x: timerCapture.map { Int($0.minX.rounded()) },
            timer_y: timerCapture.map { Int($0.minY.rounded()) },
            timer_width: timerCapture.map { Int($0.width.rounded()) },
            timer_height: timerCapture.map { Int($0.height.rounded()) },
            cancelled: false
        )
    }

    private func makeCaptureRect(for viewRect: NSRect) -> NSRect {
        let displayId = displayID(for: targetScreen)
        let displayBounds = displayId.map { CGDisplayBounds($0) } ?? targetScreen.frame

        let result = NSRect(
            x: displayBounds.minX + viewRect.minX,
            y: displayBounds.minY + (targetScreen.frame.height - viewRect.maxY),
            width: viewRect.width,
            height: viewRect.height
        )

        fputs(
            "[selection-debug] viewRect=\(viewRect) targetScreen.frame=\(targetScreen.frame) displayBounds=\(displayBounds) captureRect=\(result)\n",
            stderr
        )
        fflush(stderr)

        return result
    }

    private func capturePoint(for viewPoint: NSPoint) -> NSPoint {
        let displayId = displayID(for: targetScreen)
        let displayBounds = displayId.map { CGDisplayBounds($0) } ?? targetScreen.frame

        return NSPoint(
            x: displayBounds.minX + viewPoint.x,
            y: displayBounds.minY + (targetScreen.frame.height - viewPoint.y)
        )
    }

    private func displayID(for screen: NSScreen) -> CGDirectDisplayID? {
        guard let number = screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber else {
            return nil
        }
        return CGDirectDisplayID(number.uint32Value)
    }
}

final class MapOverlayController: NSObject, NSApplicationDelegate {
    private var panel: MapPanel?
    private var overlayView: MapOverlayView?
    private var boundsStatePath: String?
    private var hotKeyRefs: [EventHotKeyRef?] = [nil, nil, nil]
    private var hotkeyToggle = HotkeyBinding(key_code: UInt32(kVK_ANSI_M), modifiers: UInt32(optionKey | shiftKey), label: "⌥⇧M")
    private var hotkeyCurrent = HotkeyBinding(key_code: UInt32(kVK_ANSI_L), modifiers: UInt32(optionKey | shiftKey), label: "⌥⇧L")
    private var hotkeyPortal = HotkeyBinding(key_code: UInt32(kVK_ANSI_P), modifiers: UInt32(optionKey | shiftKey), label: "⌥⇧P")
    private var moveStartScreenPoint: NSPoint?
    private var moveStartFrame: NSRect?
    private var resizeStartScreenPoint: NSPoint?
    private var resizeStartFrame: NSRect?
    private var visible = false
    private var interactive = false
    private var overlayData = MapOverlayData(
        route_expires_at: nil,
        route_edges_count: 0,
        bridge_locations: [],
        bridge_edges: [],
        current_location: nil,
        last_portal_destination: nil,
        last_portal_expires_in_seconds: nil,
        locations: [],
        edges: [],
        route_locations: [],
        route_edges: [],
        last_capture_status: "No captures yet",
        capture_mode: "manual/mock capture pending",
        ocr_mode: "manual/mock",
        db_status: "SQLite connected",
        known_locations_count: 0,
        known_edges_count: 0
    )

    init(boundsStatePath: String?) {
        self.boundsStatePath = boundsStatePath
        super.init()
    }

    func setShortcutDepth(_ value: Int) {
        print("{\"event\":\"set_shortcut_depth\",\"value\":\(value)}")
        fflush(stdout)
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
        createPanel()
        registerHotKey()
        startCommandReader()
    }

    func findRoute(from: String, to: String) {
        fputs("[overlay-route] Find clicked from=\(from) to=\(to)\n", stderr)
        fflush(stderr)

        let payload: [String: Any] = [
            "event": "find_route",
            "from": from,
            "to": to
        ]

        if let data = try? JSONSerialization.data(withJSONObject: payload),
           let json = String(data: data, encoding: .utf8) {
            print(json)
            fflush(stdout)
        }
    }

    func clearRoute() {
        print("{\"event\":\"clear_route\"}")
        fflush(stdout)
    }

    func toggleFromHotkey() {
        toggle()
    }

    func captureCurrentFromHotkey() {
        print("{\"event\":\"capture_current\"}")
        fflush(stdout)
    }

    func capturePortalFromHotkey() {
        print("{\"event\":\"capture_portal\"}")
        fflush(stdout)
    }

    func undoLastAction() {
        print("{\"event\":\"undo_last_action\"}")
        fflush(stdout)
    }

    func deleteEdge(id: Int) {
        print("{\"event\":\"delete_edge\",\"edge_id\":\(id)}")
        fflush(stdout)
    }

    private func createPanel() {
        let initialBounds = loadBoundsFromDisk() ?? MapOverlayBounds(x: 80, y: 120, width: 360, height: 300)
        let frame = frameFromTopLeftBounds(initialBounds)
        let panel = MapPanel(
            contentRect: frame,
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )
        panel.isReleasedWhenClosed = false
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = false
        panel.level = .screenSaver
        panel.ignoresMouseEvents = true
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .stationary, .ignoresCycle]

        let view = MapOverlayView(frame: NSRect(origin: .zero, size: frame.size))
        view.data = overlayData
        view.onMoveStart = { [weak self] point in self?.beginMove(at: point) }
        view.onMove = { [weak self] point in self?.movePanel(to: point) }
        view.onResizeStart = { [weak self] point in self?.beginResize(at: point) }
        view.onResize = { [weak self] point in self?.resizePanel(to: point) }
        view.onDeleteEdge = { [weak self] edgeId in self?.deleteEdge(id: edgeId) }
        view.onUndoLastAction = { [weak self] in
            self?.undoLastAction()
        }
        view.onFindRoute = { [weak self] from, to in
            self?.findRoute(from: from, to: to)
        }

        view.onClearRoute = { [weak self] in
            self?.clearRoute()
        }
        view.onShortcutDepthChanged = { [weak self] value in
            self?.setShortcutDepth(value)
        }

        panel.contentView = view

        self.panel = panel
        self.overlayView = view
        setInteractive(false)
    }

    private func registerHotKey() {
        unregisterHotKeys()

        registerHotKey(id: 1, keyCode: hotkeyToggle.key_code, modifiers: hotkeyToggle.modifiers, index: 0)
        registerHotKey(id: 2, keyCode: hotkeyCurrent.key_code, modifiers: hotkeyCurrent.modifiers, index: 1)
        registerHotKey(id: 3, keyCode: hotkeyPortal.key_code, modifiers: hotkeyPortal.modifiers, index: 2)

        var eventType = EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
        InstallEventHandler(
            GetApplicationEventTarget(),
            mapOverlayHotKeyHandler,
            1,
            &eventType,
            Unmanaged.passUnretained(self).toOpaque(),
            nil
        )
    }

    private func unregisterHotKeys() {
        for index in hotKeyRefs.indices {
            if let ref = hotKeyRefs[index] {
                UnregisterEventHotKey(ref)
                hotKeyRefs[index] = nil
            }
        }
}

    private func registerHotKey(id: UInt32, keyCode: UInt32, modifiers: UInt32, index: Int) {
        let hotKeyID = EventHotKeyID(signature: OSType(0x414D4F4D), id: id)
        let status = RegisterEventHotKey(keyCode, modifiers, hotKeyID, GetApplicationEventTarget(), 0, &hotKeyRefs[index])
        if status != noErr {
            print("{\"event\":\"hotkey_error\",\"id\":\(id),\"status\":\(status)}")
            fflush(stdout)
        }
    }

    private func startCommandReader() {
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            while let line = readLine() {
                guard let data = line.data(using: .utf8),
                      let command = try? JSONDecoder().decode(OverlayCommand.self, from: data) else {
                    continue
                }
                DispatchQueue.main.async {
                    self?.handle(command)
                }
            }
        }
    }

    private func handle(_ command: OverlayCommand) {
        switch command.type {
        case "show":
            show()
        case "hide":
            hide()
        case "hotkeys":
            if let value = command.toggle_overlay {
                hotkeyToggle = value
            }
            if let value = command.capture_current {
                hotkeyCurrent = value
            }
            if let value = command.capture_portal {
                hotkeyPortal = value
            }
            registerHotKey()
        case "toggle":
            toggle()
        case "interactive":
            setInteractive(command.enabled ?? false)
        case "bounds":
            if let bounds = command.bounds {
                setBounds(bounds)
            }
        case "data":
            if let data = command.data {
                overlayData = data
                overlayView?.data = data
                overlayView?.needsDisplay = true
            }
        case "reset":
            setBounds(MapOverlayBounds(x: 80, y: 120, width: 360, height: 300))
        default:
            break
        }
    }

    private func show() {
        guard let panel else { return }
        visible = true
        panel.orderFrontRegardless()
        print("{\"event\":\"visible\",\"visible\":true}")
        fflush(stdout)
    }

    private func hide() {
        visible = false
        panel?.orderOut(nil)
        print("{\"event\":\"visible\",\"visible\":false}")
        fflush(stdout)
    }

    private func toggle() {
        visible ? hide() : show()
    }

    private func setInteractive(_ enabled: Bool) {
        interactive = enabled
        panel?.ignoresMouseEvents = !enabled
        overlayView?.interactive = enabled
        if !enabled {
            overlayView?.deleteEdgeMode = false
        }
        overlayView?.needsDisplay = true

        if enabled {
            panel?.makeKeyAndOrderFront(nil)
            panel?.makeFirstResponder(overlayView)
        }

        print("{\"event\":\"interactive\",\"enabled\":\(enabled ? "true" : "false")}")
        fflush(stdout)
    }

    private func setBounds(_ bounds: MapOverlayBounds) {
        panel?.setFrame(frameFromTopLeftBounds(sanitize(bounds)), display: true)
        persistCurrentBounds()
    }

    private func beginMove(at point: NSPoint) {
        guard let panel else { return }
        moveStartScreenPoint = point
        moveStartFrame = panel.frame
    }

    private func movePanel(to point: NSPoint) {
        guard let panel else { return }
        guard let moveStartScreenPoint, let moveStartFrame else { return }
        var frame = moveStartFrame
        frame.origin.x += point.x - moveStartScreenPoint.x
        frame.origin.y += point.y - moveStartScreenPoint.y
        panel.setFrame(frame, display: true)
        persistCurrentBounds()
    }

    private func beginResize(at point: NSPoint) {
        guard let panel else { return }
        resizeStartScreenPoint = point
        resizeStartFrame = panel.frame
        debugResize("begin", start: point, current: point, oldFrame: panel.frame, newFrame: panel.frame)
    }

    private func resizePanel(to point: NSPoint) {
        guard let panel else { return }
        guard let resizeStartScreenPoint, let resizeStartFrame else { return }

        let dx = point.x - resizeStartScreenPoint.x
        let dy = point.y - resizeStartScreenPoint.y
        let newWidth = clamp(resizeStartFrame.width + dx, min: mapOverlayMinWidth, max: mapOverlayMaxWidth)
        let newHeight = clamp(resizeStartFrame.height - dy, min: mapOverlayMinHeight, max: mapOverlayMaxHeight)

        var frame = resizeStartFrame
        frame.size.width = newWidth
        frame.size.height = newHeight
        frame.origin.y = resizeStartFrame.maxY - newHeight

        debugResize("drag", start: resizeStartScreenPoint, current: point, oldFrame: panel.frame, newFrame: frame)
        panel.setFrame(frame, display: true)
        persistCurrentBounds()
    }

    private func persistCurrentBounds() {
        guard let panel else { return }
        let bounds = topLeftBoundsFromFrame(panel.frame)
        if let data = try? JSONEncoder().encode(bounds), let path = boundsStatePath {
            try? data.write(to: URL(fileURLWithPath: path), options: .atomic)
        }
        if let data = try? JSONEncoder().encode(BoundsEvent(event: "bounds", bounds: bounds)),
           let json = String(data: data, encoding: .utf8) {
            print(json)
            fflush(stdout)
        }
    }

    private func loadBoundsFromDisk() -> MapOverlayBounds? {
        guard let boundsStatePath else { return nil }
        return try? JSONDecoder().decode(MapOverlayBounds.self, from: Data(contentsOf: URL(fileURLWithPath: boundsStatePath)))
    }

    private func frameFromTopLeftBounds(_ rawBounds: MapOverlayBounds) -> NSRect {
        let bounds = sanitize(rawBounds)
        let screen = NSScreen.main ?? NSScreen.screens.first
        let screenFrame = screen?.frame ?? NSRect(x: 0, y: 0, width: 1440, height: 900)
        let x = screenFrame.minX + CGFloat(bounds.x)
        let y = screenFrame.maxY - CGFloat(bounds.y + bounds.height)
        return NSRect(x: x, y: y, width: CGFloat(bounds.width), height: CGFloat(bounds.height))
    }

    private func topLeftBoundsFromFrame(_ frame: NSRect) -> MapOverlayBounds {
        let screen = NSScreen.main ?? NSScreen.screens.first
        let screenFrame = screen?.frame ?? NSRect(x: 0, y: 0, width: 1440, height: 900)
        return MapOverlayBounds(
            x: Int((frame.minX - screenFrame.minX).rounded()),
            y: Int((screenFrame.maxY - frame.maxY).rounded()),
            width: Int(frame.width.rounded()),
            height: Int(frame.height.rounded())
        )
    }

    private func sanitize(_ bounds: MapOverlayBounds) -> MapOverlayBounds {
        MapOverlayBounds(
            x: bounds.x,
            y: bounds.y,
            width: Int(clamp(CGFloat(bounds.width), min: mapOverlayMinWidth, max: mapOverlayMaxWidth)),
            height: Int(clamp(CGFloat(bounds.height), min: mapOverlayMinHeight, max: mapOverlayMaxHeight))
        )
    }

    private func debugResize(_ phase: String, start: NSPoint, current: NSPoint, oldFrame: NSRect, newFrame: NSRect) {
        guard overlayResizeDebug else { return }
        let scale = panel?.screen?.backingScaleFactor ?? NSScreen.main?.backingScaleFactor ?? 1
        fputs(
            "[resize:\(phase)] start=(\(start.x),\(start.y)) current=(\(current.x),\(current.y)) dx=\(current.x - start.x) dy=\(current.y - start.y) old=\(oldFrame) new=\(newFrame) scale=\(scale)\n",
            stderr
        )
    }
}

struct BoundsEvent: Encodable {
    let event: String
    let bounds: MapOverlayBounds
}

final class MapOverlayView: NSView {
    private struct EdgeHitTarget {
        let id: Int
        let from: NSPoint
        let to: NSPoint

        func contains(_ point: NSPoint, threshold: CGFloat) -> Bool {
            let dx = to.x - from.x
            let dy = to.y - from.y
            let lengthSquared = dx * dx + dy * dy
            if lengthSquared <= 0.0001 {
                let px = point.x - from.x
                let py = point.y - from.y
                return sqrt(px * px + py * py) <= threshold
            }

            let t = ((point.x - from.x) * dx + (point.y - from.y) * dy) / lengthSquared
            let clampedT = max(0, min(1, t))
            let closestX = from.x + clampedT * dx
            let closestY = from.y + clampedT * dy
            let px = point.x - closestX
            let py = point.y - closestY
            return sqrt(px * px + py * py) <= threshold
        }
    }

    var onShortcutDepthChanged: ((Int) -> Void)?
    private var shortcutDepth = 3
    var data: MapOverlayData = MapOverlayData(
        route_expires_at: nil,
        route_edges_count: 0,
        bridge_locations: [],
        bridge_edges: [],
        current_location: nil,

        last_portal_destination: nil,
        last_portal_expires_in_seconds: nil,
        locations: [],
        edges: [],
        route_locations: [],
        route_edges: [],
        last_capture_status: "No captures yet",
        capture_mode: "manual/mock capture pending",
        ocr_mode: "manual/mock",
        db_status: "SQLite connected",
        known_locations_count: 0,
        known_edges_count: 0
    )
        private func selectedLocationName() -> String? {
            guard let selectedLocationId else {
                return nil
            }

            if let loc = data.locations.first(where: { $0.id == selectedLocationId }) {
                return loc.name
            }

            if let loc = data.route_locations.first(where: { $0.id == selectedLocationId }) {
                return loc.name
            }

            if let loc = data.bridge_locations.first(where: { $0.id == selectedLocationId }) {
                return loc.name
            }

            return nil
        }
        private func copySelectedRouteToClipboard() {
            guard !data.route_locations.isEmpty else {
                return
            }

            var lines: [String] = []

            if let expiresAt = data.route_expires_at {
                let formatter = ISO8601DateFormatter()
                formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]

                var date = formatter.date(from: expiresAt)

                if date == nil {
                    let fallback = ISO8601DateFormatter()
                    fallback.formatOptions = [.withInternetDateTime]
                    date = fallback.date(from: expiresAt)
                }

                if let date {
                    let timestamp = Int(date.timeIntervalSince1970)
                    lines.append("<t:\(timestamp):R>")
                } else {
                    fputs("[route-copy] failed to parse route_expires_at=\(expiresAt)\n", stderr)
                    fflush(stderr)
                }
            }

            for (index, location) in data.route_locations.enumerated() {
                lines.append("\(index + 1))\(location.name)")
            }

            let text = lines.joined(separator: "\n")

            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(text, forType: .string)
        }
    private func drawChestIcon(chest: AvalonChestInfo, rect: NSRect) {
        let body = NSBezierPath(roundedRect: rect, xRadius: 5, yRadius: 5)

        chestColor(chest.color).setFill()
        body.fill()

        NSColor(calibratedWhite: 0.0, alpha: 0.45).setStroke()
        body.lineWidth = 2
        body.stroke()

        NSColor.white.withAlphaComponent(0.22).setFill()
        NSBezierPath(
            roundedRect: NSRect(
                x: rect.minX + 5,
                y: rect.midY,
                width: rect.width - 10,
                height: rect.height * 0.32
            ),
            xRadius: 3,
            yRadius: 3
        ).fill()

        let bubbleSize: CGFloat = 22
        let bubble = NSRect(
            x: rect.maxX - bubbleSize * 0.62,
            y: rect.maxY - bubbleSize * 0.52,
            width: bubbleSize,
            height: bubbleSize
        )

        NSColor.systemRed.setFill()
        NSBezierPath(ovalIn: bubble).fill()

        "\(chest.count)".draw(
            at: NSPoint(x: bubble.minX + 7, y: bubble.minY + 4),
            withAttributes: [
                .font: NSFont.systemFont(ofSize: 13, weight: .bold),
                .foregroundColor: NSColor.white
            ]
        )

        if chest.size == "large" {
            let plusRect = NSRect(
                x: rect.maxX - 3,
                y: rect.maxY - 3,
                width: 13,
                height: 13
            )

            NSColor.systemOrange.setFill()
            NSBezierPath(ovalIn: plusRect).fill()

            "+".draw(
                at: NSPoint(x: plusRect.minX + 3, y: plusRect.minY - 1),
                withAttributes: [
                    .font: NSFont.systemFont(ofSize: 12, weight: .bold),
                    .foregroundColor: NSColor.white
                ]
            )
        }
}

private func chestColor(_ color: String) -> NSColor {
    switch color {
    case "green":
        return NSColor(calibratedRed: 0.22, green: 0.85, blue: 0.25, alpha: 1.0)
    case "blue":
        return NSColor(calibratedRed: 0.10, green: 0.75, blue: 0.95, alpha: 1.0)
    case "gold":
        return NSColor(calibratedRed: 1.0, green: 0.67, blue: 0.10, alpha: 1.0)
    default:
        return NSColor(calibratedWhite: 0.55, alpha: 1.0)
    }
}
    private func drawAvalonInfoPopup() {
        guard let id = avalonInfoLocationId,
              let point = avalonInfoPoint,
              let location = data.locations.first(where: { $0.id == id })
        else {
            return
        }

        let chests = (location.avalon_chests ?? []).filter { $0.count > 0 }

        guard !chests.isEmpty else {
            return
        }

        let iconSize: CGFloat = 34
        let gap: CGFloat = 14
        let width = CGFloat(chests.count) * iconSize + CGFloat(max(chests.count - 1, 0)) * gap + 20
        let height: CGFloat = 58

        var box = NSRect(
            x: point.x + 14,
            y: point.y + 14,
            width: width,
            height: height
        )

        if box.maxX > bounds.maxX - 8 {
            box.origin.x = point.x - width - 14
        }

        if box.maxY > bounds.maxY - 8 {
            box.origin.y = point.y - height - 14
        }

        NSColor(calibratedWhite: 0.03, alpha: 0.88).setFill()
        NSBezierPath(roundedRect: box, xRadius: 8, yRadius: 8).fill()

        for (index, chest) in chests.enumerated() {
            let x = box.minX + 10 + CGFloat(index) * (iconSize + gap)
            let y = box.minY + 10

            drawChestIcon(
                chest: chest,
                rect: NSRect(x: x, y: y, width: iconSize, height: iconSize)
            )
        }
    }
    private func drawAvalonTierBorder(location: MapLocation, nodeRect: NSRect, isCurrent: Bool) {
        guard location.zone_type == "avalon" else { return }

        let tiers = location.avalon_tiers ?? []

        if tiers.contains(8) {
            drawT8AvalonBorder(nodeRect: nodeRect, isCurrent: isCurrent)
            return
        }

        guard let tierColor = avalonTierBorderColor(location) else { return }

        tierColor.setStroke()
        let tierBorder = NSBezierPath(ovalIn: nodeRect.insetBy(dx: -5.0, dy: -5.0))
        tierBorder.lineWidth = 4.5
        tierBorder.stroke()
    }

    private func drawT8AvalonBorder(nodeRect: NSRect, isCurrent: Bool) {
        let outerRect = nodeRect.insetBy(dx: -4.0, dy: -4.0)
        let innerRect = nodeRect.insetBy(dx: -1.5, dy: -1.5)

        drawStripedSilverRing(outerRect: outerRect, innerRect: innerRect)
        colorForZoneType("avalon", isCurrent: isCurrent).setFill()
        NSBezierPath(ovalIn: nodeRect).fill()
        drawJaggedRing(around: outerRect)
    }

    private func drawStripedSilverRing(outerRect: NSRect, innerRect: NSRect) {
        let outer = NSBezierPath(ovalIn: outerRect)

        NSGraphicsContext.saveGraphicsState()
        outer.addClip()

        NSColor.white.setFill()
        outer.fill()

        NSColor(calibratedWhite: 0.68, alpha: 1.0).setStroke()

        let step: CGFloat = 7
        let start = outerRect.minX - outerRect.height
        let end = outerRect.maxX + outerRect.height

        var x = start
        while x < end {
            let line = NSBezierPath()
            line.move(to: NSPoint(x: x, y: outerRect.minY - 4))
            line.line(to: NSPoint(x: x + outerRect.height + 8, y: outerRect.maxY + 4))
            line.lineWidth = 2.2
            line.stroke()
            x += step
        }

        NSGraphicsContext.restoreGraphicsState()

        NSColor.white.withAlphaComponent(0.95).setStroke()
        outer.lineWidth = 1.2
        outer.stroke()
    }

    private func drawJaggedRing(around rect: NSRect) {
        let center = NSPoint(x: rect.midX, y: rect.midY)
        let baseRadius = max(rect.width, rect.height) / 2.0 + 2.0
        let teeth: Int = 18

        let path = NSBezierPath()

        for i in 0..<(teeth * 2) {
            let angle = CGFloat(i) * CGFloat.pi / CGFloat(teeth)
            let radius = i % 2 == 0 ? baseRadius + 3.0 : baseRadius - 1.0

            let point = NSPoint(
                x: center.x + cos(angle) * radius,
                y: center.y + sin(angle) * radius
            )

            if i == 0 {
                path.move(to: point)
            } else {
                path.line(to: point)
            }
        }

        path.close()

        NSColor(calibratedWhite: 0.92, alpha: 0.95).setStroke()
        path.lineWidth = 1.488
        path.stroke()
    }
    private func avalonTierBorderColor(_ location: MapLocation) -> NSColor? {
        guard location.zone_type == "avalon" else { return nil }

        let tiers = location.avalon_tiers ?? []

        if tiers.contains(8) {
            return NSColor(calibratedWhite: 0.88, alpha: 1.0)
        }

//         if tiers.contains(6) {
//             return NSColor(calibratedRed: 0.72, green: 0.32, blue: 0.05, alpha: 1.0)
//         }

        if tiers.contains(4) {
            return NSColor(calibratedRed: 0.05, green: 0.45, blue: 0.36, alpha: 1.0)
        }

        return nil
    }
    private var avalonInfoLocationId: Int?
    private var avalonInfoPoint: NSPoint?
    var onFindRoute: ((String, String) -> Void)?
    var onClearRoute: (() -> Void)?
    var onDeleteEdge: ((Int) -> Void)?

    private var routeFromText = ""
    private var routeToText = ""
    private var activeRouteField: RouteField?

    private enum RouteField {
        case from
        case to
    }
    override var acceptsFirstResponder: Bool { true }
    var interactive = false
    var deleteEdgeMode = false
    var onMoveStart: ((NSPoint) -> Void)?
    var onMove: ((NSPoint) -> Void)?
    var onResizeStart: ((NSPoint) -> Void)?
    var onResize: ((NSPoint) -> Void)?
    var onUndoLastAction: (() -> Void)?

    private var isDraggingHeader = false
    private var isResizing = false
    private var isPanningGraph = false
    private var panStartPoint: NSPoint?
    private var panStartOffset = NSPoint(x: 0, y: 0)
    private var selectedLocationId: Int?
    private var lastNodeRects: [(id: Int, rect: NSRect)] = []
    private var lastEdgeHitTargets: [EdgeHitTarget] = []
    private var mapPan = NSPoint(x: 0, y: 0)
    private var mapZoom: CGFloat = 1.0

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        wantsLayer = true
        layer?.backgroundColor = NSColor.clear.cgColor
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    override var isOpaque: Bool { false }

    private func formatDuration(_ seconds: Int?) -> String {
        guard let seconds else { return "unknown ttl" }

        let h = seconds / 3600
        let m = (seconds % 3600) / 60
        let s = seconds % 60

        if h > 0 {
            return "\(h)h \(m)m"
        }

        if m > 0 {
            return "\(m)m \(s)s"
        }

        return "\(s)s"
    }

    private func drawRouteControls(in rect: NSRect) {
        let y = rect.minY + 30

        drawInputBox(
            rect: routeFromRect(),
            text: routeFromText.isEmpty ? "from=current" : routeFromText,
            active: activeRouteField == .from
        )

        drawInputBox(
            rect: routeToRect(),
            text: routeToText.isEmpty ? "to" : routeToText,
            active: activeRouteField == .to
        )

        NSColor.systemGreen.withAlphaComponent(0.35).setFill()
        NSBezierPath(roundedRect: findButtonRect(), xRadius: 5, yRadius: 5).fill()
        "Find".draw(
            at: NSPoint(x: findButtonRect().minX + 10, y: findButtonRect().minY + 5),
            withAttributes: tinyAttrs
        )
        NSColor.systemBlue.withAlphaComponent(0.30).setFill()
        NSBezierPath(roundedRect: copyRouteButtonRect(), xRadius: 5, yRadius: 5).fill()
        "Copy".draw(
            at: NSPoint(x: copyRouteButtonRect().minX + 10, y: copyRouteButtonRect().minY + 5),
            withAttributes: tinyAttrs
        )
        NSColor.systemRed.withAlphaComponent(0.35).setFill()
        NSBezierPath(roundedRect: delRoutesButtonRect(), xRadius: 5, yRadius: 5).fill()
        "DelRout".draw(
            at: NSPoint(x: delRoutesButtonRect().minX + 8, y: delRoutesButtonRect().minY + 5),
            withAttributes: tinyAttrs
        )
        let countText = "\(data.route_edges_count ?? data.route_edges.count)"
        countText.draw(
            at: NSPoint(x: routeCountRect().minX + 10, y: routeCountRect().minY + 4),
            withAttributes: smallAttrs
        )
    }

    private func drawInputBox(rect: NSRect, text: String, active: Bool) {
        NSColor(calibratedWhite: 0.0, alpha: 0.35).setFill()
        NSBezierPath(roundedRect: rect, xRadius: 5, yRadius: 5).fill()

        (active ? NSColor.systemYellow : NSColor.white.withAlphaComponent(0.25)).setStroke()
        let border = NSBezierPath(roundedRect: rect, xRadius: 5, yRadius: 5)
        border.lineWidth = active ? 1.5 : 1.0
        border.stroke()

        clipped(
            text,
            at: NSPoint(x: rect.minX + 7, y: rect.minY + 5),
            maxWidth: rect.width - 14,
            attrs: tinyAttrs
        )
    }

    override func keyDown(with event: NSEvent) {
        guard interactive else {
            super.keyDown(with: event)
            return
        }

        guard let activeRouteField else {
            super.keyDown(with: event)
            return
        }

        if event.keyCode == 53 {
            self.activeRouteField = nil
            needsDisplay = true
            return
        }

        if event.keyCode == 36 {
            self.activeRouteField = nil
            onFindRoute?(routeFromText, routeToText)
            needsDisplay = true
            return
        }

        if event.keyCode == 51 {
            switch activeRouteField {
            case .from:
                if !routeFromText.isEmpty {
                    routeFromText.removeLast()
                }
            case .to:
                if !routeToText.isEmpty {
                    routeToText.removeLast()
                }
            }

            needsDisplay = true
            return
        }

        if let chars = event.charactersIgnoringModifiers {
            for ch in chars {
                if ch.isNewline {
                    continue
                }

                if ch.unicodeScalars.allSatisfy({ CharacterSet.controlCharacters.contains($0) }) {
                    continue
                }

                switch activeRouteField {
                case .from:
                    routeFromText.append(ch)
                case .to:
                    routeToText.append(ch)
                }
            }

            needsDisplay = true
            return
        }

        super.keyDown(with: event)
    }

    override func draw(_ dirtyRect: NSRect) {
        NSColor.clear.setFill()
        dirtyRect.fill()

        let panelRect = bounds.insetBy(dx: 0, dy: 0)
        let path = NSBezierPath(roundedRect: panelRect, xRadius: 8, yRadius: 8)
        NSColor(calibratedWhite: 0.06, alpha: 0.78).setFill()
        path.fill()
        NSColor(calibratedWhite: 1.0, alpha: interactive ? 0.28 : 0.14).setStroke()
        path.lineWidth = 1
        path.stroke()

        drawSelectedLocationTitle(in: panelRect)
        drawHeader(in: panelRect)
        drawGraphPreview(in: graphRect(in: panelRect))
        drawAvalonInfoPopup()
        drawRouteControls(in: panelRect)
        drawLastPortal(in: panelRect)
        if interactive {
            drawResizeHandle(in: panelRect)
        }
    }

   override func rightMouseDown(with event: NSEvent) {
       mouseDown(with: event)
   }
   override func mouseDown(with event: NSEvent) {
        guard interactive else { return }

        let point = convert(event.locationInWindow, from: nil)
        if shortcutMinusRect().contains(point) {
            shortcutDepth = max(1, shortcutDepth - 1)
            onShortcutDepthChanged?(shortcutDepth)
            needsDisplay = true
            return
        }

        if shortcutPlusRect().contains(point) {
            shortcutDepth = min(6, shortcutDepth + 1)
            onShortcutDepthChanged?(shortcutDepth)
            needsDisplay = true
            return
        }
        if event.type == .rightMouseDown {
            if let hit = lastNodeRects.reversed().first(where: { $0.rect.contains(point) }),
               let location = data.locations.first(where: { $0.id == hit.id }),
               location.zone_type == "avalon" {
                avalonInfoLocationId = location.id
                avalonInfoPoint = point
                selectedLocationId = location.id
                needsDisplay = true
            } else {
                avalonInfoLocationId = nil
                avalonInfoPoint = nil
                needsDisplay = true
            }
            return
        }

        window?.makeFirstResponder(self)

        if routeFromRect().contains(point) {
            activeRouteField = .from
            window?.makeFirstResponder(self)
            needsDisplay = true
            return
        }

        if routeToRect().contains(point) {
            activeRouteField = .to
            window?.makeFirstResponder(self)
            needsDisplay = true
            return
        }
        if copyRouteButtonRect().contains(point) {
            copySelectedRouteToClipboard()
            needsDisplay = true
            return
        }
        if findButtonRect().contains(point) {
            activeRouteField = nil

            let finalTo = routeToText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                ? (selectedLocationName() ?? "")
                : routeToText

            onFindRoute?(routeFromText, finalTo)

            needsDisplay = true
            return
        }

        if delRoutesButtonRect().contains(point) {
            activeRouteField = nil
            onClearRoute?()
            needsDisplay = true
            return
        }
        if modeButtonRect().contains(point) {
            deleteEdgeMode.toggle()
            needsDisplay = true
            return
        }
        if deleteEdgeMode {
            if let hit = lastEdgeHitTargets.reversed().first(where: { $0.contains(point, threshold: 8.5) }) {
                onDeleteEdge?(hit.id)
                needsDisplay = true
            }
            return
        }
        if graphRect(in: bounds).contains(point) {
            if let hit = lastNodeRects.reversed().first(where: { $0.rect.contains(point) }) {
                selectedLocationId = hit.id
            }

            isPanningGraph = true
            panStartPoint = point
            panStartOffset = mapPan
            needsDisplay = true
            return
        }
        if let hit = lastNodeRects.reversed().first(where: { $0.rect.contains(point) }) {
            selectedLocationId = hit.id
            needsDisplay = true
            // без return: даём дальше обработать drag/resize/header
        }


        if undoButtonRect().contains(point) {
            onUndoLastAction?()
            return
        }

        if resizeHandleRect().contains(point) {
            isResizing = true
            if let screenPoint = screenPoint(from: event) {
                onResizeStart?(screenPoint)
            }
            return
        }

        if headerRect().contains(point) {
            isDraggingHeader = true
            if let screenPoint = screenPoint(from: event) {
                onMoveStart?(screenPoint)
            }
        }
    }
    override func scrollWheel(with event: NSEvent) {
        guard interactive else { return }

        let point = convert(event.locationInWindow, from: nil)
        guard graphRect(in: bounds).contains(point) else {
            super.scrollWheel(with: event)
            return
        }

        let oldZoom = mapZoom
        let zoomFactor = 1.0 + (-event.scrollingDeltaY * 0.0015)
        mapZoom = (mapZoom * zoomFactor).clamped(to: 0.25...4.0)

        let ratio = mapZoom / oldZoom

        mapPan = NSPoint(
            x: point.x - (point.x - mapPan.x) * ratio,
            y: point.y - (point.y - mapPan.y) * ratio
        )

        needsDisplay = true
   }
   override func mouseDragged(with event: NSEvent) {
        guard interactive else { return }

        let point = convert(event.locationInWindow, from: nil)

        if isPanningGraph, let panStartPoint {
            mapPan = NSPoint(
                x: panStartOffset.x + point.x - panStartPoint.x,
                y: panStartOffset.y + point.y - panStartPoint.y
            )
            needsDisplay = true
            return
        }

        guard let screenPoint = screenPoint(from: event) else { return }

        if isDraggingHeader {
            onMove?(screenPoint)
            return
        }

        if isResizing {
            onResize?(screenPoint)
        }
   }
   override func mouseUp(with event: NSEvent) {
        isDraggingHeader = false
        isResizing = false
        isPanningGraph = false
        panStartPoint = nil
   }

  private func drawHeader(in rect: NSRect) {
        NSColor.systemOrange.setFill()
        NSBezierPath(roundedRect: undoButtonRect(), xRadius: 5, yRadius: 5).fill()
        "Undo".draw(
            at: NSPoint(x: undoButtonRect().minX + 10, y: undoButtonRect().minY + 5),
            withAttributes: tinyAttrs
        )

        if interactive {
            (deleteEdgeMode ? NSColor.systemRed.withAlphaComponent(0.42) : NSColor.systemTeal.withAlphaComponent(0.35)).setFill()
            NSBezierPath(roundedRect: modeButtonRect(), xRadius: 5, yRadius: 5).fill()
            "Del".draw(
                at: NSPoint(x: modeButtonRect().minX + 14, y: modeButtonRect().minY + 5),
                withAttributes: tinyAttrs
            )
        }
    }

    private func drawSelectedLocationTitle(in rect: NSRect) {
        let selectedName: String? = selectedLocationId.flatMap { selectedId in
            if let loc = data.locations.first(where: { $0.id == selectedId }) {
                return loc.name
            }

            if let loc = data.route_locations.first(where: { $0.id == selectedId }) {
                return loc.name
            }

            if let loc = data.bridge_locations.first(where: { $0.id == selectedId }) {
                return loc.name
            }

            return nil
        }

        let name = selectedName ?? data.current_location ?? "No location selected"

        clipped(
            name,
            at: NSPoint(x: rect.minX + 14, y: rect.maxY - 34),
            maxWidth: rect.width - 150,
            attrs: valueAttrs
        )

        drawShortcutDepthControl()
    }

    private func drawLastPortal(in rect: NSRect) {
        let text: String

        if let portal = data.last_portal_destination {
            text = "Last portal: \(portal) / ttl \(formatDuration(data.last_portal_expires_in_seconds))"
        } else {
            text = "Last portal: none"
        }

        clipped(
            text,
            at: NSPoint(x: rect.minX + 14, y: rect.minY + 8),
            maxWidth: rect.width - 28,
            attrs: smallAttrs
        )
    }
    private func statusColor(_ edge: MapEdge) -> NSColor {
        if edge.source == "traversed" {
            return NSColor(calibratedRed: 1.0, green: 0.95, blue: 0.75, alpha: 1.0)
        }

        return NSColor.systemTeal
    }
   private func drawGraphPreview(in rect: NSRect) {

        NSColor(calibratedWhite: 1.0, alpha: 0.06).setFill()
        NSBezierPath(roundedRect: rect, xRadius: 6, yRadius: 6).fill()
        lastNodeRects.removeAll()
        lastEdgeHitTargets.removeAll()
        let locations = data.locations.sorted { $0.id < $1.id }
        if locations.isEmpty {
            "No graph data".draw(at: NSPoint(x: rect.midX - 44, y: rect.midY - 7), withAttributes: smallAttrs)
            return
        }

        let locationById = Dictionary(uniqueKeysWithValues: locations.map { ($0.id, $0) })

        var rawPositions: [Int: NSPoint] = [:]

        for (index, location) in locations.enumerated() {
            if let x = location.x, let y = location.y {
                rawPositions[location.id] = NSPoint(x: x, y: y)
            } else {
                let columns = max(1, Int(ceil(sqrt(Double(locations.count)))))
                let col = index % columns
                let row = index / columns
                rawPositions[location.id] = NSPoint(x: CGFloat(col) * 140, y: CGFloat(row) * 140)
            }
        }

        for routeLocation in data.route_locations {
            if rawPositions[routeLocation.id] != nil {
                continue
            }

            if let x = routeLocation.x, let y = routeLocation.y {
                rawPositions[routeLocation.id] = NSPoint(x: x, y: y + 220)
            }
        }
        for bridgeLocation in data.bridge_locations {
            if rawPositions[bridgeLocation.id] != nil {
                continue
            }

            if let x = bridgeLocation.x, let y = bridgeLocation.y {
                rawPositions[bridgeLocation.id] = NSPoint(x: x, y: y)
            }
        }
        let xs = rawPositions.values.map(\.x)
        let ys = rawPositions.values.map(\.y)

        guard let minX = xs.min(),
              let maxX = xs.max(),
              let minY = ys.min(),
              let maxY = ys.max()
        else {
            return
        }

        let padding: CGFloat = 18
        let graphWidth = max(maxX - minX, 1)
        let graphHeight = max(maxY - minY, 1)

        let baseScale = min(
            (rect.width - padding * 2) / graphWidth,
            (rect.height - padding * 2) / graphHeight
        )

        let scale = baseScale * mapZoom

        let contentWidth = graphWidth * scale
        let contentHeight = graphHeight * scale

        let offsetX = rect.midX - contentWidth / 2
        let offsetY = rect.midY - contentHeight / 2

        var positions: [Int: NSPoint] = [:]

        for (id, point) in rawPositions {
            positions[id] = NSPoint(
                x: offsetX + (point.x - minX) * scale + mapPan.x,
                y: offsetY + (point.y - minY) * scale + mapPan.y
            )
        }

        for edge in data.edges {
            guard let from = positions[edge.from_location_id],
                  let to = positions[edge.to_location_id]
            else {
                continue
            }

            lastEdgeHitTargets.append(EdgeHitTarget(id: edge.id, from: from, to: to))

            let path = NSBezierPath()
            path.move(to: from)
            path.line(to: to)

            statusColor(edge).withAlphaComponent(edge.source == "traversed" ? 0.95 : 0.65).setStroke()
            path.lineWidth = edge.source == "traversed" ? 2.4 : 1.4
            path.stroke()
        }
        for edge in data.bridge_edges {
            guard let from = positions[edge.from_location_id],
                  let to = positions[edge.to_location_id]
            else {
                continue
            }

            let path = NSBezierPath()
            path.move(to: from)
            path.line(to: to)

            NSColor.white.withAlphaComponent(0.28).setStroke()
            path.lineWidth = 1.2
            path.stroke()
        }
        for edge in data.route_edges {
            guard let from = positions[edge.from_location_id],
                  let to = positions[edge.to_location_id]
            else {
                continue
            }

            let path = NSBezierPath()
            path.move(to: from)
            path.line(to: to)

            NSColor.systemOrange.withAlphaComponent(0.95).setStroke()
            path.lineWidth = 3.0
            path.stroke()
        }

        let currentId = data.locations.first { $0.name == data.current_location }?.id

        for location in locations {
            guard let point = positions[location.id] else { continue }

            let isCurrent = location.id == currentId
            let isSelected = selectedLocationId == location.id
            let radius: CGFloat = isSelected ? 8.0 : (isCurrent ? 6.5 : 5.5)

            let nodeRect = NSRect(
                x: point.x - radius,
                y: point.y - radius,
                width: radius * 2,
                height: radius * 2
            )

            lastNodeRects.append((id: location.id, rect: nodeRect.insetBy(dx: -8, dy: -8)))

            colorForZoneType(location.zone_type, isCurrent: isCurrent).setFill()
            NSBezierPath(ovalIn: nodeRect).fill()

            NSColor.white.withAlphaComponent(isCurrent || isSelected ? 0.95 : 0.45).setStroke()
            let border = NSBezierPath(ovalIn: nodeRect.insetBy(dx: -1.5, dy: -1.5))
            border.lineWidth = isSelected ? 3.0 : (isCurrent ? 2.0 : 1.0)
            border.stroke()
            drawAvalonTierBorder(location: location, nodeRect: nodeRect, isCurrent: isCurrent)
        }
        for location in data.bridge_locations {
            guard let point = positions[location.id] else { continue }

            let radius: CGFloat = 4.5
            let nodeRect = NSRect(
                x: point.x - radius,
                y: point.y - radius,
                width: radius * 2,
                height: radius * 2
            )

            lastNodeRects.append((id: location.id, rect: nodeRect.insetBy(dx: -8, dy: -8)))

            colorForZoneType(location.zone_type, isCurrent: false).withAlphaComponent(0.72).setFill()
            NSBezierPath(ovalIn: nodeRect).fill()

            NSColor.white.withAlphaComponent(0.42).setStroke()
            let border = NSBezierPath(ovalIn: nodeRect.insetBy(dx: -1.2, dy: -1.2))
            border.lineWidth = 1.2
            border.stroke()
        }
        for location in data.route_locations {
            guard let point = positions[location.id] else { continue }

            let alreadyExists = data.locations.contains { $0.id == location.id }

            let radius: CGFloat = alreadyExists ? 9.0 : 7.0
            let nodeRect = NSRect(
                x: point.x - radius,
                y: point.y - radius,
                width: radius * 2,
                height: radius * 2
            )

            lastNodeRects.append((id: location.id, rect: nodeRect.insetBy(dx: -8, dy: -8)))

            if !alreadyExists {
                colorForZoneType(location.zone_type, isCurrent: false).setFill()
                NSBezierPath(ovalIn: nodeRect).fill()
            }

            NSColor.systemOrange.setStroke()
            let border = NSBezierPath(ovalIn: nodeRect.insetBy(dx: -2.0, dy: -2.0))
            border.lineWidth = alreadyExists ? 3.5 : 2.5
            border.stroke()
        }
   }




    private func drawResizeHandle(in rect: NSRect) {
        let handle = resizeHandleRect()
        NSColor(calibratedWhite: 1.0, alpha: 0.34).setStroke()
        let path = NSBezierPath()
        path.move(to: NSPoint(x: handle.minX + 4, y: handle.minY + 4))
        path.line(to: NSPoint(x: handle.maxX - 4, y: handle.minY + 4))
        path.move(to: NSPoint(x: handle.maxX - 4, y: handle.minY + 4))
        path.line(to: NSPoint(x: handle.maxX - 4, y: handle.maxY - 4))
        path.stroke()
    }

    private func clipped(_ text: String, at point: NSPoint, maxWidth: CGFloat, attrs: [NSAttributedString.Key: Any]) {
        let paragraph = NSMutableParagraphStyle()
        paragraph.lineBreakMode = .byTruncatingTail
        var attributes = attrs
        attributes[.paragraphStyle] = paragraph
        text.draw(in: NSRect(x: point.x, y: point.y, width: maxWidth, height: 18), withAttributes: attributes)
    }

    private func copyRouteButtonRect() -> NSRect {
        NSRect(x: findButtonRect().minX, y: findButtonRect().minY - 30, width: findButtonRect().width, height: 24)
    }

    private func routeCountRect() -> NSRect {
        NSRect(x: delRoutesButtonRect().minX, y: delRoutesButtonRect().minY - 30, width: delRoutesButtonRect().width, height: 24)
    }
    private func headerRect() -> NSRect {
        NSRect(x: 0, y: bounds.height - 44, width: bounds.width, height: 44)
    }

    private func shortcutMinusRect() -> NSRect {
        NSRect(x: bounds.width - 216, y: bounds.height - 36, width: 26, height: 24)
    }

    private func shortcutValueRect() -> NSRect {
        NSRect(x: bounds.width - 186, y: bounds.height - 36, width: 34, height: 24)
    }

    private func shortcutPlusRect() -> NSRect {
        NSRect(x: bounds.width - 148, y: bounds.height - 36, width: 26, height: 24)
    }

    private func drawShortcutDepthControl() {
        NSColor(calibratedWhite: 0.0, alpha: 0.35).setFill()
        NSBezierPath(roundedRect: shortcutMinusRect(), xRadius: 5, yRadius: 5).fill()
        NSBezierPath(roundedRect: shortcutValueRect(), xRadius: 5, yRadius: 5).fill()
        NSBezierPath(roundedRect: shortcutPlusRect(), xRadius: 5, yRadius: 5).fill()

        NSColor.white.withAlphaComponent(0.25).setStroke()
        NSBezierPath(roundedRect: shortcutMinusRect(), xRadius: 5, yRadius: 5).stroke()
        NSBezierPath(roundedRect: shortcutValueRect(), xRadius: 5, yRadius: 5).stroke()
        NSBezierPath(roundedRect: shortcutPlusRect(), xRadius: 5, yRadius: 5).stroke()

        "-".draw(
            at: NSPoint(x: shortcutMinusRect().minX + 9, y: shortcutMinusRect().minY + 4),
            withAttributes: smallAttrs
        )

        "\(shortcutDepth)".draw(
            at: NSPoint(x: shortcutValueRect().minX + 12, y: shortcutValueRect().minY + 4),
            withAttributes: smallAttrs
        )

        "+".draw(
            at: NSPoint(x: shortcutPlusRect().minX + 8, y: shortcutPlusRect().minY + 4),
            withAttributes: smallAttrs
        )
    }

    private func undoButtonRect() -> NSRect {
        NSRect(x: bounds.width - 62, y: bounds.height - 37, width: 48, height: 24)
    }

    private func modeButtonRect() -> NSRect {
        NSRect(x: bounds.width - 116, y: bounds.height - 37, width: 48, height: 24)
    }
    private func routeFromRect() -> NSRect {
        NSRect(x: 14, y: 30, width: 105, height: 24)
    }

    private func routeToRect() -> NSRect {
        NSRect(x: 124, y: 30, width: 105, height: 24)
    }

    private func findButtonRect() -> NSRect {
        NSRect(x: 234, y: 30, width: 48, height: 24)
    }

    private func delRoutesButtonRect() -> NSRect {
        NSRect(x: 288, y: 30, width: 48, height: 24)
    }
    private func resizeHandleRect() -> NSRect {
        NSRect(x: bounds.width - 24, y: 0, width: 24, height: 24)
    }

    private func screenPoint(from event: NSEvent) -> NSPoint? {
        window?.convertPoint(toScreen: event.locationInWindow)
    }

    private func graphRect(in rect: NSRect) -> NSRect {
        NSRect(
            x: rect.minX + 14,
            y: rect.minY + 64,
            width: rect.width - 28,
            height: rect.height - 118
        )
    }

    private func neighborsRect(in rect: NSRect) -> NSRect {
        let graph = graphRect(in: rect)
        return NSRect(
            x: graph.maxX + 18,
            y: graph.minY,
            width: rect.maxX - graph.maxX - 32,
            height: graph.height
        )
    }


    private var titleAttrs: [NSAttributedString.Key: Any] {
        [.font: NSFont.systemFont(ofSize: 15, weight: .semibold), .foregroundColor: NSColor.white]
    }

    private var valueAttrs: [NSAttributedString.Key: Any] {
        [.font: NSFont.systemFont(ofSize: 18, weight: .semibold), .foregroundColor: NSColor.white]
    }

    private var labelAttrs: [NSAttributedString.Key: Any] {
        [.font: NSFont.systemFont(ofSize: 10, weight: .bold), .foregroundColor: NSColor(calibratedWhite: 1.0, alpha: 0.62)]
    }

    private var smallAttrs: [NSAttributedString.Key: Any] {
        [.font: NSFont.systemFont(ofSize: 11, weight: .regular), .foregroundColor: NSColor(calibratedWhite: 1.0, alpha: 0.78)]
    }

    private var tinyAttrs: [NSAttributedString.Key: Any] {
        [.font: NSFont.monospacedSystemFont(ofSize: 9, weight: .regular), .foregroundColor: NSColor(calibratedWhite: 1.0, alpha: 0.58)]
    }
}

func clamp(_ value: CGFloat, min minValue: CGFloat, max maxValue: CGFloat) -> CGFloat {
    min(max(value, minValue), maxValue)
}

func anchoredPortalCaptureRect(selected: CGRect, anchor: CGPoint, cursor: CGPoint) -> CGRect {
    let offsetX = selected.minX - anchor.x
    let offsetY = selected.minY - anchor.y

    return CGRect(
        x: cursor.x + offsetX,
        y: cursor.y + offsetY,
        width: selected.width,
        height: selected.height
    ).integral
}

func runCaptureRequest(
    kind: String,
    outputDir: String,
    width: CGFloat,
    height: CGFloat,
    centerCursor: Bool,
    portalX: CGFloat?,
    portalY: CGFloat?,
    portalWidth: CGFloat?,
    portalHeight: CGFloat?,
    portalAnchorX: Double?,
    portalAnchorY: Double?,
    x: CGFloat,
    y: CGFloat
) -> CaptureOcrOutput? {
    let started = Date()

    let rect: CGRect
    let rectStarted = Date()

    if let portalX, let portalY, let portalWidth, let portalHeight, let portalAnchorX, let portalAnchorY {
        let mouse = NSEvent.mouseLocation
        let cursor = topLeftCursorPoint(mouse)
        let selected = CGRect(x: portalX, y: portalY, width: portalWidth, height: portalHeight)

        rect = anchoredPortalCaptureRect(
            selected: selected,
            anchor: CGPoint(x: portalAnchorX, y: portalAnchorY),
            cursor: cursor
        )
    } else if centerCursor {
        let mouse = NSEvent.mouseLocation
        let screen = screenContaining(point: mouse) ?? NSScreen.main ?? NSScreen.screens.first
        let screenFrame = screen?.frame ?? NSRect(x: 0, y: 0, width: 1440, height: 900)

        rect = CGRect(
            x: mouse.x - width / 2,
            y: screenFrame.maxY - mouse.y - height / 2,
            width: width,
            height: height
        )
    } else {
        rect = CGRect(x: x, y: y, width: width, height: height)
    }

    fputs("[capture-helper-timing] kind=\(kind) phase=rect ms=\(Int(Date().timeIntervalSince(rectStarted) * 1000)) rect=\(rect)\n", stderr)
    fflush(stderr)

    let permissionStarted = Date()
    let permission = CGPreflightScreenCaptureAccess()

    fputs("[capture-helper-timing] kind=\(kind) phase=permission ms=\(Int(Date().timeIntervalSince(permissionStarted) * 1000)) allowed=\(permission)\n", stderr)
    fflush(stderr)

    let captureStarted = Date()

    guard let image = CGWindowListCreateImage(
        rect,
        .optionOnScreenOnly,
        kCGNullWindowID,
        [.bestResolution, .nominalResolution]
    ) else {
        fputs("Could not capture screen region. Grant Screen Recording permission and use Windowed/Borderless mode.\n", stderr)
        fflush(stderr)
        return nil
    }

    fputs("[capture-helper-timing] kind=\(kind) phase=screen_capture ms=\(Int(Date().timeIntervalSince(captureStarted) * 1000)) size=\(image.width)x\(image.height)\n", stderr)
    fflush(stderr)

    let saveStarted = Date()
    let imagePath = saveCaptureImage(image: image, outputDir: outputDir, kind: kind)

    fputs("[capture-helper-timing] kind=\(kind) phase=save_png ms=\(Int(Date().timeIntervalSince(saveStarted) * 1000)) total_ms=\(Int(Date().timeIntervalSince(started) * 1000)) path=\(imagePath)\n", stderr)
    fflush(stderr)

    return CaptureOcrOutput(
        text: "",
        confidence: nil,
        engine: "capture_only",
        image_path: imagePath,
        width: image.width,
        height: image.height,
        duration_ms: Int(Date().timeIntervalSince(started) * 1000),
        screen_recording_permission: permission,
        lines: []
    )
}
func runCaptureOcrMode(args: [String]) {
    let kind = argValue(args, "--kind") ?? "capture"
    let outputDir = argValue(args, "--output-dir") ?? NSTemporaryDirectory()
    let width = CGFloat(Int(argValue(args, "--width") ?? "320") ?? 320)
    let height = CGFloat(Int(argValue(args, "--height") ?? "180") ?? 180)
    let centerCursor = args.contains("--center-cursor")

    let portalX = argValue(args, "--portal-x").flatMap { CGFloat(Int($0) ?? 0) }
    let portalY = argValue(args, "--portal-y").flatMap { CGFloat(Int($0) ?? 0) }
    let portalWidth = argValue(args, "--portal-width").flatMap { CGFloat(Int($0) ?? 0) }
    let portalHeight = argValue(args, "--portal-height").flatMap { CGFloat(Int($0) ?? 0) }
    let portalAnchorX = argValue(args, "--portal-anchor-x").flatMap(Double.init)
    let portalAnchorY = argValue(args, "--portal-anchor-y").flatMap(Double.init)

    let x = CGFloat(Int(argValue(args, "--x") ?? "0") ?? 0)
    let y = CGFloat(Int(argValue(args, "--y") ?? "0") ?? 0)

    guard let output = runCaptureRequest(
        kind: kind,
        outputDir: outputDir,
        width: width,
        height: height,
        centerCursor: centerCursor,
        portalX: portalX,
        portalY: portalY,
        portalWidth: portalWidth,
        portalHeight: portalHeight,
        portalAnchorX: portalAnchorX,
        portalAnchorY: portalAnchorY,
        x: x,
        y: y
    ) else {
        exit(2)
    }

    if let data = try? JSONEncoder().encode(output),
       let json = String(data: data, encoding: .utf8) {
        print(json)
        fflush(stdout)
    }
}

func runCaptureServerMode() {
    let decoder = JSONDecoder()
    let encoder = JSONEncoder()

    fputs("[capture-server] ready\n", stderr)
    fflush(stderr)

    while let line = readLine() {
        guard let data = line.data(using: .utf8) else {
            continue
        }

        do {
            let request = try decoder.decode(CaptureServerRequest.self, from: data)

            guard request.cmd == "capture" else {
                continue
            }

            guard let output = runCaptureRequest(
                kind: request.kind,
                outputDir: request.output_dir,
                width: CGFloat(request.width),
                height: CGFloat(request.height),
                centerCursor: request.center_cursor ?? false,
                portalX: request.portal_x.map { CGFloat($0) },
                portalY: request.portal_y.map { CGFloat($0) },
                portalWidth: request.portal_width.map { CGFloat($0) },
                portalHeight: request.portal_height.map { CGFloat($0) },
                portalAnchorX: request.portal_anchor_x,
                portalAnchorY: request.portal_anchor_y,
                x: CGFloat(request.x),
                y: CGFloat(request.y)
            ) else {
                let errorOutput = [
                    "ok": false,
                    "error": "capture failed"
                ] as [String : Any]

                if let data = try? JSONSerialization.data(withJSONObject: errorOutput),
                   let json = String(data: data, encoding: .utf8) {
                    print(json)
                    fflush(stdout)
                }

                continue
            }

            let data = try encoder.encode(output)
            if let json = String(data: data, encoding: .utf8) {
                print(json)
                fflush(stdout)
            }
        } catch {
            fputs("[capture-server] error=\(error)\n", stderr)
            fflush(stderr)
        }
    }
}

func topLeftCursorPoint(_ mouse: NSPoint) -> CGPoint {
    let screen = screenContaining(point: mouse) ?? NSScreen.main ?? NSScreen.screens.first
    let screenFrame = screen?.frame ?? NSRect(x: 0, y: 0, width: 1440, height: 900)
    return CGPoint(x: mouse.x, y: screenFrame.maxY - mouse.y)
}

func mirroredPortalCaptureRect(selected: CGRect, anchor: CGPoint, cursor: CGPoint) -> CGRect {
    let offsetX = selected.minX - anchor.x
    let offsetY = selected.minY - anchor.y
    let sameSide = CGRect(x: cursor.x + offsetX, y: cursor.y + offsetY, width: selected.width, height: selected.height)
    let mirrored = CGRect(
        x: cursor.x - (selected.maxX - anchor.x),
        y: cursor.y + offsetY,
        width: selected.width,
        height: selected.height
    )
    return sameSide.union(mirrored).integral
}

func bestPortalLine(from lines: [String]) -> String {
    let ignored = ["portal", "avalonian", "roads", "enter", "exit"]
    let scored = lines.map { line -> (String, Int) in
        let lower = line.lowercased()
        let penalty = ignored.reduce(0) { score, word in score + (lower.contains(word) ? 2 : 0) }
        let letters = lower.filter { $0.isLetter }.count
        return (line, letters - penalty)
    }
    return scored.max { $0.1 < $1.1 }?.0 ?? lines.joined(separator: "\n")
}

func saveCaptureImage(image: CGImage, outputDir: String, kind: String) -> String {
    try? FileManager.default.createDirectory(atPath: outputDir, withIntermediateDirectories: true)
    let filename = "\(kind)-\(Int(Date().timeIntervalSince1970 * 1000)).png"
    let url = URL(fileURLWithPath: outputDir).appendingPathComponent(filename)
    let rep = NSBitmapImageRep(cgImage: image)
    if let png = rep.representation(using: .png, properties: [:]) {
        try? png.write(to: url, options: .atomic)
    }
    return url.path
}

func screenContaining(point: NSPoint) -> NSScreen? {
    NSScreen.screens.first { screen in screen.frame.contains(point) }
}

func argValue(_ args: [String], _ name: String) -> String? {
    guard let index = args.firstIndex(of: name), args.indices.contains(index + 1) else {
        return nil
    }
    return args[index + 1]
}

func mapOverlayHotKeyHandler(
    nextHandler: EventHandlerCallRef?,
    event: EventRef?,
    userData: UnsafeMutableRawPointer?
) -> OSStatus {
    guard let event, let userData else { return noErr }
    var hotKeyID = EventHotKeyID()
    let status = GetEventParameter(
        event,
        EventParamName(kEventParamDirectObject),
        EventParamType(typeEventHotKeyID),
        nil,
        MemoryLayout<EventHotKeyID>.size,
        nil,
        &hotKeyID
    )
    if status == noErr && hotKeyID.signature == OSType(0x414D4F4D) {
        let controller = Unmanaged<MapOverlayController>.fromOpaque(userData).takeUnretainedValue()
        switch hotKeyID.id {
        case 1:
            controller.toggleFromHotkey()
        case 2:
            controller.captureCurrentFromHotkey()
        case 3:
            controller.capturePortalFromHotkey()
        default:
            break
        }
    }
    return noErr
}
extension Comparable {
    func clamped(to limits: ClosedRange<Self>) -> Self {
        min(max(self, limits.lowerBound), limits.upperBound)
    }
}
let args = CommandLine.arguments
let modeIndex = args.firstIndex(of: "--mode")
let mode = modeIndex.flatMap { index in
    args.indices.contains(index + 1) ? args[index + 1] : nil
} ?? "region"

let app = NSApplication.shared
if mode == "capture-ocr" {
    runCaptureOcrMode(args: args)
} else if mode == "capture-server" {
    runCaptureServerMode()
} else if mode == "map-overlay" {
    let boundsIndex = args.firstIndex(of: "--bounds-state")
    let boundsStatePath = boundsIndex.flatMap { index in
        args.indices.contains(index + 1) ? args[index + 1] : nil
    }
    let delegate = MapOverlayController(boundsStatePath: boundsStatePath)
    app.delegate = delegate
    app.run()
} else {
    let delegate = SelectionOverlayController(mode: mode)
    app.delegate = delegate
    app.run()
}
