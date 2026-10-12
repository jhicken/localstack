import SwiftUI

struct ServiceRow: View {
    @Environment(AppStore.self) private var store
    @Environment(\.openWindow) private var openWindow
    let service: ServiceState
    @State private var hover = false
    @State private var confirmRemove = false

    private var expanded: Bool { store.expanded == service.name }
    private var samples: [MetricSample] { store.history[service.name] ?? [] }

    var body: some View {
        VStack(spacing: 0) {
            summary
                .contentShape(Rectangle())
                .onTapGesture {
                    withAnimation(.snappy(duration: 0.25)) {
                        store.expanded = expanded ? nil : service.name
                    }
                    if !expanded { Task { await store.loadHistory(service.name) } }
                }
            if expanded {
                detail
                    .transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
        .background(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .fill(expanded ? Color.primary.opacity(0.06) : hover ? Color.primary.opacity(0.04) : .clear)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .strokeBorder(service.isFailed ? Color.red.opacity(0.35) : .clear, lineWidth: 1)
        )
        .onHover { hover = $0 }
        .confirmationDialog("Remove \(service.name)?", isPresented: $confirmRemove) {
            Button("Stop & Remove", role: .destructive) { store.remove(service.name) }
        } message: {
            Text("The service will be stopped and its definition deleted.")
        }
    }

    // MARK: collapsed row

    private var summary: some View {
        HStack(spacing: 10) {
            TypeBadge(type: service.type)
                .overlay(alignment: .bottomTrailing) {
                    StatusDot(status: service.status, size: 8)
                        .padding(1.5)
                        .background(Circle().fill(.background))
                        .offset(x: 4, y: 4)
                }
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 5) {
                    Text(service.name).font(.system(size: 13, weight: .semibold)).lineLimit(1)
                    if service.autostart {
                        Image(systemName: "power.circle.fill")
                            .font(.system(size: 10))
                            .foregroundStyle(.cyan)
                            .help("Starts at login")
                    }
                    healthIcon
                }
                Text(subtitle)
                    .font(.system(size: 11))
                    .foregroundStyle(service.isFailed ? AnyShapeStyle(.red) : AnyShapeStyle(.secondary))
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            Spacer(minLength: 6)
            if hover || expanded {
                controls
            } else if service.isRunning {
                VStack(alignment: .trailing, spacing: 1) {
                    Text(Fmt.pct(service.metrics?.cpu)).font(.system(size: 11, weight: .medium)).monospacedDigit()
                    Text(Fmt.bytes(service.metrics?.mem)).font(.system(size: 10)).foregroundStyle(.secondary).monospacedDigit()
                }
                Sparkline(samples: Array(samples.suffix(40)), value: \.cpu, color: .green)
                    .frame(width: 54, height: 22)
            } else {
                Text(service.status)
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(Theme.statusColor(service.status))
                    .textCase(.uppercase)
            }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 8)
    }

    @ViewBuilder private var healthIcon: some View {
        switch service.health {
        case "healthy": Image(systemName: "heart.fill").font(.system(size: 9)).foregroundStyle(.green).help("Healthy")
        case "unhealthy": Image(systemName: "heart.slash.fill").font(.system(size: 9)).foregroundStyle(.red).help("Unhealthy")
        case "unknown": Image(systemName: "heart").font(.system(size: 9)).foregroundStyle(.orange).help("Checking…")
        default: EmptyView()
        }
    }

    private var subtitle: String {
        if service.isFailed, let e = service.lastError { return e }
        if let u = service.uptime { return "up \(Fmt.duration(u)) · \(service.summary)" }
        return service.description ?? service.summary
    }

    private var controls: some View {
        HStack(spacing: 0) {
            if service.isRunning || service.isTransitioning {
                IconButton(icon: "stop.fill", help: "Stop", tint: .red) { store.perform(service.name, "stop") }
            } else {
                IconButton(icon: "play.fill", help: "Start", tint: .green) { store.perform(service.name, "start") }
            }
            IconButton(icon: "arrow.clockwise", help: "Restart") { store.perform(service.name, "restart") }
            IconButton(icon: "text.alignleft", help: "Logs") {
                openWindow(id: "logs", value: service.name)
                bringToFront()
            }
        }
    }

    // MARK: expanded detail

    private var detail: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 6) {
                MetricCard(title: "CPU", value: Fmt.pct(service.metrics?.cpu), samples: samples, pick: \.cpu, color: .green, format: { Fmt.pct($0) })
                MetricCard(title: "Memory", value: Fmt.bytes(service.metrics?.mem), samples: samples, pick: \.mem, color: .blue, format: { Fmt.bytes($0) })
            }
            Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 3) {
                GridRow {
                    stat("PID", service.pid.map(String.init) ?? "—")
                    stat("Uptime", Fmt.duration(service.uptime))
                    stat("Restarts", "\(service.restarts)")
                }
                GridRow {
                    stat("Net ↓", Fmt.rate(service.metrics?.netRx))
                    stat("Net ↑", Fmt.rate(service.metrics?.netTx))
                    stat("Health", service.health == "none" ? "—" : service.health)
                }
            }
            Text(service.summary)
                .font(.system(size: 10, design: .monospaced))
                .foregroundStyle(.secondary)
                .lineLimit(2)
                .textSelection(.enabled)
            if let e = service.lastError, !service.isRunning {
                Label(e, systemImage: "xmark.octagon.fill").font(.system(size: 11)).foregroundStyle(.red)
            }
            HStack(spacing: 6) {
                Toggle(isOn: Binding(get: { service.autostart }, set: { store.setAutostart(service.name, $0) })) {
                    Text("Start at login").font(.system(size: 11))
                }
                .toggleStyle(.switch)
                .controlSize(.mini)
                Spacer()
                Button {
                    store.openTerminal(service.name)
                } label: {
                    Label(service.type == "process" ? "Attach" : "Shell", systemImage: "apple.terminal")
                }
                .disabled(!service.isRunning)
                Menu {
                    Button("Edit Definition…") { store.revealConfig(service.name) }
                    Divider()
                    Button("Remove…", role: .destructive) { confirmRemove = true }
                } label: {
                    Image(systemName: "ellipsis")
                }
                .menuStyle(.borderlessButton)
                .menuIndicator(.hidden)
                .fixedSize()
            }
            .controlSize(.small)
        }
        .padding(.horizontal, 10)
        .padding(.bottom, 10)
    }

    private func stat(_ label: String, _ value: String) -> some View {
        HStack(spacing: 4) {
            Text(label).font(.system(size: 10)).foregroundStyle(.secondary)
            Text(value).font(.system(size: 11, weight: .medium)).monospacedDigit()
        }
    }
}
