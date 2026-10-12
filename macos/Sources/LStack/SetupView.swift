import AppKit
import CoreImage.CIFilterBuiltins
import SwiftUI

/// First-run setup and, afterwards, the Settings window.
struct SetupView: View {
    @Environment(AppStore.self) private var store
    @Environment(\.dismissWindow) private var dismissWindow
    @State private var wantLogin = true
    @State private var wantCLI = true
    @State private var error: String?
    @State private var onboarding = !UserDefaults.standard.bool(forKey: AppStore.onboardedKey)
    @State private var rUser = ""
    @State private var rPass = ""
    @State private var rPort = ""
    @State private var rControl = false
    @State private var rEditing = false
    @State private var rBusy = false
    @State private var rError: String?
    @State private var totpSetup: TotpSetup?
    @State private var totpCode = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            header
            if Helper.isBundled && !Helper.inApplications { moveCard }
            VStack(spacing: 0) {
                loginRow
                Divider().padding(.leading, 52)
                cliRow
                Divider().padding(.leading, 52)
                daemonRow
            }
            .background(RoundedRectangle(cornerRadius: 12, style: .continuous).fill(.background.secondary))
            .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).strokeBorder(.separator.opacity(0.6)))
            if !onboarding && store.connected { generalCard }
            if !onboarding && store.connected { remoteCard }
            if let error {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.system(size: 11))
                    .foregroundStyle(.red)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            }
            footer
        }
        .padding(24)
        .frame(width: 520)
        .task {
            await store.refreshSetup()
            await store.refreshRemote()
            await store.refreshSettings()
            syncRemoteFields()
        }
    }

    private var header: some View {
        HStack(spacing: 14) {
            AppIconView(size: 56)
            VStack(alignment: .leading, spacing: 3) {
                Text(onboarding ? "Welcome to LStack" : "LStack Settings").font(.system(size: 20, weight: .bold))
                Text("Run and watch your scripts, containers and Compose stacks from the menu bar.")
                    .font(.system(size: 12)).foregroundStyle(.secondary)
            }
        }
    }

    private var moveCard: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: "arrow.down.app.fill").foregroundStyle(.orange).font(.system(size: 18))
            VStack(alignment: .leading, spacing: 6) {
                Text("Move LStack to Applications").font(.system(size: 12, weight: .semibold))
                Text("LStack is running from \(Bundle.main.bundleURL.deletingLastPathComponent().path). Login items and the `lstack` command point at the app, so it should live in Applications.")
                    .font(.system(size: 11)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                Button("Move to Applications") { error = Helper.moveToApplications() }
                    .controlSize(.small)
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 10, style: .continuous).fill(.orange.opacity(0.1)))
    }

    // MARK: rows

    private var loginRow: some View {
        let st = store.setup?.login
        let on = onboarding ? wantLogin : (st?.installed ?? false)
        var detail = "Starts the lstack daemon and this menu bar app when you log in, then launches every service marked “start at login”."
        if !onboarding, let st, st.installed, !st.current { detail = "Points at an old copy (\(st.program ?? "?")). Toggle off and on, or click Repair." }
        return SetupRow(icon: "power", tint: .green, title: "Start at login", detail: detail) {
            Toggle("", isOn: Binding(get: { on }, set: { v in
                if onboarding { wantLogin = v } else { apply(v ? ["--login"] : ["uninstall", "--login"], restartDaemon: !v) }
            }))
            .toggleStyle(.switch).labelsHidden().disabled(store.setupBusy)
        }
    }

    private var cliRow: some View {
        let st = store.setup?.cli
        let on = onboarding ? wantCLI : (st?.installed ?? false)
        var detail = "Adds the `lstack` command so you can run `lstack top`, `lstack logs -f`, `lstack add` … in Terminal."
        if !onboarding, let st, st.installed, let p = st.path {
            detail = "Installed at \(p)."
            if let sh = st.shadowedBy { detail += " Shadowed by \(sh) earlier on your PATH." }
            else if !st.onPath { detail += " Open a new terminal window to use it." }
        }
        return SetupRow(icon: "terminal.fill", tint: .indigo, title: "Command-line tool", detail: detail) {
            VStack(alignment: .trailing, spacing: 6) {
                Toggle("", isOn: Binding(get: { on }, set: { v in
                    if onboarding { wantCLI = v } else { apply(v ? ["--cli"] : ["uninstall", "--cli"]) }
                }))
                .toggleStyle(.switch).labelsHidden().disabled(store.setupBusy)
                if !onboarding {
                    Button("Install in /usr/local/bin…") {
                        error = Helper.installCLIWithAdmin()
                        Task { await store.refreshSetup() }
                    }
                    .buttonStyle(.link).font(.system(size: 10))
                    .help("Symlink lstack into /usr/local/bin (asks for your password)")
                }
            }
        }
    }

    private var daemonRow: some View {
        let d = store.setup?.daemon
        let running = d?.running ?? store.connected
        let detail = running
            ? "Running · pid \(d?.pid.map(String.init) ?? "—") · port \(d?.port.map(String.init) ?? "—") · v\(d?.version ?? store.setup?.version ?? "?")"
            : "Not running. It supervises your services and keeps their metrics & logs."
        return SetupRow(icon: "gearshape.2.fill", tint: running ? .green : .secondary, title: "Daemon", detail: detail) {
            if !running {
                Button("Start") {
                    store.startDaemon()
                    Task { try? await Task.sleep(for: .seconds(1.5)); await store.refreshSetup() }
                }
                .controlSize(.small)
            } else {
                Button("Config Folder") { NSWorkspace.shared.open(DaemonClient.home) }.controlSize(.small)
            }
        }
    }

    // MARK: general

    private var generalCard: some View {
        VStack(spacing: 0) {
            terminalRow
            Divider().padding(.leading, 52)
            diskAccessRow
        }
        .background(RoundedRectangle(cornerRadius: 12, style: .continuous).fill(.background.secondary))
        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).strokeBorder(.separator.opacity(0.6)))
    }

    private var terminalRow: some View {
        let st = store.settings
        return SetupRow(icon: "apple.terminal.fill", tint: .gray, title: "Terminal app", detail: "Used by Dashboard, Attach and Shell to open `lstack top`, `lstack attach` …") {
            Picker("", selection: Binding(get: { st?.terminal ?? "terminal" }, set: { store.setTerminal($0) })) {
                ForEach(st?.terminals ?? [], id: \.id) { t in
                    Text(t.installed ? t.name : "\(t.name) (not installed)").tag(t.id)
                }
            }
            .labelsHidden()
            .fixedSize()
            .disabled(st == nil)
        }
    }

    private var diskAccessRow: some View {
        let granted = store.settings?.fullDiskAccess ?? false
        let detail = granted
            ? "Granted. Scripts can use tools that read other apps' data, such as the 1Password CLI (`op`)."
            : "Scripts run by the daemon can't reach other apps' data, so tools like the 1Password CLI (`op`) can't see your accounts. Click Grant, then turn on **lstack** in the list (or add it with + from the Finder window that opens)."
        return SetupRow(icon: "lock.shield.fill", tint: granted ? .green : .orange, title: "Full Disk Access", detail: detail) {
            if granted {
                Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
            } else {
                VStack(alignment: .trailing, spacing: 6) {
                    Button("Grant…") { grantDiskAccess() }.controlSize(.small)
                    Button("Recheck") { Task { await store.refreshSettings() } }
                        .buttonStyle(.link).font(.system(size: 10))
                }
            }
        }
    }

    private func grantDiskAccess() {
        if let helper = store.settings?.helperPath {
            NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: helper)])
        }
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles") {
            NSWorkspace.shared.open(url)
        }
    }

    // MARK: remote dashboard

    private var remoteCard: some View {
        let r = store.remote
        let on = r?.enabled ?? false
        let detail: String
        if let r, r.running {
            detail = "Anyone who signs in as **\(r.username)** can view status, metrics and logs\(r.allowControl ? " and start/stop services" : " (read-only)"). Plain HTTP — use on a trusted network or a VPN like Tailscale."
        } else if let e = r?.error {
            detail = e
        } else {
            detail = "Serve a password-protected web dashboard with live status, metrics and logs, so you can check on this Mac from another device."
        }
        return VStack(alignment: .leading, spacing: 0) {
            SetupRow(icon: "globe", tint: r?.running == true ? .teal : .secondary, title: "Remote dashboard", detail: detail) {
                Toggle("", isOn: Binding(get: { on || rEditing }, set: { v in
                    if v {
                        if r?.hasPassword == true { saveRemote(enable: true) } else { rEditing = true }
                    } else {
                        rEditing = false
                        saveRemote(enable: false)
                    }
                }))
                .toggleStyle(.switch).labelsHidden().disabled(rBusy || r == nil)
            }
            if let r, r.running, !r.urls.isEmpty {
                Divider().padding(.leading, 52)
                VStack(alignment: .leading, spacing: 4) {
                    ForEach(r.urls, id: \.self) { u in
                        HStack(spacing: 8) {
                            Image(systemName: "link").font(.system(size: 10)).foregroundStyle(.secondary)
                            if let url = URL(string: u) { Link(u, destination: url).font(.system(size: 12, design: .monospaced)) }
                            Button {
                                NSPasteboard.general.clearContents()
                                NSPasteboard.general.setString(u, forType: .string)
                            } label: { Image(systemName: "doc.on.doc") }
                                .buttonStyle(.borderless).help("Copy URL")
                        }
                    }
                }
                .padding(.leading, 52).padding(.vertical, 8)
            }
            if on || rEditing {
                Divider().padding(.leading, 52)
                remoteForm.padding(.leading, 52).padding(.trailing, 12).padding(.vertical, 10)
            }
        }
        .background(RoundedRectangle(cornerRadius: 12, style: .continuous).fill(.background.secondary))
        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).strokeBorder(.separator.opacity(0.6)))
    }

    private var remoteForm: some View {
        let r = store.remote
        return VStack(alignment: .leading, spacing: 8) {
            Grid(alignment: .leading, horizontalSpacing: 10, verticalSpacing: 8) {
                GridRow {
                    Text("Username").font(.system(size: 11)).foregroundStyle(.secondary)
                    TextField("admin", text: $rUser).textFieldStyle(.roundedBorder).frame(width: 180)
                }
                GridRow {
                    Text("Password").font(.system(size: 11)).foregroundStyle(.secondary)
                    SecureField(r?.hasPassword == true ? "unchanged" : "at least 8 characters", text: $rPass)
                        .textFieldStyle(.roundedBorder).frame(width: 180)
                }
                GridRow {
                    Text("Port").font(.system(size: 11)).foregroundStyle(.secondary)
                    TextField("7380", text: $rPort).textFieldStyle(.roundedBorder).frame(width: 80)
                }
                GridRow {
                    Text("")
                    Toggle("Allow start / stop / restart", isOn: $rControl).font(.system(size: 11)).toggleStyle(.checkbox)
                }
                if r?.hasPassword == true {
                    GridRow {
                        Text("Two-factor").font(.system(size: 11)).foregroundStyle(.secondary)
                        HStack(spacing: 8) {
                            if r?.totp == true {
                                Label("On", systemImage: "checkmark.shield.fill").font(.system(size: 11)).foregroundStyle(.green)
                                Button("Turn Off") {
                                    Task { rError = await store.totpDisable() }
                                }
                                .controlSize(.small)
                            } else if totpSetup == nil {
                                Text("Off").font(.system(size: 11)).foregroundStyle(.secondary)
                                Button("Set Up…") {
                                    Task {
                                        do { totpSetup = try await store.totpBegin(); totpCode = ""; rError = nil }
                                        catch { rError = error.localizedDescription }
                                    }
                                }
                                .controlSize(.small)
                            }
                        }
                    }
                }
            }
            if let t = totpSetup, r?.totp != true { totpPanel(t) }
            if let rError {
                Text(rError).font(.system(size: 11)).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
            }
            HStack {
                Spacer()
                if rBusy { ProgressView().controlSize(.small) }
                if rEditing && !(r?.enabled ?? false) {
                    Button("Cancel") { rEditing = false; rError = nil; syncRemoteFields() }.controlSize(.small)
                }
                Button(r?.enabled == true ? "Save" : "Turn On") { saveRemote(enable: true) }
                    .controlSize(.small)
                    .buttonStyle(.borderedProminent)
                    .disabled(rBusy)
            }
        }
    }

    private func totpPanel(_ t: TotpSetup) -> some View {
        HStack(alignment: .top, spacing: 14) {
            if let img = qrImage(t.uri) {
                Image(nsImage: img)
                    .interpolation(.none)
                    .resizable()
                    .frame(width: 132, height: 132)
                    .padding(6)
                    .background(RoundedRectangle(cornerRadius: 8).fill(.white))
            }
            VStack(alignment: .leading, spacing: 8) {
                Text("Scan with an authenticator app (1Password, Google Authenticator, Authy…), then enter the 6-digit code it shows.")
                    .font(.system(size: 11)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                Text(t.secret.chunked(4))
                    .font(.system(size: 11, design: .monospaced))
                    .textSelection(.enabled)
                    .help("Setup key, for manual entry")
                HStack(spacing: 6) {
                    TextField("123456", text: $totpCode)
                        .textFieldStyle(.roundedBorder)
                        .font(.system(size: 13, design: .monospaced))
                        .frame(width: 90)
                        .onSubmit(confirmTotp)
                    Button("Confirm", action: confirmTotp)
                        .controlSize(.small)
                        .buttonStyle(.borderedProminent)
                        .disabled(totpCode.filter(\.isNumber).count != 6)
                    Button("Cancel") { totpSetup = nil; rError = nil }.controlSize(.small)
                }
            }
        }
        .padding(10)
        .background(RoundedRectangle(cornerRadius: 10, style: .continuous).fill(.background.tertiary))
    }

    private func confirmTotp() {
        let code = totpCode.filter(\.isNumber)
        guard code.count == 6 else { return }
        Task {
            rError = await store.totpConfirm(code)
            if rError == nil { totpSetup = nil; totpCode = "" }
        }
    }

    private func qrImage(_ text: String) -> NSImage? {
        let f = CIFilter.qrCodeGenerator()
        f.message = Data(text.utf8)
        f.correctionLevel = "M"
        guard let out = f.outputImage?.transformed(by: CGAffineTransform(scaleX: 8, y: 8)) else { return nil }
        let rep = NSCIImageRep(ciImage: out)
        let img = NSImage(size: rep.size)
        img.addRepresentation(rep)
        return img
    }

    private func syncRemoteFields() {
        guard let r = store.remote else { return }
        rUser = r.username
        rPort = String(r.port)
        rControl = r.allowControl
        rPass = ""
    }

    private func saveRemote(enable: Bool) {
        rError = nil
        var patch = RemotePatch(enabled: enable)
        if enable, let r = store.remote {
            let user = rUser.trimmingCharacters(in: .whitespaces)
            if !user.isEmpty, user != r.username { patch.username = user }
            if !rPass.isEmpty { patch.password = rPass }
            if let p = Int(rPort), p != r.port { patch.port = p }
            if rControl != r.allowControl { patch.allowControl = rControl }
        }
        rBusy = true
        Task {
            rError = await store.updateRemote(patch)
            rBusy = false
            if rError == nil {
                rEditing = false
                syncRemoteFields()
            }
        }
    }

    // MARK: footer

    private var footer: some View {
        HStack {
            if let v = store.setup?.version { Text("lstack v\(v)").font(.system(size: 10)).foregroundStyle(.tertiary) }
            Spacer()
            if store.setupBusy { ProgressView().controlSize(.small) }
            if onboarding {
                Button("Not Now") { finish() }
                Button("Set Up LStack") { setUp() }
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.defaultAction)
                    .disabled(store.setupBusy)
            } else {
                Button("Repair") { apply(["repair"]) }.disabled(store.setupBusy)
                Button("Done") { dismissWindow(id: "setup") }.keyboardShortcut(.defaultAction)
            }
        }
    }

    private func apply(_ args: [String], restartDaemon: Bool = false) {
        error = nil
        Task {
            error = await store.runSetup(args)
            if restartDaemon { store.startDaemon() }
        }
    }

    private func setUp() {
        error = nil
        Task {
            var args: [String] = []
            if wantCLI { args.append("--cli") }
            if wantLogin { args.append("--login") }
            if !args.isEmpty { error = await store.runSetup(args) }
            if !wantLogin, store.setup?.daemon.running != true { store.startDaemon() }
            if error == nil { finish() }
        }
    }

    private func finish() {
        store.onboarded = true
        onboarding = false
        dismissWindow(id: "setup")
    }
}

private struct SetupRow<Accessory: View>: View {
    let icon: String
    let tint: Color
    let title: String
    let detail: String
    @ViewBuilder let accessory: () -> Accessory

    var body: some View {
        HStack(alignment: .center, spacing: 12) {
            Image(systemName: icon)
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(.white)
                .frame(width: 28, height: 28)
                .background(RoundedRectangle(cornerRadius: 7, style: .continuous).fill(tint.gradient))
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.system(size: 13, weight: .semibold))
                Text(LocalizedStringKey(detail)).font(.system(size: 11)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 12)
            accessory()
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 11)
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
