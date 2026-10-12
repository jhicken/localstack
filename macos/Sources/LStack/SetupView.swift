import AppKit
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
            }
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
