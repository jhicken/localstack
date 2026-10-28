import SwiftUI

/// The log window keeps a bounded window of lines: it opens with the newest few hundred and
/// pages older ones in from the daemon's log file as you scroll up.
enum LogPaging {
    static let initial = 300
    static let page = 500
    static let maxLines = 4000
}

struct LogView: View {
    @Environment(AppStore.self) private var store
    let name: String

    @State private var lines: [IndexedLine] = []
    @State private var filter = ""
    @State private var follow = true
    @State private var input = ""
    @State private var streamError: String?
    @State private var counter = 0
    @State private var oldest = 0
    @State private var reachedStart = false
    @State private var loadingOlder = false
    @State private var olderError: String?
    /// The newest lines were dropped to make room for older ones (or new output was paused while you
    /// read back), so live lines aren't appended until you jump back to the latest.
    @State private var detached = false
    @State private var generation = 0
    /// Whether the end of the log is on screen (tracked with a sentinel row); only then does Follow scroll.
    @State private var atBottom = true

    struct IndexedLine: Identifiable {
        let id: Int
        let line: LogLine
        let text: String
    }

    /// `name` = this Mac's service; `name@machine` = a paired Mac's.
    private var service: ServiceState? { store.service(id: name) }
    private var isRemote: Bool { AppStore.split(name).machine != nil }
    private var visible: [IndexedLine] {
        filter.isEmpty ? lines : lines.filter { $0.text.localizedCaseInsensitiveContains(filter) }
    }

    var body: some View {
        VStack(spacing: 0) {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 1) {
                        olderRow(proxy)
                        ForEach(visible) { l in
                            row(l).id(l.id)
                        }
                        Color.clear.frame(height: 1)
                            .onAppear { atBottom = true }
                            .onDisappear { atBottom = false }
                    }
                    .padding(10)
                    .textSelection(.enabled)
                }
                .background(Color(nsColor: .textBackgroundColor).opacity(0.6))
                .onChange(of: lines.last?.id) {
                    if follow, atBottom, let last = visible.last { proxy.scrollTo(last.id, anchor: .bottom) }
                }
                .onChange(of: follow) {
                    if follow, detached { generation += 1; return }
                    if follow, let last = visible.last { proxy.scrollTo(last.id, anchor: .bottom) }
                }
            }
            .overlay(alignment: .bottomTrailing) {
                if detached {
                    Button { follow = true; generation += 1 } label: {
                        Label("Paused — Jump to Latest", systemImage: "arrow.down.to.line")
                    }
                    .buttonStyle(.borderedProminent).controlSize(.small).padding(12)
                }
            }
            if service?.type == "process", !isRemote {
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
                    Group {
                        if s.isRunning {
                            Button { store.perform(s, "stop") } label: { Label("Stop", systemImage: "stop.fill") }
                        } else {
                            Button { store.perform(s, "start") } label: { Label("Start", systemImage: "play.fill") }
                        }
                        Button { store.perform(s, "restart") } label: { Label("Restart", systemImage: "arrow.clockwise") }
                    }
                    .disabled(!store.canControl(s))
                    if !isRemote {
                        Button { store.openTerminal(name) } label: { Label("Terminal", systemImage: "apple.terminal") }
                            .disabled(!s.isRunning)
                    }
                }
                if !isRemote {
                    Button {
                        store.clearLogs(name)
                        lines = []
                        reachedStart = true
                    } label: { Label("Clear", systemImage: "trash") }
                }
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
        .background(LiveWindowReporter(store: store))
        .task(id: "\(name)#\(generation)") { await stream() }
    }

    private var hasOlder: Bool { !reachedStart && lines.first?.line.pos != nil }

    @ViewBuilder
    private func olderRow(_ proxy: ScrollViewProxy) -> some View {
        if hasOlder {
            HStack(spacing: 6) {
                if loadingOlder {
                    ProgressView().controlSize(.small)
                    Text("Loading older lines…")
                } else if let e = olderError {
                    Image(systemName: "exclamationmark.triangle").foregroundStyle(.orange)
                    Text(e).lineLimit(1)
                    Button("Retry") { Task { await loadOlder(proxy) } }.buttonStyle(.link)
                } else {
                    Button("Load Older Lines") { Task { await loadOlder(proxy) } }.buttonStyle(.link)
                }
            }
            .font(.system(size: 11))
            .foregroundStyle(.secondary)
            .frame(maxWidth: .infinity)
            .padding(.vertical, 4)
            // Scrolling to the top pulls in the previous page (not while filtering, so a filter with
            // few matches doesn't page through the whole history on its own).
            .onAppear { if filter.isEmpty, olderError == nil, !atBottom { Task { await loadOlder(proxy) } } }
        } else if !lines.isEmpty, reachedStart {
            Text("Beginning of log").font(.system(size: 11)).foregroundStyle(.tertiary)
                .frame(maxWidth: .infinity).padding(.vertical, 4)
        }
    }

    private func loadOlder(_ proxy: ScrollViewProxy) async {
        guard !loadingOlder, let before = lines.first?.line.pos else { return }
        let anchor = visible.first?.id
        loadingOlder = true
        defer { loadingOlder = false }
        do {
            let page = try await store.olderLogs(name, before: before)
            olderError = nil
            guard lines.first?.line.pos == before else { return } // reloaded meanwhile
            if page.count < LogPaging.page { reachedStart = true }
            guard !page.isEmpty else { return }
            var older: [IndexedLine] = []
            older.reserveCapacity(page.count)
            for l in page.reversed() {
                oldest -= 1
                older.append(IndexedLine(id: oldest, line: l, text: Self.stripAnsi(l.line)))
            }
            lines.insert(contentsOf: older.reversed(), at: 0)
            if lines.count > LogPaging.maxLines {
                lines.removeLast(lines.count - LogPaging.maxLines)
                detached = true
                follow = false
            }
            if let anchor {
                try? await Task.sleep(for: .milliseconds(10))
                proxy.scrollTo(anchor, anchor: .top)
            }
        } catch {
            if !Task.isCancelled { olderError = error.localizedDescription }
        }
    }

    private func append(_ l: LogLine) {
        guard !detached else { return }
        counter += 1
        lines.append(IndexedLine(id: counter, line: l, text: Self.stripAnsi(l.line)))
        guard lines.count > LogPaging.maxLines else { return }
        if follow, atBottom {
            lines.removeFirst(lines.count - LogPaging.maxLines)
            reachedStart = false
        } else {
            // You're reading back: keep what's on screen and stop appending until you jump back.
            lines.removeLast()
            detached = true
        }
    }

    /// Red only for errors. Many programs write INFO lines to stderr, so a level tag wins over the stream.
    private func color(for l: IndexedLine) -> AnyShapeStyle {
        if l.line.stream == "system" { return AnyShapeStyle(.cyan) }
        let text = l.text
        let head = text.prefix(80).uppercased()
        if ["ERROR", "FATAL", "CRITICAL", "PANIC"].contains(where: head.contains) { return AnyShapeStyle(.red) }
        if head.contains("WARN") { return AnyShapeStyle(.orange) }
        if ["INFO", "DEBUG", "TRACE", "NOTICE"].contains(where: head.contains) { return AnyShapeStyle(.primary) }
        if l.line.stream == "stderr", ["ERROR", "EXCEPTION", "TRACEBACK", "FATAL", "FAILED", "PANIC"].contains(where: text.uppercased().contains) { return AnyShapeStyle(.red) }
        return AnyShapeStyle(.primary)
    }

    private func row(_ l: IndexedLine) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text(Date(timeIntervalSince1970: l.line.ts / 1000), format: .dateTime.hour().minute().second())
                .foregroundStyle(.tertiary)
            Text(l.text)
                .foregroundStyle(color(for: l))
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .font(.system(size: 11.5, design: .monospaced))
    }

    private func stream() async {
        let decoder = JSONDecoder()
        while !Task.isCancelled {
            lines = []
            atBottom = true
            reachedStart = false
            detached = false
            olderError = nil
            guard let events = store.logStream(name) else {
                streamError = "daemon offline"
                try? await Task.sleep(for: .seconds(2))
                continue
            }
            do {
                streamError = nil
                for try await (_, data) in events {
                    guard let l = try? decoder.decode(LogLine.self, from: data) else { continue }
                    append(l)
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
