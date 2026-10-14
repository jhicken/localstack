import AppKit
import CoreImage.CIFilterBuiltins
import SwiftUI

enum SettingsTab: CaseIterable {
    case general, secrets, machines, security, monitoring

    var title: String {
        switch self {
        case .general: return "General"
        case .secrets: return "Secrets"
        case .machines: return "Machines"
        case .security: return "Security"
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

// MARK: - Security

/// Two-factor authentication: asked for when a new Mac pairs with this one and at web dashboard sign-in.
struct SecuritySettings: View {
    @Environment(AppStore.self) private var store
    @State private var setup: TotpSetup?
    @State private var code = ""
    @State private var busy = false
    @State private var error: String?

    private var on: Bool { store.remote?.totp == true }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            SettingsCard {
                SetupRow(icon: on ? "checkmark.shield.fill" : "shield", tint: on ? .green : .secondary, title: "Two-factor authentication", detail: detail) {
                    if on {
                        Button("Turn Off") {
                            Task { busy = true; error = await store.totpDisable(); busy = false }
                        }
                        .controlSize(.small).disabled(busy)
                    } else if setup == nil {
                        Button("Set Up…") {
                            Task {
                                do { setup = try await store.totpBegin(); code = ""; error = nil } catch { self.error = error.localizedDescription }
                            }
                        }
                        .controlSize(.small).disabled(store.remote == nil)
                    }
                }
                if let setup, !on {
                    Divider().padding(.leading, 52)
                    panel(setup).padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
                }
            }
            ErrorText(text: error)
        }
        .task { await store.refreshRemote() }
    }

    private var detail: String {
        if on {
            return "On. Pairing a new Mac with this one needs a 6-digit code from your authenticator app as well as the one-time pairing code. Web dashboard sign-in asks for it too. Macs already paired aren't affected."
        }
        return "Ask for a code from an authenticator app (1Password, Google Authenticator, Authy…) when another Mac pairs with this one — on top of the one-time pairing code — and at web dashboard sign-in."
    }

    private func panel(_ t: TotpSetup) -> some View {
        HStack(alignment: .top, spacing: 14) {
            if let img = Self.qrImage(t.uri) {
                Image(nsImage: img)
                    .interpolation(.none)
                    .resizable()
                    .frame(width: 132, height: 132)
                    .padding(6)
                    .background(RoundedRectangle(cornerRadius: 8).fill(.white))
            }
            VStack(alignment: .leading, spacing: 8) {
                Text("Scan with an authenticator app, then enter the 6-digit code it shows.")
                    .font(.system(size: 11)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                Text(t.secret.chunked(4))
                    .font(.system(size: 11, design: .monospaced))
                    .textSelection(.enabled)
                    .help("Setup key, for manual entry")
                HStack(spacing: 6) {
                    TextField("123456", text: $code)
                        .textFieldStyle(.roundedBorder)
                        .font(.system(size: 13, design: .monospaced))
                        .frame(width: 90)
                        .onSubmit(confirm)
                    Button("Confirm", action: confirm)
                        .controlSize(.small)
                        .buttonStyle(.borderedProminent)
                        .disabled(code.filter(\.isNumber).count != 6)
                    Button("Cancel") { setup = nil; error = nil }.controlSize(.small)
                }
            }
        }
    }

    private func confirm() {
        let c = code.filter(\.isNumber)
        guard c.count == 6 else { return }
        Task {
            error = await store.totpConfirm(c)
            if error == nil { setup = nil; code = "" }
        }
    }

    static func qrImage(_ text: String) -> NSImage? {
        let f = CIFilter.qrCodeGenerator()
        f.message = Data(text.utf8)
        f.correctionLevel = "M"
        guard let out = f.outputImage?.transformed(by: CGAffineTransform(scaleX: 8, y: 8)) else { return nil }
        let rep = NSCIImageRep(ciImage: out)
        let img = NSImage(size: rep.size)
        img.addRepresentation(rep)
        return img
    }
}

// MARK: - Machines

struct MachinesSettings: View {
    @Environment(AppStore.self) private var store
    @State private var address = ""
    @State private var code = ""
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
    @State private var showActivity = false
    /// Machines whose forwarded services are expanded (collapsed by default).
    @State private var openForwards: Set<String> = []
    @State private var addresses: [ShareAddress] = []
    @State private var findingAddresses = false
    @State private var authKey = ""
    @State private var tailnetBusy = false
    @State private var tailnetNote: String?
    /// Machines with a transport switch being sent (before the daemon reports progress).
    @State private var switching: Set<String> = []

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            SettingsCard {
                SetupRow(icon: "desktopcomputer", tint: .blue, title: "Watch another Mac",
                         detail: "See its services and busy processes here, in the CLI and the web dashboard, and reach its web servers on localhost through an encrypted link. On the other Mac turn on “Share this Mac” and show a pairing code, then enter it here (plus a 2FA code if that Mac uses two-factor authentication).") {
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
                        let fw = forwards.filter { $0.machine == m.name }
                        if !fw.isEmpty { forwardsGroup(m.name, fw) }
                    }
                }
            }
            tailscaleCard
            // Forwards of a machine not in the list (e.g. just removed) still show until they close.
            let orphans = forwards.filter { f in !store.machines.contains { $0.name == f.machine } }
            if !orphans.isEmpty {
                SettingsCard {
                    VStack(alignment: .leading, spacing: 4) {
                        sectionTitle("Forwarded to this Mac")
                        ForEach(orphans) { f in forwardRow(f, showMachine: true) }
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
                        addressPicker(r)
                        ForEach(r.shareUrls ?? [], id: \.self) { u in
                            Label(u, systemImage: "network").font(.system(size: 11, design: .monospaced)).foregroundStyle(.secondary).textSelection(.enabled)
                        }
                        Button(pair == nil ? "Show Pairing Code" : "New Code") {
                            Task {
                                do { pair = try await store.pairCode() } catch { self.error = error.localizedDescription }
                            }
                        }
                        .controlSize(.small)
                        .disabled((r.shareUrls ?? []).isEmpty)
                        transportPolicy(r).padding(.top, 4)
                    }
                    .padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
                }
                if sharing, let pair {
                    Divider().padding(.leading, 52)
                    HStack(spacing: 10) {
                        Text(pair.code).font(.system(size: 22, weight: .bold, design: .monospaced)).textSelection(.enabled)
                        VStack(alignment: .leading, spacing: 2) {
                            Text("Expires ") + Text(Date(timeIntervalSince1970: pair.expiresAt / 1000), style: .relative)
                            if let u = store.remote?.shareUrls?.first ?? pair.urls.first { Text("Address: \(u)").textSelection(.enabled) }
                            if pair.totp == true { Text("Also needs a 2FA code from this Mac's authenticator app.") }
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
            if sharing { await lookUpAddresses() }
            while !Task.isCancelled {
                await reload()
                try? await Task.sleep(for: .seconds(5))
            }
        }
    }

    private func reload() async {
        await store.refreshTailnet()
        await store.refreshNetwork()
        peers = await store.sharedPeers()
        identity = await store.linkIdentity()
        forwards = await store.forwards()
        activity = sharing ? await store.linkActivity() : nil
    }

    private var sharing: Bool { store.remote?.sharing ?? false }

    private func lookUpAddresses() async {
        findingAddresses = true
        addresses = await store.shareAddresses()
        findingAddresses = false
    }

    /// Discovered addresses, plus the chosen one if discovery hasn't found it (so the picker can show it).
    private func addressOptions(_ r: RemoteStatus) -> [ShareAddress] {
        var list = addresses
        if let s = r.shareAddress, !list.contains(where: { $0.address == s }) {
            list.append(ShareAddress(address: s, kind: "dns", source: "chosen", resolves: [], pointsHere: true, selected: true))
        }
        return list
    }

    private func addressLabel(_ a: ShareAddress) -> String {
        let kind = ["ip": "IP address", "dns": "DNS name", "bonjour": "Bonjour", "tailscale": "Tailscale"][a.kind] ?? a.kind
        return "\(a.address)  —  \(kind)\(a.pointsHere ? "" : " ⚠︎ doesn't reach this Mac")"
    }

    private func addressPicker(_ r: RemoteStatus) -> some View {
        let chosen = addresses.first { $0.address == r.shareAddress }
        return VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 6) {
                Picker("Show this Mac as", selection: Binding(get: { r.shareAddress ?? "" }, set: { v in
                    Task { error = await store.updateRemote(RemotePatch(shareAddress: v)); await lookUpAddresses() }
                })) {
                    Text("Automatic (IP address)").tag("")
                    ForEach(addressOptions(r)) { a in Text(addressLabel(a)).tag(a.address) }
                }
                .font(.system(size: 11)).controlSize(.small).fixedSize()
                Button {
                    Task { await lookUpAddresses() }
                } label: {
                    if findingAddresses { ProgressView().controlSize(.mini) } else { Image(systemName: "arrow.clockwise") }
                }
                .buttonStyle(.borderless).disabled(findingAddresses)
                .help("Look up this Mac's names again (DNS host name, Bonjour, reverse DNS, search domains)")
            }
            if let c = chosen, !c.pointsHere {
                Text("\(c.address) doesn't resolve to this Mac right now — other Macs may not reach it.")
                    .font(.system(size: 10)).foregroundStyle(.orange)
            } else {
                Text("A name (router DNS such as .lan, or Bonjour .local) keeps working when the IP address changes.")
                    .font(.system(size: 10)).foregroundStyle(.tertiary)
            }
        }
    }

    // MARK: Tailscale

    private var tailnetOn: Bool { store.tailnet?.enabled ?? false }

    private var tailnetMacs: [TailnetStatus.Peer] {
        guard let t = store.tailnet, t.running else { return [] }
        return (t.peers ?? []).filter { $0.isLStack == true }
    }

    private var tailscaleDetail: String {
        guard tailnetOn else {
            return "Reach paired Macs from anywhere, not just this network: LStack's own Tailscale helper joins your tailnet as a separate device (no VPN, no admin rights). The machine link stays end-to-end encrypted inside it. Downloaded only when turned on."
        }
        return "Paired Macs can use Tailscale instead of the local network (Use Tailscale on a machine). Tailscale picks a direct path when both Macs are on the same network."
    }

    private var tailscaleCard: some View {
        SettingsCard {
            SetupRow(icon: "point.3.connected.trianglepath.dotted", tint: store.tailnet?.running == true ? .indigo : .secondary, title: "Tailscale", detail: tailscaleDetail) {
                Toggle("", isOn: Binding(get: { tailnetOn }, set: { v in setTailnet(v) }))
                    .toggleStyle(.switch).labelsHidden().disabled(tailnetBusy || store.tailnet == nil)
            }
            if tailnetOn, let t = store.tailnet {
                Divider().padding(.leading, 52)
                tailnetDetail(t).padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
            }
        }
    }

    private func setTailnet(_ on: Bool) {
        var ack: Bool?
        if on && store.tailnet?.acknowledged != true {
            let a = NSAlert()
            a.messageText = "Turn on Tailscale?"
            a.informativeText = "This Mac's machine link becomes reachable from your whole tailnet, not just the local network — anywhere your tailnet reaches, over the internet. Only Macs you pair with (and that you switch to Tailscale) are accepted, and the link stays end-to-end encrypted with each Mac's key, but it's a wider door than the LAN.\n\nLStack downloads its Tailscale helper (about 25 MB, signed) and keeps it updated. You'll sign in to Tailscale once."
            a.addButton(withTitle: "Turn On")
            a.addButton(withTitle: "Cancel")
            a.alertStyle = .warning
            guard a.runModal() == .alertFirstButtonReturn else { return }
            ack = true
        }
        tailnetBusy = true
        Task {
            error = await store.setTailnet(enabled: on, acknowledged: ack)
            tailnetBusy = false
        }
    }

    private func phaseText(_ t: TailnetStatus) -> (String, Color) {
        switch t.phase {
        case "running":
            var who: [String] = []
            for v in [t.user, t.tailnet] { if let v, !v.isEmpty, !who.contains(v) { who.append(v) } }
            return ("Connected\(who.isEmpty ? "" : " — \(who.joined(separator: " · "))")", .green)
        case "needs-login": return ("Sign in to Tailscale to use it", .orange)
        case "installing": return ("Downloading the Tailscale helper…", .secondary)
        case "starting": return ("Starting…", .secondary)
        case "error": return (t.error ?? "The Tailscale helper failed", .red)
        default: return (t.phase, .secondary)
        }
    }

    private func tailnetDetail(_ t: TailnetStatus) -> some View {
        let (phase, color) = phaseText(t)
        return VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                Circle().fill(color).frame(width: 7, height: 7)
                Text(phase).font(.system(size: 11))
                if ["installing", "starting"].contains(t.phase) { ProgressView().controlSize(.mini) }
                Spacer()
                if t.running {
                    Button("Sign Out") { Task { tailnetBusy = true; error = await store.tailnetLogout(); tailnetBusy = false } }
                        .controlSize(.small).disabled(tailnetBusy)
                }
            }
            if t.phase == "needs-login" {
                HStack(spacing: 6) {
                    Button("Sign In…") { Task { tailnetBusy = true; error = await store.tailnetLogin(); tailnetBusy = false } }
                        .controlSize(.small).disabled(tailnetBusy)
                    if let u = t.authURL, !u.isEmpty {
                        Button("Open Link Again") { if let url = URL(string: u) { NSWorkspace.shared.open(url) } }
                            .buttonStyle(.link).font(.system(size: 10))
                    }
                    Text("or").font(.system(size: 10)).foregroundStyle(.secondary)
                    SecureField("Auth key (tskey-…)", text: $authKey).textFieldStyle(.roundedBorder).controlSize(.small).frame(width: 170)
                    Button("Use Key") {
                        Task {
                            tailnetBusy = true
                            error = await store.tailnetLogin(authKey: authKey)
                            if error == nil { authKey = "" }
                            tailnetBusy = false
                        }
                    }
                    .controlSize(.small).disabled(tailnetBusy || authKey.isEmpty)
                }
            }
            if let d = t.`self`?.dnsName, !d.isEmpty {
                Label("This Mac: \(d)", systemImage: "desktopcomputer").font(.system(size: 10, design: .monospaced)).foregroundStyle(.secondary).textSelection(.enabled)
            } else if let d = t.deviceName {
                Label("Joins as \(d)", systemImage: "desktopcomputer").font(.system(size: 10, design: .monospaced)).foregroundStyle(.secondary)
            }
            ForEach(t.health ?? [], id: \.self) { h in
                Label(h, systemImage: "exclamationmark.triangle").font(.system(size: 10)).foregroundStyle(.orange).lineLimit(2)
            }
            if let v = store.vpn, v.active, t.running {
                Label("A VPN is on (\(v.interface ?? "VPN")). Tailscale may go through a relay while it's on (slower), and stops if the VPN blocks it.", systemImage: "lock.shield")
                    .font(.system(size: 10)).foregroundStyle(.secondary)
            }
            HStack(spacing: 8) {
                Text("Helper \(t.version ?? t.installedVersion ?? "—")\(t.devBinary == true ? " (dev build)" : "")").font(.system(size: 10)).foregroundStyle(.secondary)
                Toggle("Update automatically", isOn: Binding(get: { t.autoUpdate ?? true }, set: { v in
                    Task { error = await store.setTailnet(autoUpdate: v) }
                }))
                .toggleStyle(.checkbox).font(.system(size: 10))
                Button {
                    Task {
                        tailnetBusy = true
                        let r = await store.tailnetUpdate()
                        error = r.error
                        tailnetNote = r.message
                        tailnetBusy = false
                    }
                } label: {
                    if t.checking == true { ProgressView().controlSize(.mini) } else { Text("Check for Updates") }
                }
                .controlSize(.small).disabled(tailnetBusy || t.checking == true || t.devBinary == true)
                .help("Updating also asks Macs connected over Tailscale to update their helper, then reconnects")
                Spacer()
            }
            if let n = tailnetNote { Text(n).font(.system(size: 10)).foregroundStyle(.secondary) }
        }
    }

    /// "LAN" or "Tailscale · direct/relayed", plus progress and VPN hints for a watched machine.
    @ViewBuilder
    private func transportLine(_ m: Machine) -> some View {
        let peer = store.tailnet?.peers?.first { $0.dnsName == m.tailnetName }
        HStack(spacing: 4) {
            Text(m.onTailscale ? "Tailscale" : "LAN").font(.system(size: 9, weight: .medium)).padding(.horizontal, 5).padding(.vertical, 1)
                .background(Capsule().fill((m.onTailscale ? Color.indigo : Color.secondary).opacity(0.15)))
                .foregroundStyle(m.onTailscale ? Color.indigo : .secondary)
                .help(m.linkDescription)
            if m.online, let v = m.via {
                if v.kind == "tailscale" {
                    let path = peer?.path == "relay" ? "relayed\(peer?.relay.map { " (DERP \($0))" } ?? "")" : (peer?.path == "direct" ? "direct" : "")
                    if !path.isEmpty { Text(path).font(.system(size: 10)).foregroundStyle(.secondary) }
                } else if m.onTailscale {
                    Text("over the LAN until Tailscale reconnects").font(.system(size: 10)).foregroundStyle(.secondary)
                }
            }
        }
        if let c = m.transportChange {
            HStack(spacing: 4) {
                if c.busy { ProgressView().controlSize(.mini) }
                Text(c.message ?? c.state).font(.system(size: 10)).foregroundStyle(c.busy ? AnyShapeStyle(.secondary) : AnyShapeStyle(.orange)).lineLimit(2)
            }
        }
        if let v = store.vpn, v.active {
            if !m.onTailscale && !m.online && m.status != "repair" {
                Text("A VPN is on (\(v.interface ?? "VPN")) and may be blocking the local network. Try Tailscale, or allow local network access in the VPN.")
                    .font(.system(size: 10)).foregroundStyle(.orange).lineLimit(3)
            } else if m.onTailscale, peer?.path == "relay" {
                Text("A VPN is on: LStack is going through Tailscale's relay (slower).").font(.system(size: 10)).foregroundStyle(.secondary)
            }
        }
    }

    @ViewBuilder
    private func transportButton(_ m: Machine) -> some View {
        let busy = switching.contains(m.name) || m.transportChange?.busy == true
        if m.onTailscale {
            Button("Use Local Network") { switchTransport(m, "local") }
                .controlSize(.small).disabled(busy)
                .help("Connect to \(m.name) over the local network only (checks it answers there first)")
        } else {
            let ready = store.tailnet?.running == true
            Button("Use Tailscale") { switchTransport(m, "tailscale") }
                .controlSize(.small).disabled(busy || !ready || !m.online)
                .help(ready ? (m.online ? "Connect to \(m.name) over Tailscale: it turns Tailscale on there too (it may need signing in there once)" : "\(m.name) must be online to switch")
                    : "Turn on Tailscale on this Mac first (Tailscale, below)")
        }
    }

    private func switchTransport(_ m: Machine, _ t: String, force: Bool = false) {
        switching.insert(m.name)
        Task {
            let r = await store.setTransport(m.name, t, force: force)
            switching.remove(m.name)
            switch r {
            case .ok: error = nil
            case .error(let e): error = e
            case .unreachable(let msg):
                let a = NSAlert()
                a.messageText = "\(m.name) doesn't answer on this network"
                a.informativeText = msg.replacingOccurrences(of: " Switch anyway?", with: "") + "\n\nIf you switch, \(m.name) stays offline until both Macs are on the same network."
                a.addButton(withTitle: "Keep Tailscale")
                a.addButton(withTitle: "Switch Anyway")
                a.alertStyle = .warning
                if a.runModal() == .alertSecondButtonReturn { switchTransport(m, t, force: true) }
            }
        }
    }

    /// Share this Mac: whether paired Macs may switch transport without asking, and requests waiting here.
    private func transportPolicy(_ r: RemoteStatus) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Toggle("Allow paired Macs to switch between LAN and Tailscale without asking", isOn: Binding(get: { r.autoApproveTransport ?? true }, set: { v in
                Task { error = await store.updateRemote(RemotePatch(autoApproveTransport: v)) }
            }))
            .toggleStyle(.checkbox).font(.system(size: 11))
            .help("A paired Mac can ask this one to turn on Tailscale and connect over it (or back to the LAN). You get a notification either way.")
            ForEach(r.transportRequests ?? []) { q in
                HStack(spacing: 6) {
                    Image(systemName: "arrow.left.arrow.right").font(.system(size: 10)).foregroundStyle(.orange)
                    Text("\(q.name) wants to connect \(q.transport == "tailscale" ? "over Tailscale" : "on the local network only")").font(.system(size: 11))
                    Spacer()
                    Button("Deny") { Task { error = await store.decideTransport(q.peerId, allow: false) } }.controlSize(.small)
                    Button("Allow") { Task { error = await store.decideTransport(q.peerId, allow: true); await reload() } }.controlSize(.small)
                }
                .padding(.leading, 18)
            }
        }
    }

    private var pairDetail: String {
        if !sharing {
            return "Let other Macs watch this one and reach the services you allow. Pairing needs a one-time code shown here (+ a 2FA code if two-factor authentication is on in Settings → Security). The web dashboard doesn't need to be on. Paired Macs never see secrets."
        }
        let port = store.remote?.linkPort.map { " (port \($0))" } ?? ""
        let tfa = store.remote?.totp == true ? " and a 2FA code from your authenticator app" : ""
        return "Show a one-time code (valid 10 minutes) and enter it on the other Mac with one of the addresses below\(tfa). Everything between paired Macs is end-to-end encrypted\(port) and tied to each Mac's own key."
    }

    private func sectionTitle(_ t: String) -> some View {
        Text(t).font(.system(size: 10, weight: .semibold)).foregroundStyle(.secondary).textCase(.uppercase)
    }

    private var addForm: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                TextField("Address, e.g. studio.local or 192.168.1.20", text: $address)
                TextField("Name (optional)", text: $label).frame(width: 120)
                let macs = tailnetMacs
                if !macs.isEmpty {
                    Menu {
                        ForEach(macs) { p in
                            Button("\(p.hostName ?? p.dnsName)\(p.online == false ? " (offline)" : "")") { address = p.dnsName }
                        }
                    } label: { Image(systemName: "point.3.connected.trianglepath.dotted") }
                        .menuStyle(.borderlessButton).fixedSize()
                        .help("Macs running LStack on your tailnet — pick one to fill in its address (its pairing code is still needed)")
                }
            }
            HStack(spacing: 6) {
                TextField("Code XXXX-XXXX", text: $code).font(.system(size: 12, design: .monospaced)).frame(width: 120)
                TextField("2FA code", text: $totp).font(.system(size: 12, design: .monospaced)).frame(width: 80)
                Spacer()
                Button {
                    add()
                } label: {
                    if busy { ProgressView().controlSize(.small) } else { Text("Pair") }
                }
                .disabled(busy || address.isEmpty || code.isEmpty)
            }
            Text("Leave 2FA empty if that Mac doesn't use two-factor authentication.").font(.system(size: 10)).foregroundStyle(.tertiary)
        }
        .textFieldStyle(.roundedBorder)
        .controlSize(.small)
    }

    private func machineRow(_ m: Machine) -> some View {
        HStack(spacing: 8) {
            MachineHeader(name: m.name, status: m.status, detail: nil, help: m.linkDescription).fixedSize()
            VStack(alignment: .leading, spacing: 1) {
                Text(m.address ?? m.url ?? "").font(.system(size: 10, design: .monospaced)).foregroundStyle(.secondary)
                Text(statusLine(m)).font(.system(size: 10)).foregroundStyle(m.online ? AnyShapeStyle(.secondary) : AnyShapeStyle(.orange)).lineLimit(2)
                transportLine(m)
                if let f = m.fingerprint {
                    Text(f).font(.system(size: 9, design: .monospaced)).foregroundStyle(.tertiary).textSelection(.enabled)
                }
            }
            Spacer()
            if !m.needsRepair { transportButton(m) }
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
                .help("Unpair: \(m.name) is told to forget this Mac too")
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

    private func forwardsGroup(_ machine: String, _ fw: [ForwardInfo]) -> some View {
        let open = openForwards.contains(machine)
        let live = fw.filter { $0.online && !$0.paused }.count
        return VStack(alignment: .leading, spacing: 4) {
            Button {
                withAnimation(.easeInOut(duration: 0.15)) {
                    if open { openForwards.remove(machine) } else { openForwards.insert(machine) }
                }
            } label: {
                HStack(spacing: 5) {
                    Image(systemName: "chevron.right").font(.system(size: 8, weight: .bold)).rotationEffect(.degrees(open ? 90 : 0))
                    Text("Forwarded to this Mac").font(.system(size: 10, weight: .semibold)).textCase(.uppercase)
                    Text("\(live) of \(fw.count) active").font(.system(size: 10))
                    Spacer()
                }
                .foregroundStyle(.secondary).contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            if open {
                ForEach(fw) { f in forwardRow(f, showMachine: false) }.padding(.leading, 13).transition(.opacity)
            }
        }
        .padding(.leading, 18).padding(.trailing, 6).padding(.bottom, 8)
    }

    private func forwardRow(_ f: ForwardInfo, showMachine: Bool) -> some View {
        HStack(spacing: 6) {
            Image(systemName: f.paused ? "pause.circle" : (f.online ? "lock.fill" : "lock.slash"))
                .font(.system(size: 10)).foregroundStyle(f.online && !f.paused ? Color.teal : .secondary)
            Text(showMachine ? "\(f.service)@\(f.machine)" : f.service).font(.system(size: 12))
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
                        Text("connected\(p.via == "tailscale" ? " over Tailscale" : "")\(p.address.map { " from \($0)" } ?? "")")
                    } else if let s = p.lastSeen {
                        Text("seen ") + Text(Date(timeIntervalSince1970: s / 1000), style: .relative) + Text(" ago")
                    } else {
                        Text("never connected")
                    }
                }
                .font(.system(size: 10)).foregroundStyle(.secondary)
                Spacer()
                if p.transport == "tailscale" {
                    Text("Tailscale").font(.system(size: 9, weight: .medium)).padding(.horizontal, 5).padding(.vertical, 1)
                        .background(Capsule().fill(Color.indigo.opacity(0.15))).foregroundStyle(.indigo)
                    Button("Local Only") {
                        Task { error = await store.setPeerTransport(p.id, "local"); await reload() }
                    }
                    .controlSize(.small)
                    .help("Accept \(p.name) only from the local network again; it switches back to connecting over the LAN")
                }
                Button("Revoke", role: .destructive) {
                    Task { error = await store.revokePeer(p.id); await reload() }
                }
                .controlSize(.small)
                .help("Unpair: \(p.name) is told to forget this Mac too (now, or when it next connects)")
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
            Button {
                withAnimation(.easeInOut(duration: 0.15)) { showActivity.toggle() }
            } label: {
                HStack(spacing: 5) {
                    Image(systemName: "chevron.right").font(.system(size: 8, weight: .bold)).rotationEffect(.degrees(showActivity ? 90 : 0))
                    sectionTitle("Activity")
                    if !showActivity {
                        let n = a.tunnels.count
                        Text(n > 0 ? "\(n) tunnel\(n == 1 ? "" : "s") · \(a.events.count) event\(a.events.count == 1 ? "" : "s")" : "\(a.events.count) event\(a.events.count == 1 ? "" : "s")")
                            .font(.system(size: 10))
                    }
                    Spacer()
                }
                .foregroundStyle(.secondary).contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            if showActivity { activityDetail(a).transition(.opacity) }
        }
    }

    private func activityDetail(_ a: LinkActivity) -> some View {
        VStack(alignment: .leading, spacing: 3) {
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
            switch await store.addMachine(address: address, code: code, totp: totp, name: label) {
            case .success(let r):
                paired = r
                address = ""; code = ""; totp = ""; label = ""
            case .failure(let e):
                error = e.localizedDescription
            }
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

private extension String {
    func chunked(_ n: Int) -> String {
        stride(from: 0, to: count, by: n).map { i -> String in
            let a = index(startIndex, offsetBy: i)
            return String(self[a..<(index(a, offsetBy: n, limitedBy: endIndex) ?? endIndex)])
        }.joined(separator: " ")
    }
}
