import SwiftUI

struct ServiceRow: View {
    @Environment(AppStore.self) private var store
    @Environment(\.openWindow) private var openWindow
    let service: ServiceState
    @State private var hover = false
    @State private var confirmRemove = false

    private var expanded: Bool { store.expanded == service.id }
    private var samples: [MetricSample] { store.history[service.id] ?? [] }
    private var canControl: Bool { store.canControl(service) }

    var body: some View {
        VStack(spacing: 0) {
            summary
                .contentShape(Rectangle())
                .onTapGesture {
                    withAnimation(.snappy(duration: 0.25)) {
                        store.expanded = expanded ? nil : service.id
                    }
                    if !expanded { Task { await store.loadHistory(service) } }
                }
            if expanded {
                // Fade in place: sliding from the top drew the detail over the rows above it while opening.
                detail
                    .transition(.opacity)
            }
        }
        .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
        .background(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .fill(expanded ? Color.primary.opacity(0.06) : hover ? Color.primary.opacity(0.04) : .clear)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .strokeBorder(service.isRestarting ? Color.orange.opacity(0.5) : service.isFailed ? Color.red.opacity(0.35) : .clear, lineWidth: 1)
        )
        .onHover { hover = $0 }
        .onAppear {
            if ProcessInfo.processInfo.environment["LSTACK_SNAPSHOT_REMOVE"] == service.name { confirmRemove = true }
        }
    }

    // MARK: collapsed row

    private var summary: some View {
        HStack(spacing: 10) {
            TypeBadge(type: service.type)
                .overlay(alignment: .bottomTrailing) {
                    StatusDot(status: service.isRestarting ? "starting" : service.status, size: 8)
                        .padding(1.5)
                        .background(Circle().fill(.background))
                        .offset(x: 4, y: 4)
                }
            VStack(alignment: .leading, spacing: 2) {
                // Badges shrink to icons when the full line doesn't fit, so the name stays visible
                // and the row never grows wider than the panel.
                ViewThatFits(in: .horizontal) {
                    titleLine(compact: false)
                    titleLine(compact: true)
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
            } else if service.isRestarting {
                // Crash loop: make it obvious it can be stopped without hovering.
                HStack(spacing: 4) {
                    TimelineView(.periodic(from: .now, by: 1)) { ctx in
                        Text("restart in \(service.restartsIn(now: ctx.date))s")
                            .font(.system(size: 10, weight: .semibold))
                            .foregroundStyle(.orange)
                            .textCase(.uppercase)
                            .monospacedDigit()
                    }
                    IconButton(icon: "stop.fill", help: "Stop — cancel the pending restart", tint: .red) { store.perform(service, "stop") }
                }
            } else if service.isRunning {
                if let u = service.links.first {
                    IconButton(icon: "globe", help: u.blocked == "paused" ? "Resume forwarding and open" : "Open \(u.url)") { store.open(u, in: service) }
                }
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

    private func titleLine(compact: Bool) -> some View {
        HStack(spacing: 5) {
            Text(service.name).font(.system(size: 13, weight: .semibold)).lineLimit(1).layoutPriority(1)
            if service.autostart {
                Image(systemName: "power.circle.fill")
                    .font(.system(size: 10))
                    .foregroundStyle(.cyan)
                    .help("Starts at login")
            }
            healthIcon
            if let hb = service.heartbeat, !hb.ok {
                Image(systemName: "waveform.path.ecg").font(.system(size: 9)).foregroundStyle(.orange)
                    .help("Heartbeat ping failed: \(hb.error ?? "unknown error")")
            }
            if let p = service.secretsPending, !service.isRemote { pendingBadge(p, compact: compact) }
            if let k = service.envStale, !k.isEmpty, service.isRunning, !service.isRemote { staleBadge(k, compact: compact) }
        }
    }

    /// The env files changed while it runs: a restart applies them.
    private func staleBadge(_ keys: [String], compact: Bool = false) -> some View {
        Button {
            store.perform(service, "restart")
        } label: {
            HStack(spacing: 3) {
                Image(systemName: "arrow.clockwise").font(.system(size: 8, weight: .bold))
                if !compact { Text("Env changed").font(.system(size: 9, weight: .semibold)) }
            }
            .lineLimit(1).fixedSize()
            .padding(.horizontal, 5).padding(.vertical, 1)
            .background(Capsule().fill(Color.blue.opacity(0.15)))
            .foregroundStyle(.blue)
        }
        .buttonStyle(.plain)
        .help("Changed in the env file since it started: \(keys.joined(separator: ", "))\nClick to restart and apply")
    }

    /// Env file secrets waiting for review: 1Password references open the review (nothing is read
    /// yet); `lstack://` lines open the editor, where they're approved.
    private func pendingBadge(_ p: SecretsPending, compact: Bool = false) -> some View {
        Button {
            if p.onlyStored { openEditor(.environment); return }
            openWindow(id: "op-review", value: service.name)
            bringToFront()
        } label: {
            HStack(spacing: 3) {
                Image(systemName: "key.horizontal.fill").font(.system(size: 8))
                Text(compact ? (p.count > 0 ? "\(p.count)" : "!") : p.onlyStored ? "Secrets: \(p.count) to approve" : p.count > 0 ? "1Password: \(p.count) to review" : "1Password: needs a look")
                    .font(.system(size: 9, weight: .semibold))
            }
            .lineLimit(1).fixedSize()
            .padding(.horizontal, 5).padding(.vertical, 1)
            .background(Capsule().fill(Color.orange.opacity(0.15)))
            .foregroundStyle(.orange)
        }
        .buttonStyle(.plain)
        .help(pendingHelp(p))
    }

    private func pendingHelp(_ p: SecretsPending) -> String {
        var lines: [String] = []
        for (label, keys) in [("New", p.new), ("Changed", p.changed), ("Not cached", p.missing), ("Blocked", p.blocked), ("Removed", p.removed),
                              ("Stored secret to approve", p.link), ("Not stored", p.notStored)] {
            if let keys, !keys.isEmpty { lines.append("\(label): \(keys.joined(separator: ", "))") }
        }
        lines += p.errors ?? []
        return ([p.onlyStored ? "Approve lstack:// secrets from the env files" : "Review secrets from the env files"] + lines).joined(separator: "\n")
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
        if service.isRestarting {
            let max = service.config.maxRestarts ?? 10
            return "Crashed\(service.exitCode.map { " (exit \($0))" } ?? "") · retry \(service.restarts)/\(max)"
        }
        if service.isFailed, let e = service.lastError { return e }
        if let u = service.uptime { return "up \(Fmt.duration(u)) · \(service.summary)" }
        return service.description ?? service.summary
    }

    private var controls: some View {
        HStack(spacing: 0) {
            if let u = service.links.first {
                IconButton(icon: "globe", help: u.blocked == "paused" ? "Resume forwarding and open" : "Open \(u.url)") { store.open(u, in: service) }
            }
            controlButtons.disabled(!canControl)
            IconButton(icon: "text.alignleft", help: "Logs") {
                openWindow(id: "logs", value: service.id)
                bringToFront()
            }
        }
    }

    @ViewBuilder private var controlButtons: some View {
        HStack(spacing: 0) {
            if service.isStoppable {
                IconButton(icon: "stop.fill", help: service.isRestarting ? "Stop — cancel the pending restart" : "Stop", tint: .red) { store.perform(service, "stop") }
            } else {
                IconButton(icon: "play.fill", help: "Start", tint: .green) { store.perform(service, "start") }
            }
            IconButton(icon: "arrow.clockwise", help: canControl ? "Restart" : "\(service.machine ?? "That Mac") doesn't allow control") { store.perform(service, "restart") }
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
            if !service.links.isEmpty { links }
            if let hb = service.heartbeat { heartbeatLine(hb) }
            if hasEnvironment { environmentChips }
            if service.isRemote {
                if !canControl {
                    Label("\(service.machine ?? "That Mac") is read-only (turn on “Allow control” there).", systemImage: "lock")
                        .font(.system(size: 10)).foregroundStyle(.secondary)
                }
            } else {
                localActions
            }
        }
        .padding(.horizontal, 10)
        .padding(.bottom, 10)
    }

    private var links: some View {
        FlowLinks(urls: service.links) { store.open($0, in: service) }
    }

    private var secretKeys: [String] { (service.config.secrets?.pairs ?? []).map(\.key) }
    private var plainKeys: [String] { (service.config.env ?? [:]).keys.sorted() }
    private var fileKeys: [String] { service.envFileKeys ?? [] }
    private var hasEnvironment: Bool { !secretKeys.isEmpty || !plainKeys.isEmpty || !fileKeys.isEmpty || !(service.config.envFiles ?? []).isEmpty }

    /// Env files, variables and secrets as chips that wrap onto more lines (names only, never values).
    private var environmentChips: some View {
        let max = 14
        let items: [(String, String?)] = (service.config.envFiles ?? []).map { ($0, "doc.text") }
            + secretKeys.map { ($0, "key.fill") }
            + (plainKeys + fileKeys).reduce(into: [String]()) { if !$0.contains($1) && !secretKeys.contains($1) { $0.append($1) } }.map { ($0, nil) }
        return FlowLayout(spacing: 4, lineSpacing: 4) {
            ForEach(items.prefix(max), id: \.0) { item in
                HStack(spacing: 3) {
                    if let icon = item.1 { Image(systemName: icon).font(.system(size: 8)).foregroundStyle(.secondary) }
                    Text(item.0).font(.system(size: 10, design: .monospaced)).lineLimit(1).truncationMode(.middle)
                }
                .padding(.horizontal, 5).padding(.vertical, 1)
                .background(Capsule().fill(Color.primary.opacity(0.07)))
            }
            if items.count > max {
                Text("+\(items.count - max) more").font(.system(size: 10)).foregroundStyle(.secondary).padding(.vertical, 1)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
        .onTapGesture { if !service.isRemote { openEditor(.environment) } }
        .help(service.isRemote ? "Environment (env files, secrets and variables)" : "Environment — click to edit env files, variables and secrets")
    }

    private func openEditor(_ tab: ServiceEditorView.Tab) {
        openWindow(id: "service", value: EditorTarget(service: service.name, tab: tab))
        bringToFront()
    }

    private func heartbeatLine(_ hb: HeartbeatStatus) -> some View {
        HStack(spacing: 4) {
            Image(systemName: "waveform.path.ecg").font(.system(size: 10)).foregroundStyle(hb.ok ? .green : .orange)
            Text(hb.ok ? "Heartbeat \(hb.kind) " : "Heartbeat failed ")
                .font(.system(size: 10)).foregroundStyle(.secondary)
            + Text(hb.date, style: .relative).font(.system(size: 10)).foregroundStyle(.secondary)
            + Text(" ago").font(.system(size: 10)).foregroundStyle(.secondary)
            if let e = hb.error, !hb.ok {
                Text("· \(e)").font(.system(size: 10)).foregroundStyle(.orange).lineLimit(1).truncationMode(.tail)
            }
        }
    }

    @ViewBuilder private var localActions: some View {
        VStack(alignment: .leading, spacing: 8) {
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
                    Button("Edit…") { openEditor(.general) }
                    Button("Environment Variables…") { openEditor(.environment) }
                    if !(service.config.envFiles ?? []).isEmpty {
                        if service.isRunning, !(service.envStale ?? []).isEmpty {
                            Button("Restart to Apply Env Changes") { store.perform(service, "restart") }
                        }
                        Button("Review 1Password References…") {
                            openWindow(id: "op-review", value: service.name)
                            bringToFront()
                        }
                    }
                    Button("Open YAML Definition") { store.revealConfig(service.name) }
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
            // Inline confirmation: a dialog/sheet would take key focus and close the menu bar panel.
            if confirmRemove {
                HStack(spacing: 6) {
                    Image(systemName: "trash.fill").foregroundStyle(.red)
                    Text("Stop and delete \(service.name)?").font(.system(size: 11, weight: .medium))
                    Spacer()
                    Button("Cancel") { confirmRemove = false }
                    Button("Stop & Remove", role: .destructive) {
                        confirmRemove = false
                        store.remove(service.name)
                    }
                    .tint(.red)
                    .buttonStyle(.borderedProminent)
                }
                .controlSize(.small)
                .padding(8)
                .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(Color.red.opacity(0.12)))
                .transition(.opacity)
            }
        }
    }

    private func stat(_ label: String, _ value: String) -> some View {
        HStack(spacing: 4) {
            Text(label).font(.system(size: 10)).foregroundStyle(.secondary)
            Text(value).font(.system(size: 11, weight: .medium)).monospacedDigit()
        }
    }
}

/// Clickable chips for a service's web addresses.
struct FlowLinks: View {
    let urls: [ServiceURL]
    let open: (ServiceURL) -> Void

    var body: some View {
        // Wraps onto more lines; a chip wider than the panel is truncated rather than widening it.
        FlowLayout(spacing: 4, lineSpacing: 4) {
            ForEach(urls, id: \.self) { u in chip(u) }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func chip(_ u: ServiceURL) -> some View {
        Button { open(u) } label: {
            HStack(spacing: 3) {
                Image(systemName: icon(u))
                    .font(.system(size: 9))
                    .foregroundStyle(paused(u) ? Color.secondary : warn(u) ? .orange : (u.via != nil ? .teal : .accentColor))
                Text(u.display).font(.system(size: 10, weight: .medium)).lineLimit(1).truncationMode(.middle)
            }
            .padding(.horizontal, 6).padding(.vertical, 2)
            .background(Capsule().fill(Color.accentColor.opacity(0.12)))
        }
        .buttonStyle(.plain)
        .help(help(u))
    }

    private func warn(_ u: ServiceURL) -> Bool { u.via == nil && u.localOnly == true }
    private func paused(_ u: ServiceURL) -> Bool { u.blocked == "paused" }

    private func icon(_ u: ServiceURL) -> String {
        if paused(u) { return "pause.circle.fill" }
        if u.via != nil { return "lock.fill" }
        return u.localOnly == true ? "exclamationmark.triangle.fill" : "link"
    }

    private func help(_ u: ServiceURL) -> String {
        if paused(u) { return "Forwarding is paused — click to resume it and open \(u.port.map { "port \($0)" } ?? "it") on this Mac" }
        if let m = u.via { return "\(u.url) — forwarded from \(m) over the encrypted link" }
        return u.localOnly == true ? "\(u.url) — bound to localhost on that Mac, so it may not be reachable from here" : u.url
    }
}
