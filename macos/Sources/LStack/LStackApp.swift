import AppKit
import SwiftUI

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        // Menu bar only — no Dock icon (also set via LSUIElement in the bundle's Info.plist).
        NSApp.setActivationPolicy(.accessory)
    }
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
        let icon = !store.connected ? "square.stack.3d.up.slash" : store.failedCount > 0 ? "exclamationmark.triangle.fill" : "square.stack.3d.up.fill"
        HStack(spacing: 3) {
            Image(systemName: icon)
            if store.connected, !store.running.isEmpty {
                Text("\(store.running.count)").monospacedDigit()
            }
        }
        // A notification or Settings asked for a service's 1Password review.
        .onChange(of: store.reviewRequest) {
            guard let s = store.reviewRequest else { return }
            store.reviewRequest = nil
            openWindow(id: "op-review", value: s)
            bringToFront()
        }
        .task {
            if await store.launch(), ProcessInfo.processInfo.environment["LSTACK_SNAPSHOT"] == nil {
                openWindow(id: "setup")
                bringToFront()
            }
        }
    }
}

@MainActor
func bringToFront() {
    NSApp.activate(ignoringOtherApps: true)
}
