import AppKit
import SwiftUI

struct AddServiceView: View {
    @Environment(AppStore.self) private var store
    @Environment(\.dismissWindow) private var dismissWindow

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
    @State private var env = ""
    @State private var sources = "~/.zshrc"
    @State private var defaultSource = ["~/.zshrc"]
    @State private var suggestion: CommandSplit?
    @State private var restart = "on-failure"
    @State private var autostart = true
    @State private var startNow = true
    @State private var healthKind = "none"
    @State private var healthURL = "http://localhost:3000/health"
    @State private var healthPort = ""
    @State private var error: String?
    @State private var saving = false

    init(command: String = "") {
        _command = State(initialValue: command)
    }

    var body: some View {
        VStack(spacing: 0) {
            Form {
                Section {
                    Picker("Type", selection: $type) {
                        Label("Script", systemImage: "terminal.fill").tag("process")
                        Label("Docker", systemImage: "shippingbox.fill").tag("docker")
                        Label("Compose", systemImage: "square.stack.3d.up.fill").tag("compose")
                    }
                    .pickerStyle(.segmented)
                    TextField("Name", text: $name, prompt: Text("api-server"))
                    TextField("Description", text: $description, prompt: Text("optional"))
                }
                Section(type == "process" ? "Command" : type == "docker" ? "Container" : "Compose project") {
                    switch type {
                    case "process":
                        TextField("Command", text: $command, prompt: Text("npm run dev"))
                            .font(.system(.body, design: .monospaced))
                        if let s = suggestion, s.changed {
                            splitHint(s)
                        }
                        pathField("Working directory", $cwd, directories: true)
                    case "docker":
                        TextField("Image", text: $image, prompt: Text("postgres:16"))
                        TextField("Existing container", text: $container, prompt: Text("optional — manage an existing container instead"))
                        TextField("Ports", text: $ports, prompt: Text("5432:5432, 8080:80"))
                    default:
                        pathField("Compose file", $composeFile, directories: false)
                        TextField("Project name", text: $project, prompt: Text("optional"))
                    }
                    if type != "compose" {
                        TextField("Environment", text: $env, prompt: Text("KEY=value, OTHER=value"), axis: .vertical)
                            .lineLimit(1...4)
                    }
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
                    }
                    if healthKind == "http" { TextField("URL", text: $healthURL) }
                    if healthKind == "tcp" { TextField("Port", text: $healthPort) }
                    Toggle("Start now", isOn: $startNow)
                }
            }
            .formStyle(.grouped)

            HStack {
                if let error {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .font(.caption).foregroundStyle(.red).lineLimit(2)
                }
                Spacer()
                Button("Cancel") { dismissWindow(id: "add") }
                    .keyboardShortcut(.cancelAction)
                Button {
                    Task { await save() }
                } label: {
                    if saving { ProgressView().controlSize(.small) } else { Text("Add Service") }
                }
                .keyboardShortcut(.defaultAction)
                .buttonStyle(.borderedProminent)
                .disabled(name.isEmpty || saving)
            }
            .padding(14)
        }
        .frame(width: 480)
        .fixedSize(horizontal: false, vertical: true)
        .task {
            if let s = await store.splitCommand("", cwd: cwd) {
                if sources == defaultSource.joined(separator: ", ") { sources = s.defaultSource.joined(separator: ", ") }
                defaultSource = s.defaultSource
            }
        }
        .task(id: command) {
            guard type == "process", !command.isEmpty else { suggestion = nil; return }
            try? await Task.sleep(for: .milliseconds(350))
            guard !Task.isCancelled else { return }
            suggestion = await store.splitCommand(command, cwd: cwd)
        }
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
        if !p.env.isEmpty {
            let lines = p.env.keys.sorted().map { "\($0)=\(p.env[$0]!)" }
            env = ([env.trimmingCharacters(in: .whitespacesAndNewlines)].filter { !$0.isEmpty } + lines).joined(separator: "\n")
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

    private func save() async {
        error = nil
        var cfg = ServiceConfig(name: name.trimmingCharacters(in: .whitespaces), type: type)
        cfg.description = description.isEmpty ? nil : description
        cfg.autostart = autostart
        cfg.restart = restart
        switch type {
        case "process":
            cfg.command = command
            cfg.cwd = cwd
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
        if let pairs = list(env) {
            cfg.env = Dictionary(pairs.compactMap { p -> (String, String)? in
                guard let eq = p.firstIndex(of: "=") else { return nil }
                return (String(p[..<eq]), String(p[p.index(after: eq)...]))
            }, uniquingKeysWith: { _, b in b })
        }
        if healthKind == "http" { cfg.healthcheck = HealthCheck(type: "http", url: healthURL) }
        if healthKind == "tcp", let port = Int(healthPort) { cfg.healthcheck = HealthCheck(type: "tcp", port: port) }

        saving = true
        defer { saving = false }
        do {
            try await store.create(cfg, start: startNow)
            dismissWindow(id: "add")
        } catch {
            self.error = error.localizedDescription
        }
    }
}
