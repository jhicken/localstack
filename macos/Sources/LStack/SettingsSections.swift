import AppKit
import SwiftUI

enum SettingsTab: CaseIterable {
    case general, secrets, machines, monitoring

    var title: String {
        switch self {
        case .general: return "General"
        case .secrets: return "Secrets"
        case .machines: return "Machines"
        case .monitoring: return "Monitoring"
        }
    }
}

/// Rounded group used by every Settings section.
struct SettingsCard<Content: View>: View {
    @ViewBuilder let content: () -> Content

    var body: some View {
        VStack(alignment: .leading, spacing: 0, content: content)
            .background(RoundedRectangle(cornerRadius: 12, style: .continuous).fill(.background.secondary))
            .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).strokeBorder(.separator.opacity(0.6)))
    }
}

private struct ErrorText: View {
    let text: String?
    var body: some View {
        if let text {
            Label(text, systemImage: "exclamationmark.triangle.fill")
                .font(.system(size: 11)).foregroundStyle(.red)
                .fixedSize(horizontal: false, vertical: true)
                .textSelection(.enabled)
        }
    }
}

// MARK: - Secrets

/// Write-only secret store: values go in and are passed to services as env vars; they never come back out.
struct SecretsSettings: View {
    @Environment(AppStore.self) private var store
    @State private var name = ""
    @State private var value = ""
    @State private var busy = false
    @State private var error: String?
    @State private var replacing: String?

    private var visible: [SecretInfo] { (store.secrets?.secrets ?? []).filter { !($0.isInternal ?? false) } }
    private var localServices: [ServiceState] { store.services }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            SettingsCard {
                SetupRow(icon: "key.fill", tint: store.secrets?.locked == true ? .orange : .indigo, title: "Secret store", detail: storageDetail) {
                    if store.secrets?.locked == true {
                        Button("Unlock") { Task { error = await store.unlockSecrets() } }.controlSize(.small)
                    }
                }
                Divider().padding(.leading, 52)
                form.padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
            }
            if !visible.isEmpty {
                SettingsCard {
                    ForEach(Array(visible.enumerated()), id: \.element.id) { i, s in
                        if i > 0 { Divider().padding(.leading, 12) }
                        row(s)
                    }
                }
            }
            ErrorText(text: error)
        }
        .task { await store.refreshSecrets() }
    }

    private var storageDetail: String {
        guard let st = store.secrets else { return "Loading…" }
        if st.locked { return st.error ?? "Locked — the Keychain share couldn't be read." }
        let where_ = st.storage == "keychain" ? "a Keychain item and a private file" : "two private files"
        return "Encrypted (ChaCha20-Poly1305) with a key split between \(where_). Values are write-only: they're passed to services as environment variables and scrubbed from every log — nothing can read them back."
    }

    private var form: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                TextField("NAME", text: $name)
                    .font(.system(size: 12, design: .monospaced))
                    .frame(width: 150)
                    .onChange(of: name) { name = Self.sanitize(name) }
                SecureField(replacing.map { "New value for \($0)" } ?? "Value", text: $value)
                    .onSubmit(save)
                Button(replacing == nil ? "Add" : "Replace", action: save)
                    .disabled(busy || name.isEmpty || value.isEmpty)
                if replacing != nil {
                    Button("Cancel") { replacing = nil; name = ""; value = "" }
                }
            }
            .textFieldStyle(.roundedBorder)
            .controlSize(.small)
            Text("The env var has the secret's name. Pick which services get it below.")
                .font(.system(size: 10)).foregroundStyle(.secondary)
        }
    }

    private func row(_ s: SecretInfo) -> some View {
        HStack(spacing: 8) {
            Image(systemName: s.isSet ? "checkmark.seal.fill" : "exclamationmark.circle.fill")
                .foregroundStyle(s.isSet ? .green : .orange)
            VStack(alignment: .leading, spacing: 2) {
                Text(s.name).font(.system(size: 12, weight: .medium, design: .monospaced))
                Group {
                    if !s.isSet {
                        Text("Not set — needed by \(s.usedBy.joined(separator: ", "))")
                    } else if let at = s.setAt {
                        Text("Set \(Date(timeIntervalSince1970: at / 1000), format: .relative(presentation: .named))") +
                            Text(s.usedBy.isEmpty ? " · not used yet" : " · used by \(s.usedBy.joined(separator: ", "))")
                    }
                }
                .font(.system(size: 10)).foregroundStyle(.secondary)
            }
            Spacer()
            Menu {
                ForEach(localServices) { svc in
                    let names = svc.config.secrets?.names ?? []
                    let on = names.contains(s.name)
                    Button {
                        Task { error = await toggle(s.name, in: svc) }
                    } label: {
                        Label(svc.name, systemImage: on ? "checkmark" : "")
                    }
                    .disabled(isMapped(svc))
                }
                if localServices.isEmpty { Text("No services yet") }
            } label: { Text("Services") }
                .menuStyle(.borderlessButton).fixedSize().controlSize(.small)
                .help("Choose which services receive \(s.name)")
            Button(s.isSet ? "Replace…" : "Set…") {
                replacing = s.name
                name = s.name
                value = ""
            }
            .controlSize(.small)
            if s.isSet {
                Button(role: .destructive) {
                    Task { error = await store.removeSecret(s.name) }
                } label: { Image(systemName: "trash") }
                    .buttonStyle(.borderless)
                    .help("Delete \(s.name)")
            }
        }
        .padding(.horizontal, 12).padding(.vertical, 8)
    }

    /// `secrets: {ENV: NAME}` maps are edited in YAML; the menu only manages the simple list form.
    private func isMapped(_ svc: ServiceState) -> Bool {
        if case .map = svc.config.secrets { return true }
        return false
    }

    private func toggle(_ secret: String, in svc: ServiceState) async -> String? {
        var names = svc.config.secrets?.names ?? []
        if let i = names.firstIndex(of: secret) { names.remove(at: i) } else { names.append(secret) }
        return await store.setServiceSecrets(svc.name, names)
    }

    private func save() {
        guard !name.isEmpty, !value.isEmpty else { return }
        busy = true
        let n = name, v = value
        value = ""
        Task {
            error = await store.setSecret(n, v)
            busy = false
            if error == nil { name = ""; replacing = nil }
        }
    }

    static func sanitize(_ s: String) -> String {
        String(s.uppercased().map { $0.isLetter || $0.isNumber || $0 == "_" ? $0 : "_" }.prefix(64))
    }
}

// MARK: - Machines

struct MachinesSettings: View {
    @Environment(AppStore.self) private var store
    @State private var address = ""
    @State private var code = ""
    @State private var user = "admin"
    @State private var pass = ""
    @State private var totp = ""
    @State private var label = ""
    @State private var busy = false
    @State private var error: String?
    @State private var paired: PairResult?
    @State private var pair: PairCode?
    @State private var peers: [SharedPeer] = []
    @State private var identity: LinkIdentity?
    @State private var forwards: [ForwardInfo] = []
    @State private var activity: LinkActivity?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            SettingsCard {
                SetupRow(icon: "desktopcomputer", tint: .blue, title: "Watch another Mac",
                         detail: "See its services and busy processes here, in the CLI and the web dashboard, and reach its web servers on localhost through an encrypted link. On the other Mac turn on “Share this Mac”, show a pairing code, then enter it here with that Mac's dashboard login.") {
                    EmptyView()
                }
                Divider().padding(.leading, 52)
                addForm.padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
                if let paired {
                    Divider().padding(.leading, 52)
                    VStack(alignment: .leading, spacing: 3) {
                        Label("Paired with \(paired.name)", systemImage: "lock.fill").font(.system(size: 12, weight: .semibold))
                        if let p = paired.phrase {
                            Text("Check that the other Mac shows the same words: **\(p)**").font(.system(size: 11))
                        }
                        if let g = paired.grants { Text("Access: \(g.summary)").font(.system(size: 10)).foregroundStyle(.secondary) }
                    }
                    .padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
                }
            }
            if !store.machines.isEmpty {
                SettingsCard {
                    ForEach(Array(store.machines.enumerated()), id: \.element.id) { i, m in
                        if i > 0 { Divider().padding(.leading, 12) }
                        machineRow(m)
                    }
                }
            }
            if !forwards.isEmpty {
                SettingsCard {
                    VStack(alignment: .leading, spacing: 4) {
                        sectionTitle("Forwarded to this Mac")
                        ForEach(forwards) { f in forwardRow(f) }
                    }
                    .padding(12)
                }
            }
            SettingsCard {
                SetupRow(icon: "link.badge.plus", tint: sharing ? .teal : .secondary, title: "Share this Mac",
                         detail: pairDetail) {
                    Toggle("", isOn: Binding(get: { sharing }, set: { v in
                        busy = true
                        Task {
                            error = await store.updateRemote(RemotePatch(sharing: v))
                            if !v { pair = nil }
                            busy = false
                        }
                    }))
                    .toggleStyle(.switch).labelsHidden().disabled(busy || store.remote == nil)
                }
                if sharing, let r = store.remote {
                    Divider().padding(.leading, 52)
                    VStack(alignment: .leading, spacing: 6) {
                        if let e = r.error { Text(e).font(.system(size: 11)).foregroundStyle(.red) }
                        ForEach(r.shareUrls ?? [], id: \.self) { u in
                            Label(u, systemImage: "network").font(.system(size: 11, design: .monospaced)).foregroundStyle(.secondary).textSelection(.enabled)
                        }
                        Button(pair == nil ? "Show Pairing Code" : "New Code") {
                            Task {
                                do { pair = try await store.pairCode() } catch { self.error = error.localizedDescription }
                            }
                        }
                        .controlSize(.small)
                        .disabled(!r.running)
                    }
                    .padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
                }
                if sharing, let pair {
                    Divider().padding(.leading, 52)
                    HStack(spacing: 10) {
                        Text(pair.code).font(.system(size: 22, weight: .bold, design: .monospaced)).textSelection(.enabled)
                        VStack(alignment: .leading, spacing: 2) {
                            Text("Expires ") + Text(Date(timeIntervalSince1970: pair.expiresAt / 1000), style: .relative)
                            if let u = pair.urls.first { Text("Address: \(u)").textSelection(.enabled) }
                            Text(pair.totp == true ? "Also needs this Mac's dashboard login and a 2FA code." : "Also needs this Mac's dashboard login.")
                        }
                        .font(.system(size: 10)).foregroundStyle(.secondary)
                    }
                    .padding(.leading, 52).padding(.vertical, 10)
                }
                if !peers.isEmpty {
                    Divider().padding(.leading, 52)
                    VStack(alignment: .leading, spacing: 8) {
                        sectionTitle("Paired Macs")
                        ForEach(peers) { p in peerRow(p) }
                    }
                    .padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
                }
                if let a = activity, !(a.events.isEmpty && a.tunnels.isEmpty) {
                    Divider().padding(.leading, 52)
                    activityView(a).padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
                }
                Divider().padding(.leading, 52)
                identityRow.padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
            }
            ErrorText(text: error)
        }
        .task {
            await store.refreshRemote()
            while !Task.isCancelled {
                await reload()
                try? await Task.sleep(for: .seconds(5))
            }
        }
    }

    private func reload() async {
        peers = await store.sharedPeers()
        identity = await store.linkIdentity()
        forwards = await store.forwards()
        activity = sharing ? await store.linkActivity() : nil
    }

    private var sharing: Bool { store.remote?.sharing ?? false }

    private var pairDetail: String {
        if !sharing {
            return "Let other Macs watch this one and reach the services you allow. Needs the web dashboard's login (Settings → General) — pairing asks for it plus a one-time code. Paired Macs never see secrets."
        }
        let port = store.remote?.linkPort.map { " (port \($0))" } ?? ""
        return "Show a one-time code (valid 10 minutes) and enter it on the other Mac with one of the addresses below and this Mac's dashboard login. Everything between paired Macs is end-to-end encrypted\(port) and tied to each Mac's own key."
    }

    private func sectionTitle(_ t: String) -> some View {
        Text(t).font(.system(size: 10, weight: .semibold)).foregroundStyle(.secondary).textCase(.uppercase)
    }

    private var addForm: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                TextField("Address, e.g. studio.local or 192.168.1.20", text: $address)
                TextField("Name (optional)", text: $label).frame(width: 120)
            }
            HStack(spacing: 6) {
                TextField("Code XXXX-XXXX", text: $code).font(.system(size: 12, design: .monospaced)).frame(width: 120)
                TextField("Username", text: $user).frame(width: 90)
                SecureField("Password", text: $pass).frame(width: 110)
                TextField("2FA", text: $totp).frame(width: 60)
                Spacer()
                Button {
                    add()
                } label: {
                    if busy { ProgressView().controlSize(.small) } else { Text("Pair") }
                }
                .disabled(busy || address.isEmpty || code.isEmpty || pass.isEmpty)
            }
            Text("Leave 2FA empty if that Mac doesn't use it.").font(.system(size: 10)).foregroundStyle(.tertiary)
        }
        .textFieldStyle(.roundedBorder)
        .controlSize(.small)
    }

    private func machineRow(_ m: Machine) -> some View {
        HStack(spacing: 8) {
            MachineHeader(name: m.name, status: m.status, detail: nil).fixedSize()
            VStack(alignment: .leading, spacing: 1) {
                Text(m.address ?? m.url ?? "").font(.system(size: 10, design: .monospaced)).foregroundStyle(.secondary)
                Text(statusLine(m)).font(.system(size: 10)).foregroundStyle(m.online ? AnyShapeStyle(.secondary) : AnyShapeStyle(.orange)).lineLimit(2)
                if let f = m.fingerprint {
                    Text(f).font(.system(size: 9, design: .monospaced)).foregroundStyle(.tertiary).textSelection(.enabled)
                }
            }
            Spacer()
            if m.needsRepair {
                Button("Pair Again") {
                    address = m.address ?? m.hostname ?? m.name
                    label = m.name
                }
                .controlSize(.small)
            }
            Button(role: .destructive) {
                Task { error = await store.removeMachine(m.name); await reload() }
            } label: { Text("Remove") }
                .controlSize(.small)
        }
        .padding(.horizontal, 6).padding(.vertical, 8)
    }

    private func statusLine(_ m: Machine) -> String {
        if m.status == "repair" { return "Paired with an older LStack — pair again (needs a new code)" }
        if !m.online { return m.error ?? m.status }
        let n = m.services?.count ?? 0
        let access = m.grants?.summary ?? (m.canControl ? "can control" : "read-only")
        return "🔒 \(n) service\(n == 1 ? "" : "s") · \(access)\(m.version.map { " · v\($0)" } ?? "")"
    }

    private func forwardRow(_ f: ForwardInfo) -> some View {
        HStack(spacing: 6) {
            Image(systemName: f.paused ? "pause.circle" : (f.online ? "lock.fill" : "lock.slash"))
                .font(.system(size: 10)).foregroundStyle(f.online && !f.paused ? Color.teal : .secondary)
            Text("\(f.service)@\(f.machine)").font(.system(size: 12))
            Text(":\(f.port)").font(.system(size: 10, design: .monospaced)).foregroundStyle(.secondary)
            Image(systemName: "arrow.right").font(.system(size: 9)).foregroundStyle(.tertiary)
            if let u = f.url, !f.paused {
                Button(u) { if let url = URL(string: u) { NSWorkspace.shared.open(url) } }
                    .buttonStyle(.link).font(.system(size: 11, design: .monospaced))
            } else {
                Text(f.paused ? "paused" : (f.error ?? "—")).font(.system(size: 10)).foregroundStyle(.secondary)
            }
            if f.connections > 0 { Text("\(f.connections) open").font(.system(size: 10)).foregroundStyle(.tertiary) }
            Spacer()
            Button(f.paused ? "Resume" : "Pause") {
                Task { error = await store.setForwardPaused(f, !f.paused); forwards = await store.forwards() }
            }
            .controlSize(.small)
        }
    }

    private func peerRow(_ p: SharedPeer) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                Circle().fill(p.online == true ? Color.green : Color.gray).frame(width: 7, height: 7)
                Text(p.name).font(.system(size: 12, weight: .medium))
                Group {
                    if p.online == true {
                        Text("connected\(p.address.map { " from \($0)" } ?? "")")
                    } else if let s = p.lastSeen {
                        Text("seen ") + Text(Date(timeIntervalSince1970: s / 1000), style: .relative) + Text(" ago")
                    } else {
                        Text("never connected")
                    }
                }
                .font(.system(size: 10)).foregroundStyle(.secondary)
                Spacer()
                Button("Revoke", role: .destructive) {
                    Task { error = await store.revokePeer(p.id); await reload() }
                }
                .controlSize(.small)
            }
            HStack(spacing: 12) {
                Toggle("Start / stop / restart", isOn: Binding(get: { p.grants.control }, set: { v in
                    var g = p.grants
                    g.control = v
                    update(p, g)
                }))
                Toggle("Tunnels to its services", isOn: Binding(get: { p.grants.tunnel != .none }, set: { v in
                    var g = p.grants
                    g.tunnel = v ? .all : .none
                    update(p, g)
                }))
                if case .only(let l) = p.grants.tunnel {
                    Text("(only \(l.joined(separator: ", ")))").font(.system(size: 10)).foregroundStyle(.secondary)
                }
            }
            .toggleStyle(.checkbox).font(.system(size: 11)).padding(.leading, 13)
            if let f = p.fingerprint {
                Text([f, p.phrase].compactMap { $0 }.joined(separator: " · "))
                    .font(.system(size: 9, design: .monospaced)).foregroundStyle(.tertiary).textSelection(.enabled).padding(.leading, 13)
            }
        }
    }

    private func update(_ p: SharedPeer, _ g: PeerGrants) {
        Task {
            error = await store.setGrants(p.id, g)
            peers = await store.sharedPeers()
        }
    }

    private func activityView(_ a: LinkActivity) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            sectionTitle("Activity")
            ForEach(a.tunnels, id: \.self) { t in
                Text("\(t.peer) → \(t.service):\(t.port) · \(t.connections) connection\(t.connections == 1 ? "" : "s") · \(ByteCountFormatter.string(fromByteCount: Int64(t.bytes), countStyle: .file))")
                    .font(.system(size: 10)).foregroundStyle(.secondary)
            }
            ForEach(Array(a.events.prefix(8).enumerated()), id: \.offset) { _, e in
                HStack(spacing: 4) {
                    Text(Date(timeIntervalSince1970: e.at / 1000), style: .time).foregroundStyle(.tertiary)
                    Text(describe(e)).foregroundStyle(.secondary).lineLimit(1)
                }
                .font(.system(size: 10))
            }
        }
    }

    private func describe(_ e: LinkActivity.Event) -> String {
        var parts = [e.event.replacingOccurrences(of: "-", with: " ")]
        if let p = e.peer { parts.append(p) }
        if let s = e.service { parts.append(e.port.map { "\(s):\($0)" } ?? s) }
        if let a = e.address { parts.append("from \(a)") }
        if let r = e.reason { parts.append("— \(r)") }
        return parts.joined(separator: " ")
    }

    private var identityRow: some View {
        HStack(spacing: 6) {
            Image(systemName: "key.fill").font(.system(size: 10)).foregroundStyle(.secondary)
            Text("This Mac's key").font(.system(size: 11))
            Text(identity?.fingerprint ?? "…").font(.system(size: 10, design: .monospaced)).foregroundStyle(.secondary).textSelection(.enabled)
            Spacer()
            Button("New Key…") {
                let a = NSAlert()
                a.messageText = "Replace this Mac's key?"
                a.informativeText = "Every Mac paired with this one (in both directions) will have to pair again."
                a.addButton(withTitle: "Replace Key")
                a.addButton(withTitle: "Cancel")
                a.alertStyle = .warning
                guard a.runModal() == .alertFirstButtonReturn else { return }
                Task { error = await store.rekey(); await reload() }
            }
            .controlSize(.small)
        }
    }

    private func add() {
        busy = true
        error = nil
        paired = nil
        Task {
            switch await store.addMachine(address: address, code: code, username: user, password: pass, totp: totp, name: label) {
            case .success(let r):
                paired = r
                address = ""; code = ""; totp = ""; label = ""
            case .failure(let e):
                error = e.localizedDescription
            }
            pass = ""
            busy = false
            await reload()
        }
    }
}

// MARK: - Monitoring (heartbeats + busy processes)

struct MonitoringSettings: View {
    @Environment(AppStore.self) private var store
    @State private var pingKey = ""
    @State private var baseUrl = ""
    @State private var machineUrl = ""
    @State private var machineInterval = 60.0
    @State private var cpu = 10.0
    @State private var mem = 10.0
    @State private var netMB = 1.0
    @State private var max = 15
    @State private var procEnabled = true
    @State private var error: String?
    @State private var note: String?
    @State private var loaded = false

    private var hc: HealthchecksStatus? { store.settings?.healthchecks }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            SettingsCard {
                SetupRow(icon: "waveform.path.ecg", tint: .pink, title: "Heartbeats (healthchecks.io)",
                         detail: "Ping healthchecks.io (or a self-hosted instance) so you get alerted when a service or this Mac goes down. `/start` on start, `/fail` with the last log lines (secrets scrubbed) on crashes.") {
                    EmptyView()
                }
                Divider().padding(.leading, 52)
                heartbeatForm.padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
                Divider().padding(.leading, 52)
                serviceHeartbeats.padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
            }
            SettingsCard {
                SetupRow(icon: "cpu", tint: .purple, title: "Busy processes",
                         detail: "List other processes that cross any of these limits under each machine's services. LStack services are always shown.") {
                    Toggle("", isOn: $procEnabled).toggleStyle(.switch).labelsHidden()
                        .onChange(of: procEnabled) { if loaded { saveProcesses() } }
                }
                Divider().padding(.leading, 52)
                thresholds.padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
                    .disabled(!procEnabled)
            }
            if let note { Label(note, systemImage: "checkmark.circle.fill").font(.system(size: 11)).foregroundStyle(.green) }
            ErrorText(text: error)
        }
        .task {
            await store.refreshSettings()
            sync()
        }
    }

    private func sync() {
        if let hc {
            baseUrl = hc.baseUrl
            machineUrl = hc.machine?.url ?? ""
            machineInterval = hc.machine?.interval ?? 60
        }
        if let p = store.settings?.processes {
            cpu = p.cpu; mem = p.mem; netMB = p.net / 1_048_576; max = p.max; procEnabled = p.enabled
        }
        DispatchQueue.main.async { loaded = true }
    }

    private var heartbeatForm: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 6) {
                Text("Ping key").frame(width: 80, alignment: .leading)
                SecureField(hc?.hasPingKey == true ? "•••••••• (set — type to replace)" : "Project ping key (optional)", text: $pingKey)
                Button("Save") { save(["pingKey": pingKey]) { pingKey = "" } }.disabled(pingKey.isEmpty)
                if hc?.hasPingKey == true {
                    Button("Clear") { save(["pingKey": NSNull()]) }
                }
            }
            HStack(spacing: 6) {
                Text("Server").frame(width: 80, alignment: .leading)
                TextField("https://hc-ping.com", text: $baseUrl)
                Button("Save") { save(["baseUrl": baseUrl]) }.disabled(baseUrl == (hc?.baseUrl ?? ""))
            }
            HStack(spacing: 6) {
                Text("This Mac").frame(width: 80, alignment: .leading)
                TextField(hc?.hasPingKey == true ? "Ping URL, or blank to use the ping key" : "https://hc-ping.com/<uuid>", text: $machineUrl)
                Stepper("every \(Int(machineInterval))s", value: $machineInterval, in: 15...3600, step: 15).fixedSize()
            }
            HStack(spacing: 6) {
                Spacer().frame(width: 80)
                if let st = hc?.machineStatus {
                    Image(systemName: st.ok ? "checkmark.circle.fill" : "xmark.octagon.fill").foregroundStyle(st.ok ? .green : .red)
                    (Text(st.ok ? "Last ping " : "Failed ") + Text(st.date, style: .relative) + Text(" ago"))
                        .font(.system(size: 10)).foregroundStyle(.secondary)
                }
                Spacer()
                if hc?.machine != nil {
                    Button("Turn Off") { save(["machine": NSNull()]) { machineUrl = "" } }
                    Button("Test") { test(nil) }
                }
                Button(hc?.machine == nil ? "Turn On" : "Save") {
                    var m: [String: Any] = ["interval": machineInterval]
                    m["url"] = machineUrl.isEmpty ? NSNull() : machineUrl
                    save(["machine": m])
                }
                .disabled(machineUrl.isEmpty && hc?.hasPingKey != true)
            }
        }
        .textFieldStyle(.roundedBorder)
        .controlSize(.small)
    }

    private var serviceHeartbeats: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Services").font(.system(size: 10, weight: .semibold)).foregroundStyle(.secondary).textCase(.uppercase)
            if store.services.isEmpty {
                Text("No services yet").font(.system(size: 11)).foregroundStyle(.secondary)
            }
            ForEach(store.services) { s in
                HStack(spacing: 6) {
                    Toggle(isOn: Binding(get: { s.config.heartbeat != nil && s.config.heartbeat?.enabled != false }, set: { on in
                        Task { error = await store.patchService(s.name, ["heartbeat": on ? ["enabled": true] as [String: Any] : NSNull()]) }
                    })) {
                        Text(s.name).font(.system(size: 12))
                    }
                    .toggleStyle(.checkbox)
                    .disabled(hc?.hasPingKey != true && s.config.heartbeat?.url == nil)
                    if let hb = s.heartbeat {
                        Image(systemName: hb.ok ? "checkmark.circle.fill" : "xmark.octagon.fill")
                            .font(.system(size: 10)).foregroundStyle(hb.ok ? .green : .red)
                            .help(hb.error ?? "Last ping: \(hb.kind)")
                    }
                    Spacer()
                    if s.config.heartbeat != nil {
                        Text(s.config.heartbeat?.url != nil ? "custom URL" : "slug: \(s.config.heartbeat?.slug ?? s.name)")
                            .font(.system(size: 10, design: .monospaced)).foregroundStyle(.secondary)
                        Button("Test") { test(s.name) }.controlSize(.mini)
                    }
                }
            }
            Text(hc?.hasPingKey == true
                 ? "Checks are created on first ping as <ping key>/<service name>. Set `heartbeat.url` in a service's YAML to use a specific check."
                 : "Add a ping key to turn these on with one click, or set `heartbeat.url` in a service's YAML.")
                .font(.system(size: 10)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
        }
    }

    private var thresholds: some View {
        Grid(alignment: .leading, horizontalSpacing: 10, verticalSpacing: 6) {
            GridRow {
                Text("CPU over")
                Stepper("\(Int(cpu))% of a core", value: $cpu, in: 1...800, step: 5)
                Text("Memory over")
                Stepper("\(Int(mem))% of RAM", value: $mem, in: 1...100, step: 1)
            }
            GridRow {
                Text("Network over")
                Stepper(String(format: "%.1f MB/s", netMB), value: $netMB, in: 0.1...1000, step: 0.5)
                Text("Show at most")
                Stepper("\(max) per Mac", value: $max, in: 1...100)
            }
            GridRow {
                Color.clear.frame(width: 1, height: 1)
                Color.clear.frame(width: 1, height: 1)
                Color.clear.frame(width: 1, height: 1)
                Button("Save") { saveProcesses() }.frame(maxWidth: .infinity, alignment: .trailing)
            }
        }
        .font(.system(size: 11))
        .controlSize(.small)
    }

    private func saveProcesses() {
        Task {
            error = await store.updateSettings(["processes": ["enabled": procEnabled, "cpu": cpu, "mem": mem, "net": netMB * 1_048_576, "max": max]])
            if error == nil {
                flashNote("Saved")
                await store.loadProcesses()
            }
        }
    }

    private func save(_ patch: [String: Any], then: @escaping () -> Void = {}) {
        Task {
            error = await store.updateSettings(["healthchecks": patch])
            if error == nil { then(); flashNote("Saved") }
        }
    }

    private func test(_ name: String?) {
        Task {
            error = await store.testHeartbeat(name)
            if error == nil { flashNote("Ping sent") }
        }
    }

    private func flashNote(_ s: String) {
        note = s
        Task {
            try? await Task.sleep(for: .seconds(2))
            if note == s { note = nil }
        }
    }
}
