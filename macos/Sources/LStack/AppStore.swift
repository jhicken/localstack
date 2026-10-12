import AppKit
import Foundation
import Observation

@MainActor
@Observable
final class AppStore {
    var services: [ServiceState] = []
    var history: [String: [MetricSample]] = [:]
    var connected = false
    var everConnected = false
    var configErrors: [String] = []
    var toast: String?
    var startingDaemon = false
    var expanded: String?

    private var client: DaemonClient?
    private var toastTask: Task<Void, Never>?
    private static let historyLimit = 200

    init() {
        Task { await eventLoop() }
    }

    // MARK: derived

    var running: [ServiceState] { services.filter(\.isRunning) }
    var failedCount: Int { services.filter(\.isFailed).count }
    var totalCPU: Double { running.reduce(0) { $0 + ($1.metrics?.cpu ?? 0) } }
    var totalMem: Double { running.reduce(0) { $0 + ($1.metrics?.mem ?? 0) } }

    // MARK: event stream

    private func eventLoop() async {
        while !Task.isCancelled {
            do {
                let c = try DaemonClient.load()
                client = c
                for try await (event, data) in c.events("/api/events") {
                    connected = true
                    everConnected = true
                    handle(event, data)
                }
            } catch {}
            connected = false
            try? await Task.sleep(for: .seconds(2))
        }
    }

    private func handle(_ event: String, _ data: Data) {
        let dec = JSONDecoder()
        switch event {
        case "snapshot":
            guard let snap = try? dec.decode(Snapshot.self, from: data) else { return }
            services = snap.services.sorted { $0.name < $1.name }
            configErrors = snap.configErrors
            for s in services where s.isRunning { Task { await loadHistory(s.name) } }
        case "service":
            guard let s = try? dec.decode(ServiceState.self, from: data) else { return }
            if let i = services.firstIndex(where: { $0.name == s.name }) {
                var next = s
                if next.metrics == nil, next.isRunning { next.metrics = services[i].metrics }
                services[i] = next
            } else {
                services.append(s)
                services.sort { $0.name < $1.name }
            }
        case "removed":
            guard let e = try? dec.decode(NameEvent.self, from: data) else { return }
            services.removeAll { $0.name == e.name }
            history[e.name] = nil
        case "metrics":
            guard let e = try? dec.decode(MetricsEvent.self, from: data) else { return }
            if let i = services.firstIndex(where: { $0.name == e.name }) { services[i].metrics = e.metrics }
            var h = history[e.name] ?? []
            h.append(e.metrics)
            if h.count > Self.historyLimit { h.removeFirst(h.count - Self.historyLimit) }
            history[e.name] = h
        default:
            break
        }
    }

    func loadHistory(_ name: String) async {
        guard let client, let samples = try? await client.metrics(name, range: "15m", points: 120) else { return }
        history[name] = samples
    }

    // MARK: actions

    func perform(_ name: String, _ action: String) {
        Task {
            do { try await client?.action(name, action) } catch { flash(error.localizedDescription) }
        }
    }

    func setAutostart(_ name: String, _ on: Bool) {
        Task {
            do {
                try await client?.send("PATCH", "/api/services/\(name.urlPath)", body: ["autostart": on], as: ServiceState.self)
            } catch { flash(error.localizedDescription) }
        }
    }

    func remove(_ name: String) {
        Task {
            do { try await client?.send("DELETE", "/api/services/\(name.urlPath)") } catch { flash(error.localizedDescription) }
        }
    }

    func create(_ cfg: ServiceConfig, start: Bool) async throws {
        guard let client else { throw DaemonError.notRunning }
        try await client.send("POST", "/api/services\(start ? "?start=1" : "")", body: cfg, as: ServiceState.self)
    }

    func openTerminal(_ name: String) {
        Task { try? await client?.send("POST", "/api/services/\(name.urlPath)/open-terminal") }
    }

    func openDashboard() {
        Task { try? await client?.send("POST", "/api/open-dashboard") }
    }

    func revealConfig(_ name: String? = nil) {
        let dir = DaemonClient.home.appendingPathComponent("services")
        if let name {
            let file = dir.appendingPathComponent("\(name).yaml")
            NSWorkspace.shared.open(file)
        } else {
            NSWorkspace.shared.open(dir)
        }
    }

    /// Start the daemon with the bundled helper (launchd kickstart if the login item is installed).
    func startDaemon() {
        startingDaemon = true
        Task {
            let r = await Helper.run(["daemon", "start"])
            startingDaemon = false
            if r.status != 0 { flash(r.output.isEmpty ? "Couldn't start the daemon" : r.output.trimmingCharacters(in: .whitespacesAndNewlines)) }
        }
    }

    // MARK: setup / self-install

    var setup: SetupStatus?
    var setupBusy = false
    static let onboardedKey = "onboarded"
    var onboarded: Bool {
        get { UserDefaults.standard.bool(forKey: Self.onboardedKey) }
        set { UserDefaults.standard.set(newValue, forKey: Self.onboardedKey) }
    }

    func refreshSetup() async {
        setup = await Helper.status()
    }

    /// Called once at launch. Returns true if the setup window should be shown.
    func launch() async -> Bool {
        guard Helper.url != nil, ProcessInfo.processInfo.environment["LSTACK_SNAPSHOT"] == nil else { return false }
        if onboarded {
            // Keep login items / CLI link pointing at this copy of the app (it may have moved or been updated).
            _ = await Helper.run(["setup", "repair", "--quiet"])
        }
        await refreshSetup()
        if setup?.daemon.running == false { startDaemon() }
        return !onboarded
    }

    /// Run `lstack setup …` and refresh status. Returns an error message on failure.
    @discardableResult
    func runSetup(_ args: [String]) async -> String? {
        setupBusy = true
        defer { setupBusy = false }
        let r = await Helper.run(["setup"] + args)
        await refreshSetup()
        return r.status == 0 ? nil : r.output.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    // MARK: remote dashboard

    var remote: RemoteStatus?

    func refreshRemote() async {
        guard let client else { remote = nil; return }
        remote = try? await client.send("GET", "/api/remote", as: RemoteStatus.self)
    }

    /// Returns an error message, or nil on success.
    func updateRemote(_ patch: RemotePatch) async -> String? {
        guard let client else { return DaemonError.notRunning.localizedDescription }
        do {
            remote = try await client.send("PUT", "/api/remote", body: patch, as: RemoteStatus.self)
            return nil
        } catch {
            await refreshRemote()
            return error.localizedDescription
        }
    }

    func flash(_ msg: String) {
        toast = msg
        toastTask?.cancel()
        toastTask = Task {
            try? await Task.sleep(for: .seconds(4))
            if !Task.isCancelled { toast = nil }
        }
    }

    func logStream(_ name: String) -> AsyncThrowingStream<(String, Data), Error>? {
        client?.events("/api/services/\(name.urlPath)/logs/stream?tail=1000")
    }

    func sendInput(_ name: String, _ text: String) {
        Task {
            do {
                try await client?.send("POST", "/api/services/\(name.urlPath)/stdin", body: ["data": text + "\n"])
            } catch { flash(error.localizedDescription) }
        }
    }

    func clearLogs(_ name: String) {
        Task { try? await client?.send("DELETE", "/api/services/\(name.urlPath)/logs") }
    }
}
