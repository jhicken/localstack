import Charts
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
            daemonBanner
            diskAccessBanner
            cmuxBanner
            Divider().opacity(0.5)
            if !store.connected {
                disconnected
            } else if store.services.isEmpty, store.machines.isEmpty {
                empty
            } else {
                ScrollView {
                    VStack(spacing: 5) {
                        if !store.machines.isEmpty {
                            MachineHeader(name: store.localName ?? "This Mac", status: "online", detail: "this Mac",
                                          count: store.services.count, collapsed: isCollapsed(Self.localKey)) { toggle(Self.localKey) }
                        }
                        if store.machines.isEmpty || !isCollapsed(Self.localKey) {
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
                            // This Mac's load is the card at the top of the menu.
                            ProcessList(processes: store.processes, key: Self.localKey)
                        }
                        ForEach(store.machines) { m in
                            MachineHeader(name: m.name, status: m.status, detail: m.online ? (m.canControl ? nil : "view only") : (m.status == "repair" ? "pair again in Settings" : (m.error ?? m.status)),
                                          count: m.services?.count ?? 0, collapsed: isCollapsed(m.name)) { toggle(m.name) }
                                .padding(.top, 10)
                            if !isCollapsed(m.name) {
                                ForEach(m.services ?? []) { s in
                                    ServiceRow(service: s)
                                }
                                if m.online, (m.services ?? []).isEmpty {
                                    Text("No services").font(.system(size: 11)).foregroundStyle(.secondary)
                                        .frame(maxWidth: .infinity, alignment: .leading).padding(.horizontal, 10)
                                }
                                ProcessList(processes: m.processes ?? [], system: m.online ? m.system : nil, key: m.name)
                            }
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
        .background(LiveWindowReporter(store: store))
        .animation(.snappy(duration: 0.25), value: store.toast)
        .animation(.snappy(duration: 0.25), value: store.cmuxDenied)
        .onAppear { Task { await store.refreshSettings() } }
    }

    /// Machines whose section is folded away (newline-separated names; this Mac is `localKey`).
    @AppStorage("lstack.collapsedMachines") private var collapsedRaw = ""
    private static let localKey = "@local"

    private func isCollapsed(_ key: String) -> Bool { collapsedRaw.split(separator: "\n").contains { $0 == key } }

    private func toggle(_ key: String) {
        var set = collapsedRaw.split(separator: "\n").map(String.init).filter { !$0.isEmpty }
        if let i = set.firstIndex(of: key) { set.remove(at: i) } else { set.append(key) }
        withAnimation(.snappy(duration: 0.2)) { collapsedRaw = set.joined(separator: "\n") }
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

    @ViewBuilder private var daemonBanner: some View {
        if let why = store.daemonOutdated {
            Banner(icon: "arrow.triangle.2.circlepath", tint: .orange,
                   text: store.restartingDaemon ? "Restarting the background service…" : "\(why.prefix(1).uppercased() + why.dropFirst()). Restart it to finish updating.",
                   action: "Restart", perform: store.restartDaemon,
                   dismiss: { store.daemonOutdated = nil })
        }
    }

    @ViewBuilder private var diskAccessBanner: some View {
        if store.connected, !store.hasDiskAccess, !store.diskAccessDismissed {
            Banner(icon: "lock.shield.fill", tint: .orange,
                   text: store.holdingForDiskAccess
                       ? "LStack is waiting for Full Disk Access before it updates its background service and restarts your services. Scripts can't use the 1Password CLI (op) or other apps' data without it."
                       : "Scripts can't use the 1Password CLI (op) or other apps' data until LStack has Full Disk Access.",
                   action: "Grant Access…", perform: store.requestDiskAccess,
                   dismiss: { store.diskAccessDismissed = true })
        }
    }

    @ViewBuilder private var cmuxBanner: some View {
        if let d = store.cmuxDenied {
            CmuxBanner(denial: d)
        }
    }

    private var subtitle: String {
        guard store.connected else { return "daemon offline" }
        if !store.machines.isEmpty {
            let all = store.allServices
            let r = all.filter(\.isRunning).count
            let online = store.machines.filter(\.online).count + 1
            return "\(r)/\(all.count) running · \(online)/\(store.machines.count + 1) Macs online"
        }
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
            // Two dashboards side by side: the terminal UI, and the web one when remote access is on.
            FooterButton(icon: "terminal", title: "Terminal") { store.openDashboard() }
                .disabled(!store.connected)
                .help("Open the terminal dashboard (lstack top)")
            if store.webAvailable {
                FooterButton(icon: "globe", title: "Web") { store.openWeb() }
                    .help("Open the web dashboard in your browser")
            }
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

/// cmux refused LStack (its socket only trusts processes started inside cmux).
private struct CmuxBanner: View {
    @Environment(AppStore.self) private var store
    let denial: CmuxDenial
    @State private var busy = false
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .top, spacing: 8) {
                Image(systemName: "hand.raised.fill").foregroundStyle(.orange).font(.system(size: 14))
                VStack(alignment: .leading, spacing: 2) {
                    Text("cmux blocked LStack").font(.system(size: 11, weight: .semibold))
                    Text("cmux only accepts commands from its own terminals. Allow LStack with a socket password (kept in LStack's secret store; you can change it in cmux Settings any time).")
                        .font(.system(size: 10)).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    if let error { Text(error).font(.system(size: 10)).foregroundStyle(.red) }
                }
                Spacer(minLength: 0)
                Button { store.cmuxDenied = nil } label: {
                    Image(systemName: "xmark").font(.system(size: 9, weight: .bold)).foregroundStyle(.secondary)
                }
                .buttonStyle(.plain)
            }
            HStack(spacing: 6) {
                Spacer()
                Button("Use Terminal") { store.useTerminalInstead() }
                Button {
                    busy = true
                    Task {
                        error = await store.fixCmux()
                        busy = false
                    }
                } label: {
                    if busy { ProgressView().controlSize(.mini) } else { Text("Allow LStack") }
                }
                .buttonStyle(.borderedProminent)
                .disabled(busy)
            }
            .controlSize(.small)
        }
        .padding(8)
        .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(Color.orange.opacity(0.12)))
        .padding(.horizontal, 8)
        .padding(.bottom, 8)
    }
}

struct MachineHeader: View {
    let name: String
    let status: String
    let detail: String?
    var count = 0
    var collapsed = false
    var toggle: () -> Void = {}

    var body: some View {
        Button(action: toggle) {
            HStack(spacing: 6) {
                Image(systemName: "chevron.right")
                    .font(.system(size: 8, weight: .bold))
                    .foregroundStyle(.tertiary)
                    .rotationEffect(.degrees(collapsed ? 0 : 90))
                Circle().fill(color).frame(width: 7, height: 7)
                Text(name).font(.system(size: 12, weight: .semibold))
                if let detail { Text(detail).font(.system(size: 10)).foregroundStyle(.secondary).lineLimit(1).truncationMode(.tail) }
                Spacer()
                if count > 0 { Text("\(count)").font(.system(size: 10)).foregroundStyle(.tertiary).monospacedDigit() }
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 3)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help("\(name): \(status) — click to \(collapsed ? "expand" : "collapse")")
    }

    private var color: Color {
        switch status {
        case "online": return .green
        case "connecting": return .yellow
        case "unauthorized": return .red
        case "repair": return .orange
        default: return .gray
        }
    }
}

/// Another Mac's whole-machine load, styled like a service row: values plus CPU / memory sparklines.
struct MachineLoadRow: View {
    let system: MachineSystem

    private var samples: [SystemSample] { Array((system.samples ?? []).suffix(120)) }
    private var last: SystemSample? { samples.last }
    private var memPct: Double? {
        guard let last, let t = system.memTotal, t > 0 else { return nil }
        return last.mem / t * 100
    }

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "desktopcomputer").font(.system(size: 13)).foregroundStyle(.secondary).frame(width: 18)
            VStack(alignment: .leading, spacing: 2) {
                Text("Load").font(.system(size: 12, weight: .medium))
                Text(meta).font(.system(size: 10)).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer(minLength: 6)
            metric("CPU", Fmt.pct(last?.cpu), \.cpu, .green, max: nil)
            metric("MEM", memPct.map { Fmt.pct($0) } ?? Fmt.bytes(last?.mem), \.mem, .blue, max: system.memTotal)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 7)
        .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(Color.primary.opacity(0.04)))
        .help("Whole-machine load. CPU is % of all cores; memory is in use vs. total RAM.")
    }

    private var meta: String {
        [system.cores.map { "\($0) cores" }, system.memTotal.map { "\(Fmt.gb($0)) RAM" }].compactMap { $0 }.joined(separator: " · ")
    }

    private func metric(_ label: String, _ value: String, _ key: KeyPath<SystemSample, Double>, _ color: Color, max cap: Double?) -> some View {
        VStack(alignment: .trailing, spacing: 2) {
            HStack(spacing: 3) {
                Text(label).font(.system(size: 9, weight: .semibold)).foregroundStyle(.secondary)
                Text(value).font(.system(size: 11, weight: .semibold)).monospacedDigit()
            }
            Chart {
                ForEach(samples, id: \.ts) { s in
                    AreaMark(x: .value("Time", s.date), y: .value("V", s[keyPath: key]))
                        .interpolationMethod(.monotone)
                        .foregroundStyle(LinearGradient(colors: [color.opacity(0.35), color.opacity(0.02)], startPoint: .top, endPoint: .bottom))
                    LineMark(x: .value("Time", s.date), y: .value("V", s[keyPath: key]))
                        .interpolationMethod(.monotone)
                        .foregroundStyle(color)
                        .lineStyle(StrokeStyle(lineWidth: 1.2, lineCap: .round))
                }
            }
            .chartXAxis(.hidden)
            .chartYAxis(.hidden)
            .chartLegend(.hidden)
            .chartYScale(domain: 0...yMax(key, cap))
            .frame(width: 64, height: 16)
        }
    }

    private func yMax(_ key: KeyPath<SystemSample, Double>, _ cap: Double?) -> Double {
        if let cap, cap > 0 { return cap }
        let peak = samples.map { $0[keyPath: key] }.max() ?? 0
        return min(Swift.max(peak * 1.25, 10), 100)
    }
}

/// A machine's load (other Macs) plus busy processes that aren't LStack services
/// (over the thresholds in Settings → Monitoring). Folding is remembered per machine.
struct ProcessList: View {
    let processes: [ProcInfo]
    var system: MachineSystem?
    @AppStorage private var open: Bool
    @State private var all = false
    private static let collapsed = 5

    init(processes: [ProcInfo], system: MachineSystem? = nil, key: String) {
        self.processes = processes
        self.system = system
        _open = AppStorage(wrappedValue: true, "lstack.procs.\(key)")
    }

    private var hasLoad: Bool { !(system?.samples ?? []).isEmpty }

    var body: some View {
        if !processes.isEmpty || hasLoad {
            VStack(alignment: .leading, spacing: 4) {
                Button { withAnimation(.snappy(duration: 0.2)) { open.toggle() } } label: {
                    HStack(spacing: 4) {
                        Image(systemName: "chevron.right").font(.system(size: 8, weight: .bold))
                            .rotationEffect(.degrees(open ? 90 : 0))
                        Text(hasLoad ? "Machine" : "Other busy processes").font(.system(size: 10, weight: .semibold)).textCase(.uppercase)
                        Spacer()
                        if !processes.isEmpty {
                            Text("\(processes.count) busy").font(.system(size: 10)).foregroundStyle(.tertiary)
                        }
                    }
                    .foregroundStyle(.secondary)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .padding(.horizontal, 10)
                if open {
                    if let system, hasLoad { MachineLoadRow(system: system) }
                    VStack(alignment: .leading, spacing: 3) {
                        ForEach(all ? processes : Array(processes.prefix(Self.collapsed))) { p in
                            HStack(spacing: 8) {
                                Text(p.name).font(.system(size: 11)).lineLimit(1).truncationMode(.middle)
                                Text(verbatim: String(p.pid)).font(.system(size: 9)).foregroundStyle(.tertiary).monospacedDigit()
                                Spacer()
                                Text(Fmt.pct(p.cpu)).frame(width: 46, alignment: .trailing)
                                Text(Fmt.bytes(p.mem)).frame(width: 64, alignment: .trailing).foregroundStyle(.secondary)
                                Text((p.net ?? 0) > 0 ? Fmt.rate(p.net) : "")
                                    .frame(width: 62, alignment: .trailing).foregroundStyle(.secondary)
                            }
                            .font(.system(size: 10.5)).monospacedDigit()
                        }
                        if processes.count > Self.collapsed {
                            Button(all ? "Show fewer" : "Show all \(processes.count)") { withAnimation(.snappy(duration: 0.2)) { all.toggle() } }
                                .buttonStyle(.link).font(.system(size: 10))
                        }
                    }
                    .padding(.horizontal, 10)
                }
            }
            .padding(.top, 6)
            .padding(.bottom, 2)
        }
    }
}
