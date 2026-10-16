import AppKit
import SwiftUI

/// Dev helper: `LSTACK_SNAPSHOT=/tmp/popover.png LStack` renders the popover (and a log window
/// if LSTACK_SNAPSHOT_LOGS=<service> is set) to PNG files and exits. Used for docs/screenshots.
@MainActor
enum SnapshotRenderer {
    static func runIfRequested(store: AppStore) {
        guard let path = ProcessInfo.processInfo.environment["LSTACK_SNAPSHOT"] else { return }
        let expand = ProcessInfo.processInfo.environment["LSTACK_SNAPSHOT_EXPAND"]
        let logs = ProcessInfo.processInfo.environment["LSTACK_SNAPSHOT_LOGS"]
        Task {
            try? await Task.sleep(for: .seconds(4))
            if let expand, let s = store.service(id: expand) { store.expanded = expand; await store.loadHistory(s) }
            try? await Task.sleep(for: .seconds(1))
            render(PopoverView().environment(store), size: nil, to: path)
            if ProcessInfo.processInfo.environment["LSTACK_SNAPSHOT_SETUP"] != nil {
                await store.refreshSetup()
                render(SetupView().environment(store), size: nil, to: path.replacingOccurrences(of: ".png", with: "-setup.png"), delay: 1.5)
            }
            if let cmd = ProcessInfo.processInfo.environment["LSTACK_SNAPSHOT_ADD"] {
                render(ServiceEditorView(command: cmd).environment(store), size: nil, to: path.replacingOccurrences(of: ".png", with: "-add.png"), delay: 2)
            }
            if let svc = ProcessInfo.processInfo.environment["LSTACK_SNAPSHOT_EDIT"] {
                render(ServiceEditorView(editing: svc, tab: .environment).environment(store), size: NSSize(width: 560, height: 820), to: path.replacingOccurrences(of: ".png", with: "-edit.png"), delay: 3)
                try? await Task.sleep(for: .seconds(2))
            }
            if ProcessInfo.processInfo.environment["LSTACK_SNAPSHOT_MACHINES"] != nil {
                render(ScrollView { MachinesSettings().padding(20) }.environment(store), size: NSSize(width: 680, height: 1100), to: path.replacingOccurrences(of: ".png", with: "-machines.png"), delay: 4)
                try? await Task.sleep(for: .seconds(3))
            }
            if let logs {
                render(LogView(name: logs).environment(store), size: NSSize(width: 860, height: 420), to: path.replacingOccurrences(of: ".png", with: "-logs.png"), delay: 3)
            }
            try? await Task.sleep(for: .seconds(logs == nil ? 3 : 5))
            NSApp.terminate(nil)
        }
    }

    private static func render<V: View>(_ view: V, size: NSSize?, to path: String, delay: Double = 0.5) {
        let host = NSHostingView(rootView: view.frame(width: size?.width, height: size?.height))
        let fitting = size ?? host.fittingSize
        let window = NSWindow(contentRect: NSRect(origin: .zero, size: fitting), styleMask: [.borderless], backing: .buffered, defer: false)
        window.appearance = NSAppearance(named: ProcessInfo.processInfo.environment["LSTACK_SNAPSHOT_LIGHT"] != nil ? .aqua : .darkAqua)
        let effect = NSVisualEffectView(frame: NSRect(origin: .zero, size: fitting))
        effect.material = .popover
        effect.state = .active
        host.frame = effect.bounds
        effect.addSubview(host)
        window.contentView = effect
        window.setFrameOrigin(NSPoint(x: -10_000, y: -10_000))
        window.orderFrontRegardless()
        Task {
            try? await Task.sleep(for: .seconds(delay))
            if size == nil {
                let fit = host.fittingSize
                window.setContentSize(fit)
                effect.frame = NSRect(origin: .zero, size: fit)
                host.frame = effect.bounds
            }
            effect.layoutSubtreeIfNeeded()
            guard let rep = effect.bitmapImageRepForCachingDisplay(in: effect.bounds) else { return }
            effect.cacheDisplay(in: effect.bounds, to: rep)
            try? rep.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: path))
            print("wrote \(path)")
        }
    }
}
