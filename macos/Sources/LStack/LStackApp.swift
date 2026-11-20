import AppKit
import SwiftUI

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        // Menu bar only — no Dock icon (also set via LSUIElement in the bundle's Info.plist).
        NSApp.setActivationPolicy(.accessory)
    }

    /// Opening LStack again (Finder, Spotlight, `open -a LStack`) while it runs: show the services panel in a
    /// window, so there is a way in when the menu bar item is hidden by a crowded menu bar.
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        NotificationCenter.default.post(name: .lstackShowPanel, object: nil)
        return true
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard Helper.stopOnQuit, !Helper.relaunching else { return .terminateNow }
        Task {
            _ = await Helper.run(["daemon", "stop"])
            await MainActor.run { NSApp.reply(toApplicationShouldTerminate: true) }
        }
        return .terminateLater
    }
}

extension Notification.Name {
    static let lstackShowPanel = Notification.Name("lstack.showPanel")
}

@main
struct LStackApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @State private var store = AppStore()

    var body: some Scene {
        MenuBarExtra {
            PopoverView()
                .environment(store)
        } label: {
            MenuBarLabel(store: store)
                .task { SnapshotRenderer.runIfRequested(store: store) }
        }
        .menuBarExtraStyle(.window)

        WindowGroup("Logs", id: "logs", for: String.self) { $name in
            LogView(name: name ?? "")
                .environment(store)
        }
        .defaultSize(width: 860, height: 560)

        Window("Add Service", id: "add") {
            ServiceEditorView()
                .environment(store)
                .id(store.addFormGeneration)
        }
        .windowResizability(.contentSize)

        WindowGroup("Edit Service", id: "service", for: EditorTarget.self) { $target in
            if let target {
                ServiceEditorView(editing: target.service, tab: target.tab)
                    .environment(store)
                    .id(target)
            }
        }
        .windowResizability(.contentSize)

        WindowGroup("1Password", id: "op-review", for: String.self) { $name in
            OnePasswordReviewView(service: name ?? "")
                .environment(store)
        }
        .windowResizability(.contentSize)

        // The menu bar panel in a window: the way in when a crowded menu bar hides the item (reopen LStack to get it).
        Window("LStack", id: "panel") {
            PopoverView()
                .environment(store)
        }
        .windowResizability(.contentSize)

        Window("LStack Setup", id: "setup") {
            SetupView()
                .environment(store)
        }
        .windowResizability(.contentSize)
    }
}

struct MenuBarLabel: View {
    let store: AppStore
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        let status = store.menuBarStatus
        HStack(spacing: 3) {
            Image(systemName: status.symbol)
            if store.connected, store.runningTopLevel > 0 {
                Text("\(store.runningTopLevel)").monospacedDigit()
            }
        }
        .help(status.summary)
        .accessibilityLabel(status.summary)
        .onChange(of: status.summary, initial: true) {
            let summary = status.summary
            // MenuBarExtra renders the label as an image, dropping SwiftUI help and accessibility text.
            DispatchQueue.main.async {
                for window in NSApp.windows {
                    if let view = window.contentView { applySummary(summary, to: view) }
                }
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: .lstackShowPanel)) { _ in
            openWindow(id: "panel")
            bringToFront()
        }
        // A notification or Settings asked for a service's 1Password review.
        .onChange(of: store.reviewRequest) {
            guard let s = store.reviewRequest else { return }
            store.reviewRequest = nil
            openWindow(id: "op-review", value: s)
            bringToFront()
        }
        .task {
            let needsSetup = await store.launch()
            if let window = LaunchWindow.initial(needsSetup: needsSetup) {
                openWindow(id: window.rawValue)
                bringToFront()
            }
        }
    }

    private func applySummary(_ summary: String, to view: NSView) {
        if let button = view as? NSStatusBarButton {
            button.toolTip = summary
            button.setAccessibilityHelp(summary)
        }
        for subview in view.subviews { applySummary(summary, to: subview) }
    }
}

@MainActor
func bringToFront() {
    NSApp.activate(ignoringOtherApps: true)
}
