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
    @State private var opSheet: OnePasswordSheet.Mode?
    @State private var showStored = false

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
                SetupRow(icon: "touchid", tint: store.approvals?.enabled == true ? .green : .orange, title: "Touch ID approvals",
                         detail: store.approvalProblem ?? (store.approvals?.enabled == true
                            ? "On. A service holding secrets gets them only for a configuration and values you approved with Touch ID; restarts of an approved one need nobody."
                            : "Off until this app registers its Secure Enclave key.")) {
                    if store.approvals?.enabled != true {
                        Button("Turn On") { Task { await store.registerApprovals() } }.controlSize(.small)
                    }
                }
                Divider().padding(.leading, 52)
                Label("Add secrets, plain variables, env files and 1Password references on each service: Environment Variables… in its ⋯ menu. A secret stored for one service can be picked on any other.", systemImage: "info.circle")
                    .font(.system(size: 11)).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
                Divider().padding(.leading, 52)
                storedList.padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
            }
            ErrorText(text: error)
            OnePasswordCard(sheet: $opSheet)
        }
        .task { await store.refreshSecrets() }
        .sheet(item: $opSheet) { mode in
            OnePasswordSheet(mode: mode).environment(store)
        }
    }

    /// Every stored secret, collapsed by default: secrets are usually managed from each service now.
    private var storedList: some View {
        VStack(alignment: .leading, spacing: 8) {
            Button {
                withAnimation(.easeInOut(duration: 0.15)) { showStored.toggle() }
            } label: {
                HStack(spacing: 5) {
                    Image(systemName: "chevron.right").font(.system(size: 8, weight: .bold)).rotationEffect(.degrees(showStored || replacing != nil ? 90 : 0))
                    Text("Stored secrets").font(.system(size: 10, weight: .semibold)).textCase(.uppercase)
                    Text("\(visible.count)").font(.system(size: 10))
                    Spacer()
                }
                .foregroundStyle(.secondary).contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            if showStored || replacing != nil {
                form
                if !visible.isEmpty {
                    VStack(spacing: 0) {
                        ForEach(Array(visible.enumerated()), id: \.element.id) { i, s in
                            if i > 0 { Divider() }
                            row(s)
                        }
                    }
                    .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(.background))
                    .overlay(RoundedRectangle(cornerRadius: 8, style: .continuous).strokeBorder(.separator.opacity(0.6)))
                }
            }
        }
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
            Text("Stores it without giving it to any service. Pick it on a service, or with Services below.")
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
                    } else if let at = s.fetchedAt ?? s.setAt {
                        Text(s.sourceLabel.map { "\($0) · " } ?? "") +
                            Text("\(s.fetchedAt != nil ? "fetched" : "set") \(Date(timeIntervalSince1970: at / 1000), format: .relative(presentation: .named))") +
                            Text(s.usedBy.isEmpty ? " · not used yet" : " · used by \(s.usedBy.joined(separator: ", "))")
                    }
                }
                .font(.system(size: 10)).foregroundStyle(.secondary)
                if let r = s.ref, s.source == "op" || s.source == "op-env" {
                    Text(r).font(.system(size: 9, design: .monospaced)).foregroundStyle(.tertiary).lineLimit(1).truncationMode(.middle)
                        .textSelection(.enabled)
                }
            }
            Spacer()
            if s.fromOnePassword {
                Button {
                    opSheet = .refresh(["name": s.name], "Refresh \(s.name) from 1Password")
                } label: { Image(systemName: "arrow.triangle.2.circlepath") }
                    .buttonStyle(.borderless)
                    .disabled(store.secrets?.locked == true)
                    .help("Refresh from 1Password (asks for approval in the 1Password app)")
            }
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
    @State private var copiedLink = false
    @State private var pushTo: PushTarget?
    /// Values watched Macs pushed here, waiting for approval.
    @State private var incoming: [OPShares.Incoming] = []
    @State private var answering = false

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
                        pushedGroup(incoming.filter { $0.machine == m.name })
                        let fw = forwards.filter { $0.machine == m.name }
                        if !fw.isEmpty { forwardsGroup(m.name, fw) }
                    }
                }
            }
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
                        HStack(spacing: 8) {
                            if r.shareAddress != nil {
                                Button {
                                    copyQuickLink()
                                } label: {
                                    Label(copiedLink ? "Copied" : "Copy Quick Connect Link", systemImage: copiedLink ? "checkmark" : "link")
                                }
                                .help("Copies this Mac's address and a new one-time pairing code as one link. Paste it in LStack → Settings → Machines on the other Mac.")
                            }
                            Button(pair == nil ? "Show Pairing Code" : "New Code") {
                                Task {
                                    do { pair = try await store.pairCode() } catch { self.error = error.localizedDescription }
                                }
                            }
                        }
                        .controlSize(.small)
                        .disabled((r.shareUrls ?? []).isEmpty)
                        if r.shareAddress == nil {
                            Text("Choose how this Mac is shown to get a Quick Connect link (address + code to paste on the other Mac).")
                                .font(.system(size: 10)).foregroundStyle(.tertiary)
                        }
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
                            if let l = pair.link { Text(l).font(.system(size: 10, design: .monospaced)).textSelection(.enabled).lineLimit(1).truncationMode(.middle) }
                        }
                        .font(.system(size: 10)).foregroundStyle(.secondary)
                    }
                    .padding(.leading, 52).padding(.vertical, 10)
                }
                if !peers.isEmpty {
                    Divider().padding(.leading, 52)
                    VStack(alignment: .leading, spacing: 8) {
                        sectionTitle("Paired Macs")
                        ForEach(peers) { p in
                            peerRow(p)
                            pushedGroup(incoming.filter { $0.machine == p.name })
                        }
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
        .sheet(item: $pushTo) { p in PushSecretsSheet(peer: p) }
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
        await store.refreshNetwork()
        peers = await store.sharedPeers()
        identity = await store.linkIdentity()
        forwards = await store.forwards()
        activity = sharing ? await store.linkActivity() : nil
        incoming = (try? await store.opShares())?.incoming ?? []
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
                    pair = nil
                    Task { error = await store.updateRemote(RemotePatch(shareAddress: v)); await lookUpAddresses() }
                })) {
                    Text("Automatic (local network only)").tag("")
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
            } else if let c = chosen {
                Text("Paired Macs can connect only through \(c.address)\(c.kind == "tailscale" ? " — from anywhere on your tailnet" : ""). Macs paired through another address reconnect through this one (or pair again).")
                    .font(.system(size: 10)).foregroundStyle(.tertiary).fixedSize(horizontal: false, vertical: true)
            } else {
                Text("Automatic accepts paired Macs on this local network only. Pick an address (a .lan or .local name, or Tailscale) to make it the one way in; a name keeps working when the IP changes.")
                    .font(.system(size: 10)).foregroundStyle(.tertiary).fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    /// How the link to a watched machine travels (always direct on the local network), plus a VPN hint.
    @ViewBuilder
    private func linkLine(_ m: Machine) -> some View {
        HStack(spacing: 4) {
            Text(m.isTailscale ? "Tailscale · encrypted" : "LAN · encrypted").font(.system(size: 9, weight: .medium)).padding(.horizontal, 5).padding(.vertical, 1)
                .background(Capsule().fill(Color.secondary.opacity(0.15)))
                .foregroundStyle(.secondary)
                .help(m.linkDescription)
        }
        if let v = store.vpn, v.active, !m.online, m.status != "repair" {
            Text("A VPN is on (\(v.interface ?? "VPN")) and may be blocking the local network. Allow local network access in the VPN, or turn it off.")
                .font(.system(size: 10)).foregroundStyle(.orange).lineLimit(3)
        }
    }

    private var pairDetail: String {
        if !sharing {
            return "Let other Macs watch this one and reach the services you allow. Pairing needs a one-time code shown here (+ a 2FA code if two-factor authentication is on in Settings → Security). The web dashboard doesn't need to be on. Paired Macs never see secrets."
        }
        let port = store.remote?.linkPort.map { " (port \($0))" } ?? ""
        let tfa = store.remote?.totp == true ? " and a 2FA code from your authenticator app" : ""
        return "Show a one-time code (valid 10 minutes) and enter it on the other Mac with one of the addresses below\(tfa). Paired Macs connect directly, LStack to LStack, through the address chosen below (automatic: the local network only): everything between them is end-to-end encrypted\(port) and tied to each Mac's own key."
    }

    private func sectionTitle(_ t: String) -> some View {
        Text(t).font(.system(size: 10, weight: .semibold)).foregroundStyle(.secondary).textCase(.uppercase)
    }

    private var addForm: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                TextField("Address or Quick Connect link", text: $address)
                    .onChange(of: address) { useQuickLink(address) }
                Button {
                    useQuickLink(NSPasteboard.general.string(forType: .string) ?? "")
                } label: { Image(systemName: "doc.on.clipboard") }
                    .buttonStyle(.borderless)
                    .help("Paste a Quick Connect link copied on the other Mac (fills in its address and code)")
                TextField("Name (optional)", text: $label).frame(width: 120)
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
            Text("Paste a Quick Connect link from the other Mac (Share this Mac → Copy Quick Connect Link), or type its address and code. Leave 2FA empty if that Mac doesn't use two-factor authentication.")
                .font(.system(size: 10)).foregroundStyle(.tertiary).fixedSize(horizontal: false, vertical: true)
        }
        .textFieldStyle(.roundedBorder)
        .controlSize(.small)
    }

    /// Fill the address and code from a Quick Connect link (pasted or typed into the address field).
    private func useQuickLink(_ s: String) {
        guard let q = PairCode.parse(s) else {
            if s.hasPrefix("lstack-pair") { return }
            if !s.isEmpty, s != address { error = "The clipboard doesn't hold a Quick Connect link (lstack-pair://…)" }
            return
        }
        address = q.address
        if !q.code.isEmpty { code = q.code }
        error = nil
    }

    private func copyQuickLink() {
        Task {
            do {
                var p = pair
                if p?.link == nil || p?.expired == true { p = try await store.pairCode() }
                pair = p
                guard let l = p?.link else { error = "Choose an address under “Show this Mac as” first"; return }
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(l, forType: .string)
                copiedLink = true
                try? await Task.sleep(for: .seconds(2))
                copiedLink = false
            } catch { self.error = error.localizedDescription }
        }
    }

    /// Values that Mac pushed here, waiting for approval.
    @ViewBuilder
    private func pushedGroup(_ pushed: [OPShares.Incoming]) -> some View {
        if !pushed.isEmpty {
            VStack(alignment: .leading, spacing: 4) {
                ForEach(pushed) { i in
                    IncomingPushRow(item: i, busy: $answering, showMachine: false) { await reload() }
                }
            }
            .padding(.leading, 18).padding(.trailing, 6).padding(.bottom, 8)
        }
    }

    private func machineRow(_ m: Machine) -> some View {
        HStack(spacing: 8) {
            MachineHeader(name: m.name, status: m.status, detail: nil, help: m.linkDescription).fixedSize()
            VStack(alignment: .leading, spacing: 1) {
                Text(m.address ?? m.url ?? "").font(.system(size: 10, design: .monospaced)).foregroundStyle(.secondary)
                Text(statusLine(m)).font(.system(size: 10)).foregroundStyle(m.online ? AnyShapeStyle(.secondary) : AnyShapeStyle(.orange)).lineLimit(2)
                linkLine(m)
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
            if !m.needsRepair, store.settings?.onepassword?.pushToPeers ?? true {
                Button("Push Secrets…") { pushTo = PushTarget(id: "machine:\(m.name)", name: m.name) }
                    .controlSize(.small)
                    .help("Offer values read from 1Password on this Mac to \(m.name); it accepts them there, then they're sent over this Mac's link")
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
                        Text("connected\(p.address.map { " from \($0)" } ?? "")")
                    } else if let s = p.lastSeen {
                        Text("seen ") + Text(Date(timeIntervalSince1970: s / 1000), style: .relative) + Text(" ago")
                    } else {
                        Text("never connected")
                    }
                }
                .font(.system(size: 10)).foregroundStyle(.secondary)
                Spacer()
                if store.settings?.onepassword?.pushToPeers ?? true {
                    Button("Push Secrets…") { pushTo = PushTarget(id: p.id, name: p.name) }
                        .controlSize(.small)
                        .help("Offer values read from 1Password on this Mac to \(p.name); it accepts them there")
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
