import SwiftUI

struct LogView: View {
    @Environment(AppStore.self) private var store
    let name: String

    @State private var lines: [IndexedLine] = []
    @State private var filter = ""
    @State private var follow = true
    @State private var input = ""
    @State private var streamError: String?
    @State private var counter = 0

    struct IndexedLine: Identifiable {
        let id: Int
        let line: LogLine
        let text: String
    }

    private var service: ServiceState? { store.services.first { $0.name == name } }
    private var visible: [IndexedLine] {
        filter.isEmpty ? lines : lines.filter { $0.text.localizedCaseInsensitiveContains(filter) }
    }

    var body: some View {
        VStack(spacing: 0) {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 1) {
                        ForEach(visible) { l in
                            row(l).id(l.id)
                        }
                    }
                    .padding(10)
                    .textSelection(.enabled)
                }
                .background(Color(nsColor: .textBackgroundColor).opacity(0.6))
                .onChange(of: lines.last?.id) {
                    if follow, let last = visible.last { proxy.scrollTo(last.id, anchor: .bottom) }
                }
                .onChange(of: follow) {
                    if follow, let last = visible.last { proxy.scrollTo(last.id, anchor: .bottom) }
                }
            }
            if service?.type == "process" {
                Divider()
                HStack(spacing: 8) {
                    Image(systemName: "chevron.right").foregroundStyle(.secondary)
                    TextField("Send input to stdin…", text: $input)
                        .textFieldStyle(.plain)
                        .font(.system(size: 12, design: .monospaced))
                        .onSubmit {
                            store.sendInput(name, input)
                            input = ""
                        }
                        .disabled(service?.isRunning != true)
                }
                .padding(10)
            }
        }
        .navigationTitle("\(name) — logs")
        .toolbar {
            ToolbarItemGroup(placement: .navigation) {
                if let s = service {
                    HStack(spacing: 6) {
                        StatusDot(status: s.status)
                        Text(s.status.capitalized).font(.system(size: 12, weight: .medium))
                        if s.isRunning {
                            Text("· \(Fmt.pct(s.metrics?.cpu)) · \(Fmt.bytes(s.metrics?.mem))")
                                .font(.system(size: 12)).foregroundStyle(.secondary).monospacedDigit()
                        }
                    }
                }
            }
            ToolbarItemGroup {
                if let s = service {
                    if s.isRunning {
                        Button { store.perform(name, "stop") } label: { Label("Stop", systemImage: "stop.fill") }
                    } else {
                        Button { store.perform(name, "start") } label: { Label("Start", systemImage: "play.fill") }
                    }
                    Button { store.perform(name, "restart") } label: { Label("Restart", systemImage: "arrow.clockwise") }
                    Button { store.openTerminal(name) } label: { Label("Terminal", systemImage: "apple.terminal") }
                        .disabled(!s.isRunning)
                }
                Button {
                    store.clearLogs(name)
                    lines = []
                } label: { Label("Clear", systemImage: "trash") }
                Toggle(isOn: $follow) { Label("Follow", systemImage: "arrow.down.to.line") }
                    .help("Auto-scroll to newest")
            }
        }
        .searchable(text: $filter, placement: .toolbar, prompt: "Filter")
        .overlay(alignment: .bottom) {
            if let e = streamError {
                Text(e).font(.caption).padding(6).background(.red.opacity(0.8), in: Capsule()).foregroundStyle(.white).padding()
            }
        }
        .task(id: name) { await stream() }
    }

    private func row(_ l: IndexedLine) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text(Date(timeIntervalSince1970: l.line.ts / 1000), format: .dateTime.hour().minute().second())
                .foregroundStyle(.tertiary)
            Text(l.text)
                .foregroundStyle(l.line.stream == "stderr" ? AnyShapeStyle(.red) : l.line.stream == "system" ? AnyShapeStyle(.cyan) : AnyShapeStyle(.primary))
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .font(.system(size: 11.5, design: .monospaced))
    }

    private func stream() async {
        let decoder = JSONDecoder()
        while !Task.isCancelled {
            lines = []
            guard let events = store.logStream(name) else {
                streamError = "daemon offline"
                try? await Task.sleep(for: .seconds(2))
                continue
            }
            do {
                streamError = nil
                for try await (_, data) in events {
                    guard let l = try? decoder.decode(LogLine.self, from: data) else { continue }
                    counter += 1
                    lines.append(IndexedLine(id: counter, line: l, text: Self.stripAnsi(l.line)))
                    if lines.count > 5000 { lines.removeFirst(lines.count - 5000) }
                }
            } catch {
                if Task.isCancelled { return }
                streamError = error.localizedDescription
            }
            try? await Task.sleep(for: .seconds(2))
        }
    }

    private static let ansi = try! NSRegularExpression(pattern: "\u{1B}\\[[0-9;?]*[A-Za-z]")
    static func stripAnsi(_ s: String) -> String {
        ansi.stringByReplacingMatches(in: s, range: NSRange(s.startIndex..., in: s), withTemplate: "")
    }
}
