import SwiftUI

struct PopoverView: View {
    @Environment(AppStore.self) private var store
    @Environment(\.openWindow) private var openWindow
    @State private var listHeight: CGFloat = 0
    @State private var contentHeight: CGFloat = 0

    var body: some View {
        VStack(spacing: 0) {
            header
            if store.connected, let sys = store.system, !sys.samples.isEmpty {
                SystemCard(info: sys)
                    .padding(.horizontal, 8)
                    .padding(.bottom, 8)
            }
            diskAccessBanner
            Divider().opacity(0.5)
            if !store.connected {
                disconnected
            } else if store.services.isEmpty {
                empty
            } else {
                ScrollView {
                    VStack(spacing: 4) {
                        ForEach(store.services) { s in
                            ServiceRow(service: s)
                        }
                        ForEach(store.configErrors, id: \.self) { e in
                            Label(e, systemImage: "exclamationmark.triangle.fill")
                                .font(.caption)
                                .foregroundStyle(.orange)
                                .frame(maxWidth: .infinity, alignment: .leading)
                                .padding(8)
                        }
                    }
                    .padding(8)
                    .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { listHeight = $0 }
                }
                // A ScrollView has no reliable ideal height, which left the menu bar window taller than
                // its content (blank space above/below a short list). Size it to the measured list.
                .frame(height: min(max(listHeight, 60), 520))
                .scrollBounceBehavior(.basedOnSize)
            }
            if let toast = store.toast {
                Text(toast)
                    .font(.caption)
                    .foregroundStyle(.white)
                    .padding(.horizontal, 10).padding(.vertical, 6)
                    .frame(maxWidth: .infinity)
                    .background(Color.red.opacity(0.85))
                    .transition(.move(edge: .bottom).combined(with: .opacity))
            }
            Divider().opacity(0.5)
            footer
        }
        .frame(width: 400)
        .fixedSize(horizontal: false, vertical: true)
        .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { contentHeight = $0 }
        .background(MenuWindowSizer(height: contentHeight))
        .animation(.snappy(duration: 0.25), value: store.toast)
        .onAppear { Task { await store.refreshSettings() } }
    }

    private var header: some View {
        HStack(alignment: .center, spacing: 10) {
            AppIconView(size: 30)
            VStack(alignment: .leading, spacing: 1) {
                Text("LStack").font(.system(size: 14, weight: .bold))
                Text(subtitle).font(.system(size: 11)).foregroundStyle(.secondary)
            }
            Spacer()
            IconButton(icon: "plus", help: "Add service") {
                openWindow(id: "add")
                bringToFront()
            }
            .disabled(!store.connected)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
    }

    @ViewBuilder private var diskAccessBanner: some View {
        if store.connected, store.diskAccessJustGranted {
            Banner(icon: "checkmark.shield.fill", tint: .green,
                   text: "Full Disk Access granted. Restart scripts so they pick it up.",
                   action: "Restart Scripts", perform: store.restartScripts,
                   dismiss: { store.diskAccessJustGranted = false })
        } else if store.connected, let st = store.settings, !st.fullDiskAccess, !store.diskAccessDismissed {
            Banner(icon: "lock.shield.fill", tint: .orange,
                   text: "Scripts can't use the 1Password CLI (op) or other apps' data until LStack has Full Disk Access.",
                   action: "Grant Access…", perform: store.requestDiskAccess,
                   dismiss: { store.diskAccessDismissed = true })
        }
    }

    private var subtitle: String {
        guard store.connected else { return "daemon offline" }
        let r = store.running.count
        let f = store.failedCount
        var parts = ["\(r) running"]
        let stopped = store.services.count - r - f
        if stopped > 0 { parts.append("\(max(stopped, 0)) stopped") }
        if f > 0 { parts.append("\(f) failing") }
        return parts.joined(separator: " · ")
    }

    private var disconnected: some View {
        VStack(spacing: 10) {
            Image(systemName: "bolt.horizontal.circle")
                .font(.system(size: 34, weight: .light))
                .foregroundStyle(.secondary)
            Text(store.everConnected ? "Lost connection to the daemon" : "The lstack daemon isn't running")
                .font(.system(size: 13, weight: .semibold))
            Text("The daemon supervises your services. Turn on “Start at login” in Settings to keep it running.")
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            Button {
                store.startDaemon()
            } label: {
                if store.startingDaemon { ProgressView().controlSize(.small) } else { Text("Start Daemon") }
            }
            .buttonStyle(.borderedProminent)
            .disabled(store.startingDaemon)
        }
        .padding(24)
        .frame(maxWidth: .infinity)
    }

    private var empty: some View {
        VStack(spacing: 8) {
            Image(systemName: "tray")
                .font(.system(size: 30, weight: .light))
                .foregroundStyle(.secondary)
            Text("No services yet").font(.system(size: 13, weight: .semibold))
            Text("Add scripts, Docker containers or Compose stacks\nand choose which start at login.")
                .font(.system(size: 11)).foregroundStyle(.secondary).multilineTextAlignment(.center)
            Button("Add Service") {
                openWindow(id: "add")
                bringToFront()
            }
            .buttonStyle(.borderedProminent)
        }
        .padding(24)
        .frame(maxWidth: .infinity)
    }

    private var footer: some View {
        HStack(spacing: 2) {
            FooterButton(icon: "rectangle.split.2x1", title: "Dashboard") { store.openDashboard() }
                .disabled(!store.connected)
            FooterButton(icon: "folder", title: "Config") { store.revealConfig() }
            Spacer()
            FooterButton(icon: "gearshape", title: "Settings") {
                openWindow(id: "setup")
                bringToFront()
            }
            FooterButton(icon: "power", title: "Quit") { NSApp.terminate(nil) }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
    }
}

private struct Banner: View {
    let icon: String
    let tint: Color
    let text: String
    let action: String
    let perform: () -> Void
    let dismiss: () -> Void

    var body: some View {
        HStack(alignment: .center, spacing: 8) {
            Image(systemName: icon).foregroundStyle(tint).font(.system(size: 14))
            Text(text).font(.system(size: 11)).fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 4)
            Button(action, action: perform).controlSize(.small)
            Button(action: dismiss) {
                Image(systemName: "xmark").font(.system(size: 9, weight: .bold)).foregroundStyle(.secondary)
            }
            .buttonStyle(.plain)
            .help("Hide (you can still grant it in Settings)")
        }
        .padding(8)
        .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(tint.opacity(0.12)))
        .padding(.horizontal, 8)
        .padding(.bottom, 8)
    }
}

private struct FooterButton: View {
    let icon: String
    let title: String
    let action: () -> Void
    @State private var hover = false

    var body: some View {
        Button(action: action) {
            Label(title, systemImage: icon)
                .font(.system(size: 11, weight: .medium))
                .padding(.horizontal, 8).padding(.vertical, 5)
                .background(RoundedRectangle(cornerRadius: 6, style: .continuous).fill(hover ? Color.primary.opacity(0.08) : .clear))
        }
        .buttonStyle(.plain)
        .foregroundStyle(.secondary)
        .onHover { hover = $0 }
    }
}
