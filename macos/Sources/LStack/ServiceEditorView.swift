import AppKit
import SwiftUI

/// Which service the editor window shows and which tab it opens on.
struct EditorTarget: Codable, Hashable {
    var service: String
    var tab: ServiceEditorView.Tab = .general
}

/// One environment variable in the editor: a plain value (`env:`) or a secret (`secrets:`).
struct EnvVarRow: Identifiable, Hashable {
    let id = UUID()
    var key: String
    var secret: Bool
    /// Plain value, or a new value for the secret ("" keeps the stored one).
    var value: String = ""
    /// Name in the secret store ("" = same as the key, for a new secret).
    var storedAs: String = ""
    /// Points at a secret that's already stored (from the definition, or picked from the store).
    var linked = false

    var storeName: String { storedAs.isEmpty ? SecretsSettings.sanitize(key.trimmingCharacters(in: .whitespaces)) : storedAs }
}

private struct EditorError: LocalizedError {
    let errorDescription: String?
    init(_ s: String) { errorDescription = s }
}

/// The folder's own env file, picked by default (also by `lstack add`).
let defaultEnvFile = ".env.lstack"

/// Add a service, or edit one: its definition and its environment (env files, variables and secrets).
struct ServiceEditorView: View {
    enum Tab: String, Codable, Hashable { case general, environment }

    @Environment(AppStore.self) private var store
    @Environment(\.dismiss) private var dismiss
    @Environment(\.dismissWindow) private var dismissWindow
    @Environment(\.openWindow) private var openWindow

    /// nil: a new service.
    let editing: String?
    @State private var tab: Tab
    @State private var original: ServiceConfig?
    @State private var loaded = false

    // general
    @State private var type = "process"
    @State private var name = ""
    @State private var description = ""
    @State private var command = ""
    @State private var cwd = FileManager.default.homeDirectoryForCurrentUser.path
    @State private var image = ""
    @State private var container = ""
    @State private var ports = ""
    @State private var composeFile = ""
    @State private var project = ""
    @State private var sources = "~/.zshrc"
    @State private var defaultSource = ["~/.zshrc"]
    @State private var suggestion: CommandSplit?
    @State private var restart = "on-failure"
    @State private var autostart = true
    @State private var startNow = true
    @State private var restartAfter = true
    @State private var healthKind = "none"
    @State private var healthURL = "http://localhost:3000/health"
    @State private var healthPort = ""

    // environment
    @State private var vars: [EnvVarRow] = []
    @State private var envFiles: [String] = []
    @State private var envFilesTouched = false
    @State private var detected: OPDetect?
    @State private var review: OPReview?
    @State private var opSheet: OnePasswordSheet.Mode?

    @State private var error: String?
    @State private var saving = false

    init(editing: String? = nil, tab: Tab = .general, command: String = "") {
        self.editing = editing
        _tab = State(initialValue: tab)
        _command = State(initialValue: command)
    }

    private var isNew: Bool { editing == nil }
    private var svc: ServiceState? { editing.flatMap { n in store.services.first { $0.name == n && !$0.isRemote } } }

    var body: some View {
        VStack(spacing: 0) {
            if !isNew, original == nil, loaded {
                Text("\(editing ?? "") isn't a service on this Mac.").foregroundStyle(.secondary).padding(40)
            } else {
                Picker("", selection: $tab) {
                    Text("General").tag(Tab.general)
                    Text(envTabTitle).tag(Tab.environment)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .padding(.horizontal, 20).padding(.top, 14).padding(.bottom, 2)
                Form {
                    switch tab {
                    case .general: generalSections
                    case .environment: environmentSections
                    }
                }
                .formStyle(.grouped)
                .frame(minHeight: 380, idealHeight: 560, maxHeight: 760)
                footer
            }
        }
        .frame(width: 560)
        .navigationTitle(isNew ? "Add Service" : "Edit \(editing ?? "")")
        .task { await loadOnce() }
        .task(id: detectDir) {
            guard let dir = detectDir else { detected = nil; return }
            try? await Task.sleep(for: .milliseconds(400))
            guard !Task.isCancelled else { return }
            detected = await store.opDetect(dir)
            // New services use the folder's .env.lstack unless you've chosen otherwise.
            if isNew, !envFilesTouched {
                envFiles = detected?.files.contains { $0.name == defaultEnvFile && !$0.isMount } == true ? [defaultEnvFile] : []
            }
        }
        .task(id: command) {
            guard type == "process", !command.isEmpty, isNew else { suggestion = nil; return }
            try? await Task.sleep(for: .milliseconds(350))
            guard !Task.isCancelled else { return }
            suggestion = await store.splitCommand(command, cwd: cwd)
        }
        .onChange(of: svc?.config.secrets) { mergeExternalSecrets() }
        .onChange(of: svc?.secretsPending) { Task { await loadReview() } }
        .sheet(item: $opSheet, onDismiss: { Task { await loadReview() } }) { mode in
            OnePasswordSheet(mode: mode, fixedService: editing).environment(store)
        }
    }

    private var envTabTitle: String {
        let n = vars.count
        return n == 0 ? "Environment" : "Environment (\(n))"
    }

    // MARK: loading

    private func loadOnce() async {
        guard !loaded else { return }
        if let s = await store.splitCommand("", cwd: cwd) {
            if sources == defaultSource.joined(separator: ", ") { sources = s.defaultSource.joined(separator: ", ") }
            defaultSource = s.defaultSource
        }
        await store.refreshSecrets()
        if let svc { load(svc.config) }
        loaded = true
        await loadReview()
    }

    private func load(_ c: ServiceConfig) {
        original = c
        type = c.type
        name = c.name
        description = c.description ?? ""
        command = c.command ?? ""
        cwd = c.cwd ?? ""
        image = c.image ?? ""
        container = c.container ?? ""
        ports = (c.ports ?? []).joined(separator: ", ")
        composeFile = c.composeFile ?? ""
        project = c.project ?? ""
        if let s = c.source { sources = s.joined(separator: ", ") }
        restart = c.restart ?? "on-failure"
        autostart = c.autostart ?? false
        switch c.healthcheck?.type {
        case nil: healthKind = "none"
        case "http"?: healthKind = "http"; healthURL = c.healthcheck?.url ?? ""
        case "tcp"?: healthKind = "tcp"; healthPort = c.healthcheck?.port.map(String.init) ?? ""
        default: healthKind = "custom"
        }
        envFiles = c.envFiles ?? []
        vars = (c.env ?? [:]).sorted { $0.key < $1.key }.map { EnvVarRow(key: $0.key, secret: false, value: $0.value) }
            + (c.secrets?.pairs ?? []).map { EnvVarRow(key: $0.key, secret: true, storedAs: $0.name, linked: true) }
    }

    private func loadReview() async {
        guard let editing, !(original?.envFiles ?? []).isEmpty else { review = nil; return }
        review = try? await store.opReview(editing)
    }

    /// A 1Password import or link changed the service's secrets while the editor is open: show them, and
    /// make them the base this edit is applied to (so saving doesn't drop them).
    private func mergeExternalSecrets() {
        guard loaded, let cur = svc?.config.secrets, cur != original?.secrets else { return }
        let before = Set((original?.secrets?.pairs ?? []).map(\.key))
        for p in cur.pairs where !before.contains(p.key) && !vars.contains(where: { $0.key == p.key }) {
            vars.append(EnvVarRow(key: p.key, secret: true, storedAs: p.name, linked: true))
        }
        original?.secrets = cur
    }

    // MARK: general tab

    @ViewBuilder private var generalSections: some View {
        Section {
            Picker("Type", selection: $type) {
                Label("Script", systemImage: "terminal.fill").tag("process")
                Label("Docker", systemImage: "shippingbox.fill").tag("docker")
                Label("Compose", systemImage: "square.stack.3d.up.fill").tag("compose")
            }
            .pickerStyle(.segmented)
            .disabled(!isNew)
            TextField("Name", text: $name, prompt: Text("api-server"))
                .disabled(!isNew)
            TextField("Description", text: $description, prompt: Text("optional"))
        }
        Section(type == "process" ? "Command" : type == "docker" ? "Container" : "Compose project") {
            switch type {
            case "process":
                TextField("Command", text: $command, prompt: Text("npm run dev"))
                    .font(.system(.body, design: .monospaced))
                if let s = suggestion, s.changed { splitHint(s) }
                pathField("Working directory", $cwd, directories: true)
            case "docker":
                TextField("Image", text: $image, prompt: Text("postgres:16"))
                TextField("Existing container", text: $container, prompt: Text("optional — manage an existing container instead"))
                TextField("Ports", text: $ports, prompt: Text("5432:5432, 8080:80"))
            default:
                pathField("Compose file", $composeFile, directories: false)
                TextField("Project name", text: $project, prompt: Text("optional"))
            }
            Button {
                tab = .environment
            } label: {
                LabeledContent("Environment") {
                    HStack(spacing: 4) {
                        Text(envSummary).foregroundStyle(.secondary)
                        Image(systemName: "chevron.right").font(.caption).foregroundStyle(.tertiary)
                    }
                }
            }
            .buttonStyle(.plain)
        }
        if type == "process" {
            Section {
                TextField("Source files", text: $sources, prompt: Text("none"), axis: .vertical)
                    .font(.system(.body, design: .monospaced))
                    .lineLimit(1...4)
            } header: {
                Text("Shell environment")
            } footer: {
                Text("Runs in a login shell, like a new Terminal tab, after sourcing these files (comma or newline separated), so PATH, nvm, bun, op and aliases work. Clear it to skip your shell config.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
        Section("Behavior") {
            Toggle("Start at login", isOn: $autostart)
            Picker("Restart", selection: $restart) {
                Text("On failure").tag("on-failure")
                Text("Always").tag("always")
                Text("Never").tag("never")
            }
            Picker("Health check", selection: $healthKind) {
                Text("None").tag("none")
                Text("HTTP").tag("http")
                Text("TCP port").tag("tcp")
                if healthKind == "custom" { Text("Custom (in YAML)").tag("custom") }
            }
            if healthKind == "http" { TextField("URL", text: $healthURL) }
            if healthKind == "tcp" { TextField("Port", text: $healthPort) }
            if isNew { Toggle("Start now", isOn: $startNow) }
        }
    }

    private var envSummary: String {
        var parts: [String] = []
        if !envFiles.isEmpty { parts.append(envFiles.joined(separator: ", ")) }
        let plain = vars.filter { !$0.secret }.count, secret = vars.filter(\.secret).count
        if plain > 0 { parts.append("\(plain) variable\(plain == 1 ? "" : "s")") }
        if secret > 0 { parts.append("\(secret) secret\(secret == 1 ? "" : "s")") }
        return parts.isEmpty ? "None" : parts.joined(separator: " · ")
    }

    private func splitHint(_ s: CommandSplit) -> some View {
        var parts: [String] = []
        if let d = s.split.cwd { parts.append("cd \((d as NSString).abbreviatingWithTildeInPath)") }
        if !s.split.env.isEmpty { parts.append("env " + s.split.env.keys.sorted().joined(separator: ", ")) }
        if !s.split.source.isEmpty { parts.append("source " + s.split.source.map { ($0 as NSString).lastPathComponent }.joined(separator: ", ")) }
        return HStack(spacing: 8) {
            Image(systemName: "wand.and.stars").foregroundStyle(.tint)
            Text("Found \(parts.joined(separator: " · "))")
                .font(.caption).foregroundStyle(.secondary).lineLimit(2)
            Spacer()
            Button("Split Out") { applySplit(s.split) }
                .controlSize(.small)
        }
    }

    private func applySplit(_ p: CommandSplit.Parts) {
        if let d = p.cwd { cwd = d }
        for k in p.env.keys.sorted() {
            if let i = vars.firstIndex(where: { $0.key == k && !$0.secret }) { vars[i].value = p.env[k]! }
            else { vars.append(EnvVarRow(key: k, secret: false, value: p.env[k]!)) }
        }
        if !p.source.isEmpty {
            sources = ((list(sources) ?? []) + p.source).joined(separator: ", ")
        }
        suggestion = nil
        command = p.command
    }

    private func pathField(_ label: String, _ binding: Binding<String>, directories: Bool) -> some View {
        HStack {
            TextField(label, text: binding)
            Button("Choose…") {
                let panel = NSOpenPanel()
                panel.canChooseDirectories = directories
                panel.canChooseFiles = !directories
                panel.allowsMultipleSelection = false
                if panel.runModal() == .OK, let url = panel.url { binding.wrappedValue = url.path }
            }
        }
    }

    private func list(_ s: String) -> [String]? {
        let items = s.split(whereSeparator: { $0 == "," || $0 == "\n" }).map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
        return items.isEmpty ? nil : items
    }

    // MARK: environment tab

    /// Folder env files are read from: the working directory, or the compose file's folder.
    private var detectDir: String? {
        let raw: String
        switch type {
        case "compose": raw = composeFile.isEmpty ? "" : (composeFile as NSString).deletingLastPathComponent
        default: raw = cwd
        }
        let dir = (raw.trimmingCharacters(in: .whitespaces) as NSString).expandingTildeInPath
        var isDir: ObjCBool = false
        guard !dir.isEmpty, FileManager.default.fileExists(atPath: dir, isDirectory: &isDir), isDir.boolValue else { return nil }
        return dir
    }

    private var envFileChoices: [String] {
        var names = (detected?.files ?? []).filter { !$0.example }.map(\.name)
        for f in envFiles where !names.contains(f) { names.append(f) }
        return names
    }

    @ViewBuilder private var environmentSections: some View {
        Section {
            if envFileChoices.isEmpty {
                Text(detectDir == nil
                     ? "Set a working directory (or compose file) to pick env files from its folder."
                     : "No env files in \((detectDir! as NSString).abbreviatingWithTildeInPath). Add a \(defaultEnvFile) there and LStack picks it up by default.")
                    .font(.callout).foregroundStyle(.secondary)
            }
            ForEach(envFileChoices, id: \.self) { envFileRow($0) }
            Button("Add Other File…", action: chooseEnvFile)
                .buttonStyle(.borderless).controlSize(.small)
        } header: {
            Text("Env files")
        } footer: {
            Text("Read like `op run --env-file`: later files win, and they override the variables below. Plain values are read at each start; op:// references are pulled from 1Password only after you review them.")
                .font(.caption).foregroundStyle(.secondary)
        }
        variablesSection
        onePasswordSection
    }

    private func envFileRow(_ f: String) -> some View {
        let d = detected?.files.first { $0.name == f }
        let on = Binding(
            get: { envFiles.contains(f) },
            set: { v in
                envFilesTouched = true
                if v { if !envFiles.contains(f) { envFiles.append(f) } } else { envFiles.removeAll { $0 == f } }
            }
        )
        return HStack(alignment: .top, spacing: 8) {
            Toggle(isOn: on) { EmptyView() }
                .toggleStyle(.checkbox)
                .disabled(d?.isMount == true)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(f).font(.system(.body, design: .monospaced))
                    if f == defaultEnvFile {
                        Text("default").font(.system(size: 9, weight: .semibold)).textCase(.uppercase)
                            .padding(.horizontal, 5).padding(.vertical, 1)
                            .background(Capsule().fill(Color.accentColor.opacity(0.15))).foregroundStyle(.tint)
                    }
                }
                Text(d.map(fileDetail) ?? "not in this folder")
                    .font(.caption).foregroundStyle(d?.error != nil || d == nil ? AnyShapeStyle(.orange) : AnyShapeStyle(.secondary))
                    .lineLimit(3).fixedSize(horizontal: false, vertical: true)
            }
            Spacer()
            if d != nil, d?.isMount != true {
                Button { openEnvFile(f) } label: { Image(systemName: "square.and.pencil") }
                    .buttonStyle(.borderless)
                    .help("Open \(f) in your editor")
            }
        }
    }

    private func fileDetail(_ d: OPDetect.File) -> String {
        if d.isMount { return "1Password Environment mount — use Import Environment below" }
        if let e = d.error { return e }
        var parts: [String] = []
        if let p = d.plain, !p.isEmpty { parts.append(p.joined(separator: ", ")) }
        if let r = d.refs, !r.isEmpty { parts.append("1Password: " + r.map(\.key).joined(separator: ", ")) }
        return parts.isEmpty ? "empty" : parts.joined(separator: " · ")
    }

    private func envFileURL(_ f: String) -> URL? {
        let p = (f as NSString).expandingTildeInPath
        if p.hasPrefix("/") { return URL(fileURLWithPath: p) }
        guard let dir = detectDir else { return nil }
        return URL(fileURLWithPath: dir).appendingPathComponent(p)
    }

    private func openEnvFile(_ f: String) {
        guard let url = envFileURL(f) else { return }
        let app = NSWorkspace.shared.urlForApplication(toOpen: url) ?? URL(fileURLWithPath: "/System/Applications/TextEdit.app")
        NSWorkspace.shared.open([url], withApplicationAt: app, configuration: NSWorkspace.OpenConfiguration())
    }

    private func chooseEnvFile() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.showsHiddenFiles = true
        if let d = detectDir { panel.directoryURL = URL(fileURLWithPath: d) }
        guard panel.runModal() == .OK, let url = panel.url else { return }
        var path = url.path
        if let d = detectDir, path.hasPrefix(d + "/") { path = String(path.dropFirst(d.count + 1)) }
        envFilesTouched = true
        if !envFiles.contains(path) { envFiles.append(path) }
    }

    private var storedSecrets: [SecretInfo] { (store.secrets?.secrets ?? []).filter { !($0.isInternal ?? false) } }
    private func storedInfo(_ name: String) -> SecretInfo? { storedSecrets.first { $0.name == name } }

    /// Stored secrets this service doesn't use yet.
    private var availableSecrets: [SecretInfo] {
        let used = Set(vars.filter(\.secret).map(\.storeName))
        return storedSecrets.filter { !used.contains($0.name) && $0.isSet }
    }

    private var variablesSection: some View {
        Section {
            if vars.isEmpty {
                Text("No variables yet.").font(.callout).foregroundStyle(.secondary)
            }
            ForEach($vars) { $v in varRow($v) }
            HStack(spacing: 12) {
                Button { vars.append(EnvVarRow(key: "", secret: false)) } label: { Label("Add Variable", systemImage: "plus") }
                Button { vars.append(EnvVarRow(key: "", secret: true)) } label: { Label("Add Secret", systemImage: "lock.fill") }
                    .disabled(store.secrets?.locked == true)
                Menu {
                    ForEach(availableSecrets) { s in
                        Button("\(s.name)\(s.usedBy.isEmpty ? "" : " — used by \(s.usedBy.joined(separator: ", "))")") {
                            vars.append(EnvVarRow(key: s.name, secret: true, storedAs: s.name, linked: true))
                        }
                    }
                } label: { Label("Use Stored Secret", systemImage: "key.fill") }
                    .fixedSize()
                    .disabled(availableSecrets.isEmpty)
                    .help("Give this service a secret that's already stored (e.g. one another service uses)")
                Spacer()
            }
            .buttonStyle(.borderless)
            .controlSize(.small)
        } header: {
            Text("Environment variables")
        } footer: {
            Text("Secrets are stored encrypted in LStack's secret store, passed in as environment variables and scrubbed from logs; they can't be read back. A stored secret can be used by several services.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private func varRow(_ v: Binding<EnvVarRow>) -> some View {
        let row = v.wrappedValue
        return VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 6) {
                TextField("NAME", text: v.key)
                    .font(.system(size: 12, design: .monospaced))
                    .frame(width: 170)
                if row.secret {
                    SecureField(secretPlaceholder(row), text: v.value)
                } else {
                    TextField("value", text: v.value).font(.system(size: 12, design: .monospaced))
                }
                Button {
                    toggleSecret(v)
                } label: {
                    Image(systemName: row.secret ? "lock.fill" : "lock.open").foregroundStyle(row.secret ? Color.accentColor : .secondary)
                }
                .buttonStyle(.borderless)
                .disabled(!row.secret && store.secrets?.locked == true)
                .help(row.secret ? "Secret — click to make it a plain variable" : "Plain variable — click to store it as an encrypted secret")
                Button {
                    vars.removeAll { $0.id == row.id }
                } label: { Image(systemName: "minus.circle.fill").foregroundStyle(.secondary) }
                    .buttonStyle(.borderless)
                    .help("Remove \(row.key.isEmpty ? "this variable" : row.key) from this service")
            }
            .textFieldStyle(.roundedBorder)
            .labelsHidden()
            if row.secret { secretCaption(v) }
        }
    }

    private func secretPlaceholder(_ v: EnvVarRow) -> String {
        if v.linked || storedInfo(v.storeName)?.isSet == true { return "•••••• stored — type to replace" }
        return "Secret value"
    }

    private func toggleSecret(_ v: Binding<EnvVarRow>) {
        if v.wrappedValue.secret {
            if v.wrappedValue.linked { v.wrappedValue.value = "" }
            v.wrappedValue.secret = false
            v.wrappedValue.linked = false
            v.wrappedValue.storedAs = ""
        } else {
            v.wrappedValue.secret = true
            v.wrappedValue.storedAs = ""
        }
    }

    @ViewBuilder private func secretCaption(_ v: Binding<EnvVarRow>) -> some View {
        let row = v.wrappedValue
        let info = storedInfo(row.storeName)
        let others = (info?.usedBy ?? []).filter { $0 != editing }
        let fromFile = review?.refs.first { $0.key == row.key && $0.secret == row.storeName }
        HStack(spacing: 4) {
            Image(systemName: "key.fill").foregroundStyle(.tertiary)
            if row.linked {
                Text("stored as \(row.storeName)").font(.system(size: 10, design: .monospaced))
                if let fromFile {
                    Text("· 1Password, from \((fromFile.file.map { ($0 as NSString).lastPathComponent }) ?? "an env file")")
                } else if let l = info?.sourceLabel {
                    Text("· \(l)")
                }
                if info?.isSet == false { Text("· not set").foregroundStyle(.orange) }
                if !others.isEmpty { Text("· also used by \(others.joined(separator: ", "))") }
            } else {
                Text("store as")
                TextField(row.storeName.isEmpty ? "NAME" : row.storeName, text: v.storedAs)
                    .font(.system(size: 10, design: .monospaced))
                    .textFieldStyle(.roundedBorder)
                    .controlSize(.mini)
                    .frame(width: 170)
                    .onChange(of: row.storedAs) { v.wrappedValue.storedAs = SecretsSettings.sanitize(v.wrappedValue.storedAs) }
                if let info {
                    Text(row.value.isEmpty
                         ? "· uses the stored \(info.name)\(others.isEmpty ? "" : " (also used by \(others.joined(separator: ", ")))")"
                         : "· replaces the stored \(info.name)\(others.isEmpty ? "" : " for \(others.joined(separator: ", ")) too")")
                        .foregroundStyle(row.value.isEmpty || others.isEmpty ? AnyShapeStyle(.secondary) : AnyShapeStyle(.orange))
                }
            }
            Spacer(minLength: 0)
        }
        .font(.system(size: 10))
        .foregroundStyle(.secondary)
        .lineLimit(1)
        .padding(.leading, 2)
    }

    @ViewBuilder private var onePasswordSection: some View {
        Section {
            if let r = review, !r.refs.isEmpty {
                ForEach(r.refs) { x in
                    HStack(spacing: 6) {
                        Text(x.key).font(.system(size: 12, design: .monospaced))
                        OPStatusBadge(status: x.status)
                        Spacer()
                        Text("\(x.vault ?? "?") · \(x.item ?? "?")").font(.caption).foregroundStyle(.secondary).lineLimit(1)
                    }
                }
                Button(r.actionable ? "Review & Pull from 1Password…" : "Review 1Password References…") {
                    if let editing { openWindow(id: "op-review", value: editing); bringToFront() }
                }
                .controlSize(.small)
            }
            HStack(spacing: 8) {
                Button("Link Reference…") { opSheet = .link }
                Button("Import Environment…") { opSheet = .environment }
                Spacer()
            }
            .controlSize(.small)
            .disabled(isNew || store.secrets?.locked == true)
        } header: {
            Text("1Password")
        } footer: {
            Text(isNew
                 ? "op:// references in the env files above can be pulled once the service is added. Link a single reference or import a 1Password Environment after saving."
                 : "References from the saved env files. Link a single op:// reference, or import a mounted 1Password Environment, straight into \(editing ?? "this service"). Values are read only when you click, and the 1Password app asks you to approve.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    // MARK: footer + save

    private var footer: some View {
        HStack {
            if let error {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption).foregroundStyle(.red).lineLimit(3)
            } else if !isNew {
                Button("Open YAML") { store.revealConfig(editing) }
                    .buttonStyle(.link).controlSize(.small)
            }
            Spacer()
            if let s = svc, s.isRunning || s.isTransitioning {
                Toggle("Restart to apply", isOn: $restartAfter).toggleStyle(.checkbox).controlSize(.small)
            }
            Button("Cancel") { close() }
                .keyboardShortcut(.cancelAction)
            Button {
                Task { await save() }
            } label: {
                if saving { ProgressView().controlSize(.small) } else { Text(isNew ? "Add Service" : "Save") }
            }
            .keyboardShortcut(.defaultAction)
            .buttonStyle(.borderedProminent)
            .disabled(name.isEmpty || saving)
        }
        .padding(14)
    }

    private func close() {
        if isNew { dismissWindow(id: "add") } else { dismiss() }
    }

    private static func validKey(_ k: String) -> Bool {
        guard let f = k.unicodeScalars.first, f == "_" || (f.isASCII && CharacterSet.letters.contains(f)) else { return false }
        return k.unicodeScalars.allSatisfy { $0 == "_" || ($0.isASCII && CharacterSet.alphanumerics.contains($0)) }
    }

    private func validateVars() throws {
        var seen = Set<String>()
        for v in vars {
            let k = v.key.trimmingCharacters(in: .whitespaces)
            if k.isEmpty { throw EditorError("Every variable needs a name.") }
            guard Self.validKey(k) else { throw EditorError("\(k) isn't a valid name (letters, digits and _, not starting with a digit).") }
            if !seen.insert(k).inserted { throw EditorError("\(k) is listed twice.") }
            guard v.secret else { continue }
            let n = v.storeName
            if n.isEmpty || !Self.validKey(n) { throw EditorError("\(k): \(n.isEmpty ? "the secret needs a name" : "\(n) isn't a valid secret name").") }
            if n.hasPrefix("LSTACK_") { throw EditorError("\(k): names starting with LSTACK_ are reserved.") }
            if v.value.isEmpty, !v.linked, storedInfo(n) == nil { throw EditorError("\(k): enter the secret's value.") }
        }
        if store.secrets?.locked == true, vars.contains(where: { $0.secret && !$0.value.isEmpty }) {
            throw EditorError("The secret store is locked — unlock it in Settings › Secrets first.")
        }
    }

    private func buildConfig() throws -> ServiceConfig {
        try validateVars()
        var cfg = original ?? ServiceConfig(name: name.trimmingCharacters(in: .whitespaces), type: type)
        cfg.description = description.isEmpty ? nil : description
        cfg.autostart = autostart
        cfg.restart = restart
        switch type {
        case "process":
            cfg.command = command
            cfg.cwd = cwd.isEmpty ? nil : cwd
            let files = list(sources) ?? []
            cfg.source = files == defaultSource ? nil : files
        case "docker":
            cfg.image = image.isEmpty ? nil : image
            cfg.container = container.isEmpty ? nil : container
            cfg.ports = list(ports)
        default:
            cfg.composeFile = composeFile
            cfg.project = project.isEmpty ? nil : project
        }
        let plain = vars.filter { !$0.secret }
        cfg.env = plain.isEmpty ? nil : Dictionary(plain.map { ($0.key.trimmingCharacters(in: .whitespaces), $0.value) }, uniquingKeysWith: { _, b in b })
        cfg.secrets = SecretRefs.make(vars.filter(\.secret).map { ($0.key.trimmingCharacters(in: .whitespaces), $0.storeName) }, asMap: original?.secrets?.isMap ?? false)
        cfg.envFiles = envFiles.isEmpty ? nil : envFiles
        switch healthKind {
        case "http": cfg.healthcheck = HealthCheck(type: "http", url: healthURL)
        case "tcp":
            guard let p = Int(healthPort) else { throw EditorError("The health check port must be a number.") }
            cfg.healthcheck = HealthCheck(type: "tcp", port: p)
        case "none": cfg.healthcheck = nil
        default: break
        }
        return cfg
    }

    /// Only what changed, so fields this editor doesn't know about are left alone. `null` removes a field.
    private func patch(from old: ServiceConfig, to new: ServiceConfig) throws -> [String: Any] {
        let enc = JSONEncoder()
        let a = try JSONSerialization.jsonObject(with: enc.encode(old)) as? [String: Any] ?? [:]
        let b = try JSONSerialization.jsonObject(with: enc.encode(new)) as? [String: Any] ?? [:]
        func canon(_ v: Any?) -> Data? {
            v.flatMap { try? JSONSerialization.data(withJSONObject: $0, options: [.sortedKeys, .fragmentsAllowed]) }
        }
        var p: [String: Any] = [:]
        for k in Set(a.keys).union(b.keys) where k != "name" && canon(a[k]) != canon(b[k]) {
            p[k] = b[k] ?? NSNull()
        }
        return p
    }

    private func save() async {
        error = nil
        let cfg: ServiceConfig
        do { cfg = try buildConfig() } catch { self.error = error.localizedDescription; return }
        saving = true
        defer { saving = false }
        for i in vars.indices where vars[i].secret && !vars[i].value.isEmpty {
            if let e = await store.setSecret(vars[i].storeName, vars[i].value) {
                error = "\(vars[i].key): \(e)"
                return
            }
            vars[i].value = ""
            vars[i].storedAs = vars[i].storeName
            vars[i].linked = true
        }
        do {
            if let original, let editing {
                let p = try patch(from: original, to: cfg)
                if !p.isEmpty, let e = await store.patchService(editing, p) { throw EditorError(e) }
                if restartAfter, let s = svc, s.isRunning || s.isTransitioning { store.perform(s, "restart") }
                await store.refreshSecrets()
                dismiss()
            } else {
                try await store.create(cfg, start: startNow)
                await store.refreshSecrets()
                dismissWindow(id: "add")
                if cfg.envFiles != nil, (detected?.files ?? []).contains(where: { (cfg.envFiles ?? []).contains($0.name) && !($0.refs ?? []).isEmpty }) {
                    offerReview(cfg.name)
                }
            }
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func offerReview(_ service: String) {
        let alert = NSAlert()
        alert.messageText = "Review 1Password references for \(service)?"
        alert.informativeText = "The env files contain op:// references. Nothing has been read from 1Password yet; review them and pull the values when you're ready."
        alert.addButton(withTitle: "Review…")
        alert.addButton(withTitle: "Later")
        if alert.runModal() == .alertFirstButtonReturn {
            openWindow(id: "op-review", value: service)
            bringToFront()
        }
    }
}
