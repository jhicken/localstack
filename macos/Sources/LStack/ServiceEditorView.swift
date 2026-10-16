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

/// One line of the service's env files in the editor, with the change queued for Save.
struct FileVarRow: Identifiable, Hashable {
    let id = UUID()
    var key: String
    var file: String
    /// plain, op (op://), lstack (lstack://NAME), or secret (added here: a value to store).
    var kind: String
    /// Plain value; for secret/lstack rows a value to store ("" keeps what's stored).
    var value: String = ""
    var name: String? = nil
    var ref: String? = nil
    var status: String? = nil
    var secret: String? = nil
    var shadowed = false
    /// As read from the file (nil: added here).
    var orig: EnvPreview.Var? = nil
    /// Convert to Secret on Save.
    var convert = false
    var removed = false

    init(key: String, file: String, kind: String, name: String? = nil) {
        self.key = key
        self.file = file
        self.kind = kind
        self.name = name
    }

    init(_ v: EnvPreview.Var) {
        key = v.key
        file = v.file
        kind = v.kind
        value = v.kind == "plain" ? (v.template ?? v.value ?? "") : ""
        name = v.name
        ref = v.ref
        status = v.status
        secret = v.secret
        shadowed = v.shadowed == true
        orig = v
    }

    var isNew: Bool { orig == nil }
    var trimmedKey: String { key.trimmingCharacters(in: .whitespaces) }
    var dirty: Bool {
        guard let o = orig else { return true }
        return removed || convert || trimmedKey != o.key || (kind == "plain" && value != (o.template ?? o.value ?? "")) || (kind != "plain" && !value.isEmpty)
    }

    /// What Save sends to `POST /api/services/:name/env`.
    var ops: [[String: Any]] {
        var out: [[String: Any]] = []
        let k = trimmedKey
        if let o = orig, removed || k != o.key { out.append(["op": "remove", "file": o.file, "key": o.key]) }
        if removed { return out }
        let moved = orig?.key != k
        switch kind {
        case "plain":
            if convert { out.append(["op": "secret", "file": file, "key": k, "value": value]) }
            else if moved || value != (orig?.template ?? orig?.value ?? "") { out.append(["op": "set", "file": file, "key": k, "value": value]) }
        case "secret":
            out.append(["op": "secret", "file": file, "key": k, "value": value])
        case "lstack":
            if !value.isEmpty, let n = name { out.append(["op": "secret", "file": file, "key": k, "value": value, "name": n]) }
            else if moved, let n = name { out.append(["op": "link", "file": file, "key": k, "name": n]) }
        default:
            if moved, let ref = orig?.template ?? ref { out.append(["op": "set", "file": file, "key": k, "value": ref]) }
        }
        return out
    }
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
    @State private var preview: EnvPreview?
    @State private var previewError: String?
    @State private var previewTick = 0
    @State private var fileRows: [FileVarRow] = []
    @State private var addTarget = defaultEnvFile
    @State private var showOverrides = false

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
        .task(id: previewKey) {
            guard loaded else { return }
            try? await Task.sleep(for: .milliseconds(250))
            guard !Task.isCancelled else { return }
            await loadPreview()
        }
        .onChange(of: svc?.config.secrets) { mergeExternalSecrets() }
        .onChange(of: svc?.secretsPending) { Task { await loadReview() } }
    }

    private var envTabTitle: String {
        let n = fileRows.filter { !$0.removed && !$0.shadowed }.count + overrideRows.count
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
        showOverrides = !overrideRows.isEmpty && envFiles.isEmpty
        await loadReview()
        await loadPreview()
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
        let rows = fileRows.filter { !$0.removed && !$0.shadowed }
        let plain = rows.filter { $0.kind == "plain" && !$0.convert }.count + overrideRows.filter { !$0.secret }.count
        let secret = rows.count - rows.filter { $0.kind == "plain" && !$0.convert }.count + overrideRows.filter(\.secret).count
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
            HStack(spacing: 12) {
                if detectDir != nil, !envFileChoices.contains(defaultEnvFile) {
                    Button { envFilesTouched = true; envFiles.append(defaultEnvFile) } label: { Label("New \(defaultEnvFile)", systemImage: "doc.badge.plus") }
                }
                Button("Add Other File…", action: chooseEnvFile)
                Spacer()
            }
            .buttonStyle(.borderless).controlSize(.small)
        } header: {
            Text("Env files")
        } footer: {
            footnote("Use one or several (read like `op run --env-file`: later files win). Values are read at each start; when a file changes while the service runs, LStack tells you a restart will apply it.")
        }
        variablesSection
        if !overrideRows.isEmpty { overridesSection }
    }

    /// Section footers, left-aligned and wrapping inside the window.
    private func footnote(_ s: LocalizedStringKey) -> some View {
        Text(s)
            .font(.caption).foregroundStyle(.secondary)
            .multilineTextAlignment(.leading)
            .frame(maxWidth: .infinity, alignment: .leading)
            .fixedSize(horizontal: false, vertical: true)
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
                Text(d.map(fileDetail) ?? (envFiles.contains(f) ? "created on Save" : "not in this folder"))
                    .font(.caption).foregroundStyle(d?.error != nil || (d == nil && !envFiles.contains(f)) ? AnyShapeStyle(.orange) : AnyShapeStyle(.secondary))
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
        if d.isMount { return "1Password Environment mount — import it with `lstack secret import --environment`" }
        if let e = d.error { return e }
        var parts: [String] = []
        if let p = d.plain, !p.isEmpty { parts.append(p.joined(separator: ", ")) }
        if let r = d.stored, !r.isEmpty { parts.append("stored: " + r.map(\.key).joined(separator: ", ")) }
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

    /// Stored secrets that can be pointed at from the env file.
    private var linkableSecrets: [SecretInfo] {
        let used = Set(fileRows.filter { !$0.removed }.compactMap(\.name))
        return storedSecrets.filter { !used.contains($0.name) && $0.isSet }
    }

    /// Where new variables go: the folder's .env.lstack when it's used, else the last file (it wins).
    private var defaultTarget: String {
        if envFiles.contains(defaultEnvFile) { return defaultEnvFile }
        return envFiles.last(where: { f in preview?.files.first { $0.name == f }?.state != "fifo" }) ?? defaultEnvFile
    }

    private var writableFiles: [String] {
        envFiles.filter { f in preview?.files.first { $0.name == f }?.state != "fifo" }
    }

    /// Changes to keep through a refresh of the file contents.
    private var previewKey: String {
        [detectDir ?? "", envFiles.joined(separator: "\u{1}"), (svc?.envFileKeys ?? []).joined(separator: ","),
         svc?.secretsPending.map { "\($0.count)\($0.link ?? [])\($0.notStored ?? [])" } ?? "",
         svc?.config.secrets.map { "\($0.pairs)" } ?? "", "\(previewTick)"].joined(separator: "|")
    }

    private func loadPreview() async {
        guard let dir = detectDir ?? (isNew ? nil : original?.cwd), !envFiles.isEmpty else {
            preview = nil
            fileRows.removeAll { !$0.isNew }
            return
        }
        do {
            let p = try await store.envPreview(service: isNew ? nil : editing, dir: isNew ? dir : detectDir, files: envFiles)
            merge(p)
            previewError = nil
        } catch {
            previewError = error.localizedDescription
        }
    }

    /// Show what's in the files now, keeping rows edited here (and dropping those whose line is gone).
    private func merge(_ p: EnvPreview) {
        preview = p
        let edited = fileRows.filter(\.dirty)
        var next: [FileVarRow] = []
        for v in p.vars {
            if var e = edited.first(where: { $0.orig?.key == v.key && $0.orig?.file == v.file }) {
                e.status = v.status
                e.shadowed = v.shadowed == true
                next.append(e)
            } else {
                next.append(FileVarRow(v))
            }
        }
        next += edited.filter(\.isNew)
        fileRows = next
    }

    private var variablesSection: some View {
        Section {
            if let previewError {
                Label(previewError, systemImage: "exclamationmark.triangle.fill").font(.caption).foregroundStyle(.orange)
            }
            if envFiles.isEmpty {
                Text("No env file selected. Variables you add go into \(defaultEnvFile) in the service's folder (created on Save).")
                    .font(.callout).foregroundStyle(.secondary)
            } else if fileRows.isEmpty {
                Text("No variables in \(envFiles.joined(separator: ", ")) yet.").font(.callout).foregroundStyle(.secondary)
            }
            if let p = svc?.secretsPending, let link = p.link, !link.isEmpty, !isNew {
                HStack(spacing: 8) {
                    Image(systemName: "hand.raised.fill").foregroundStyle(.orange)
                    Text("\(link.joined(separator: ", ")) \(link.count == 1 ? "points" : "point") at stored secrets. Approve to pass \(link.count == 1 ? "it" : "them") to \(editing ?? "the service").")
                        .font(.caption).fixedSize(horizontal: false, vertical: true)
                    Spacer()
                    Button("Approve All") { approve(nil) }.controlSize(.small)
                }
            }
            ForEach($fileRows) { $r in
                if !r.removed { fileRow($r) }
            }
            HStack(spacing: 12) {
                Button { addRow(FileVarRow(key: "", file: defaultTarget, kind: "plain")) } label: { Label("Add Variable", systemImage: "plus") }
                Button { addRow(FileVarRow(key: "", file: defaultTarget, kind: "secret")) } label: { Label("Add Secret", systemImage: "lock.fill") }
                    .disabled(store.secrets?.locked == true)
                Menu {
                    ForEach(linkableSecrets) { s in
                        Button("\(s.name)\(s.usedBy.isEmpty ? "" : " — used by \(s.usedBy.joined(separator: ", "))")") {
                            addRow(FileVarRow(key: s.name, file: defaultTarget, kind: "lstack", name: s.name))
                        }
                    }
                } label: { Label("Use Stored Secret", systemImage: "key.fill") }
                    .fixedSize()
                    .disabled(linkableSecrets.isEmpty || store.secrets?.locked == true)
                    .help("Point a variable at a secret stored in LStack (e.g. one another service uses)")
                Spacer(minLength: 0)
            }
            .buttonStyle(.borderless)
            .controlSize(.small)
            if writableFiles.count > 1 {
                Picker("New variables go into", selection: $addTarget) {
                    ForEach(writableFiles, id: \.self) { Text($0).tag($0) }
                }
                .pickerStyle(.menu)
                .controlSize(.small)
                .onAppear { if !writableFiles.contains(addTarget) { addTarget = defaultTarget } }
            }
            if let r = review, r.actionable, !isNew {
                HStack(spacing: 8) {
                    Image(systemName: "exclamationmark.circle.fill").foregroundStyle(.orange)
                    Text("op:// references to pull from 1Password").font(.caption)
                    Spacer(minLength: 0)
                    Button("Review & Pull…") {
                        if let editing { openWindow(id: "op-review", value: editing); bringToFront() }
                    }
                    .controlSize(.small)
                }
            }
        } header: {
            Text("Environment variables")
        } footer: {
            footnote("Everything in the env files above, saved back to them. `lstack://NAME` points at a secret stored in LStack: it's passed in as an environment variable and scrubbed from logs, and can't be read back. Convert to Secret (the lock) moves a value into the store and leaves the reference in the file. A `lstack://` line typed into the file by hand waits for your approval.\n\n`op://vault/item/field` lines come from 1Password: LStack pulls them once you review them (the 1Password app asks you to approve) and keeps a cached copy, so restarts never need 1Password.")
        }
    }

    private func addRow(_ r: FileVarRow) {
        var r = r
        if envFiles.isEmpty {
            envFilesTouched = true
            envFiles = [defaultEnvFile]
            r.file = defaultEnvFile
        } else if writableFiles.count > 1, writableFiles.contains(addTarget) {
            r.file = addTarget
        }
        fileRows.append(r)
    }

    private func fileRow(_ v: Binding<FileVarRow>) -> some View {
        let r = v.wrappedValue
        return VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 6) {
                TextField("NAME", text: v.key)
                    .font(.system(size: 12, design: .monospaced))
                    .frame(width: 170)
                    .disabled(r.shadowed)
                switch r.kind {
                case "plain":
                    TextField("value", text: v.value).font(.system(size: 12, design: .monospaced)).disabled(r.shadowed)
                    Button { v.wrappedValue.convert.toggle() } label: {
                        Image(systemName: r.convert ? "lock.fill" : "lock.open").foregroundStyle(r.convert ? Color.accentColor : .secondary)
                    }
                    .buttonStyle(.borderless)
                    .disabled(r.shadowed || store.secrets?.locked == true)
                    .help(r.convert ? "Will be stored as a secret on Save — click to keep it plain" : "Convert to Secret: store this value in LStack and hide it")
                case "secret":
                    SecureField("Secret value", text: v.value)
                case "lstack":
                    SecureField(r.status == "notStored" ? "not stored — enter its value" : "•••••• stored — type to replace", text: v.value)
                        .disabled(r.shadowed || store.secrets?.locked == true)
                default:
                    Text(r.ref ?? "").font(.system(size: 11, design: .monospaced)).foregroundStyle(.secondary)
                        .lineLimit(1).truncationMode(.middle)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .help(r.ref ?? "")
                }
                Button {
                    if r.isNew { fileRows.removeAll { $0.id == r.id } } else { v.wrappedValue.removed = true }
                } label: { Image(systemName: "minus.circle.fill").foregroundStyle(.secondary) }
                    .buttonStyle(.borderless)
                    .help("Remove \(r.key.isEmpty ? "this variable" : r.key) from \(r.file)")
            }
            .textFieldStyle(.roundedBorder)
            .labelsHidden()
            rowCaption(r)
        }
        .opacity(r.shadowed ? 0.55 : 1)
    }

    @ViewBuilder private func rowCaption(_ r: FileVarRow) -> some View {
        let multi = envFiles.count > 1
        let others = r.name.map { n in (storedInfo(n)?.usedBy ?? []).filter { $0 != editing } } ?? []
        let bits: [String] = {
            var b: [String] = []
            if multi || r.isNew { b.append(r.file) }
            if r.shadowed { b.append("overridden by a later file") }
            switch r.kind {
            case "plain" where r.convert: b.append("stored as a secret on Save; the file will say lstack://…")
            case "secret": b.append(r.isNew ? "stored in LStack on Save" : "")
            case "lstack":
                if let n = r.name { b.append("stored as \(n)") }
                if !r.value.isEmpty { b.append(r.status == "notStored" ? "stored on Save" : "replaces the stored value on Save\(others.isEmpty ? "" : " — for \(others.joined(separator: ", ")) too")") }
                else if !others.isEmpty { b.append("also used by \(others.joined(separator: ", "))") }
            case "op":
                if let s = r.secret, r.status == "ok" { b.append("1Password, cached as \(s)") } else { b.append("1Password") }
            default: break
            }
            return b.filter { !$0.isEmpty }
        }()
        if !bits.isEmpty || (r.status != nil && r.status != "ok") || r.kind == "lstack" {
            HStack(spacing: 4) {
                if r.kind == "lstack" || r.kind == "op" || r.kind == "secret" || r.convert {
                    Image(systemName: "key.fill").foregroundStyle(.tertiary)
                }
                Text(bits.joined(separator: " · ")).lineLimit(1).truncationMode(.middle)
                if let st = r.status, !r.shadowed, r.kind != "plain" {
                    OPStatusBadge(status: r.kind == "lstack" && (st == "new" || st == "changed") ? "approve" : st)
                }
                Spacer(minLength: 0)
                if r.kind == "lstack", r.status == "new" || r.status == "changed", !isNew, !r.shadowed {
                    Button("Approve") { approve([r.key]) }.controlSize(.mini)
                        .help("Pass \(r.name ?? "this secret") to \(editing ?? "the service") as \(r.key)")
                }
            }
            .font(.system(size: 10))
            .foregroundStyle(.secondary)
            .padding(.leading, 2)
        }
    }

    private func approve(_ keys: [String]?) {
        guard let editing else { return }
        Task {
            do { merge(try await store.envApprove(editing, keys: keys)) } catch { self.error = error.localizedDescription }
        }
    }

    /// `secrets:` keys the env files manage (shown in the list above, not as overrides).
    private var fileManagedKeys: Set<String> {
        guard let p = preview else { return [] }
        let overrides = Set(p.overrides.secrets.map(\.key))
        return Set((original?.secrets?.pairs ?? []).map(\.key)).subtracting(overrides)
    }

    private var overrideRows: [EnvVarRow] {
        vars.filter { !($0.secret && $0.linked && fileManagedKeys.contains($0.key)) }
    }

    private var overridesSection: some View {
        Section {
            DisclosureGroup(isExpanded: $showOverrides) {
                ForEach($vars) { $v in
                    if !(v.secret && v.linked && fileManagedKeys.contains(v.key)) { varRow($v) }
                }
            } label: {
                Text("Set in the service definition (\(overrideRows.count))")
            }
        } footer: {
            footnote("These come from the service's YAML (`env:` and `secrets:`), not an env file — for example `FOO=bar` typed before the command. Env file values win over `env:`; secrets win over both. Moving them into an env file keeps everything in one place.")
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
        var inFile = Set<String>()
        for r in fileRows where !r.removed && !r.shadowed {
            let k = r.trimmedKey
            if k.isEmpty { throw EditorError("Every variable needs a name.") }
            guard Self.validKey(k) else { throw EditorError("\(k) isn't a valid name (letters, digits and _, not starting with a digit).") }
            if !inFile.insert("\(r.file)\u{1}\(k)").inserted { throw EditorError("\(k) is listed twice in \(r.file).") }
            if r.kind == "secret", r.value.isEmpty { throw EditorError("\(k): enter the secret's value.") }
        }
        if store.secrets?.locked == true, fileRows.contains(where: { !$0.removed && ($0.convert || $0.kind == "secret" || ($0.kind == "lstack" && $0.dirty)) }) {
            throw EditorError("The secret store is locked — unlock it in Settings › Secrets first.")
        }
    }

    /// Queued env file edits; rows aimed at a file that's no longer selected go to the default one.
    private var fileOps: [[String: Any]] {
        fileRows.flatMap { r -> [[String: Any]] in
            var r = r
            if r.isNew, !envFiles.contains(r.file) { r.file = defaultTarget }
            return r.ops
        }
    }

    /// Selected env files that don't exist yet (e.g. a new .env.lstack): created empty, owner-only.
    private func createMissingFiles() {
        for f in envFiles {
            guard let url = envFileURL(f), !FileManager.default.fileExists(atPath: url.path) else { continue }
            FileManager.default.createFile(atPath: url.path, contents: Data(), attributes: [.posixPermissions: 0o600])
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
        let ops = fileOps
        do {
            if let original, let editing {
                let p = try patch(from: original, to: cfg)
                if !p.isEmpty, let e = await store.patchService(editing, p) { throw EditorError(e) }
                createMissingFiles()
                if !ops.isEmpty {
                    do { merge(try await store.envApply(editing, ops)) } catch { throw EditorError("Env file: \(error.localizedDescription)") }
                }
                if restartAfter, let s = svc, s.isRunning || s.isTransitioning { store.perform(s, "restart") }
                await store.refreshSecrets()
                dismiss()
            } else {
                // Created stopped, so it starts with its env files already written.
                try await store.create(cfg, start: false)
                createMissingFiles()
                if !ops.isEmpty {
                    do { _ = try await store.envApply(cfg.name, ops) } catch { throw EditorError("\(cfg.name) was added, but its env file wasn't saved: \(error.localizedDescription)") }
                }
                if startNow { store.perform(cfg.name, "start") }
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
