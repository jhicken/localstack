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
    var system: SystemInfo?
    /// This Mac's busy processes (over the thresholds in Settings).
    var processes: [ProcInfo] = []
    /// Other Macs this one watches.
    var machines: [Machine] = []
    var localName: String?
    /// Set when cmux refused to open a workspace; the popover offers the fix or Terminal instead.
    var cmuxDenied: CmuxDenial?

    private var client: DaemonClient?
    private var toastTask: Task<Void, Never>?
    private static let historyLimit = 200
    private static let systemLimit = 300

    // MARK: live views

    /// Windows on screen that show live load (the menu bar panel, log windows). The panel's view tree
    /// stays alive after it closes, so applying a sample every few seconds would re-render it (charts
    /// and all) unseen. While nothing is showing, samples are parked here and applied on the next open.
    @ObservationIgnored private var liveViews = Set<ObjectIdentifier>()
    @ObservationIgnored private var parked = Parked()
    private var isLive: Bool { !liveViews.isEmpty || ProcessInfo.processInfo.environment["LSTACK_SNAPSHOT"] != nil }

    private struct Parked {
        /// History key (`name` or `name@machine`) → samples not yet applied.
        var metrics: [String: (machine: String?, name: String, samples: [MetricSample])] = [:]
        /// Keyed by machine name; "" is this Mac.
        var system: [String: [SystemSample]] = [:]
        var processes: [String: [ProcInfo]] = [:]
    }

    func setLive(_ id: ObjectIdentifier, _ on: Bool) {
        let was = isLive
        if on { liveViews.insert(id) } else { liveViews.remove(id) }
        if isLive, !was { flushParked() }
    }

    private func flushParked() {
        let p = parked
        parked = Parked()
        for m in p.metrics.values { applyMetrics(machine: m.machine, name: m.name, m.samples) }
        for (machine, samples) in p.system { applySystem(machine: machine.isEmpty ? nil : machine, samples) }
        for (machine, list) in p.processes { applyProcesses(machine: machine.isEmpty ? nil : machine, list) }
    }

    private func park(metrics s: MetricSample, name: String, machine: String?) {
        let key = machine.map { "\(name)@\($0)" } ?? name
        var e = parked.metrics[key] ?? (machine, name, [])
        e.samples.append(s)
        if e.samples.count > Self.historyLimit { e.samples.removeFirst(e.samples.count - Self.historyLimit) }
        parked.metrics[key] = e
    }

    private func park(system s: SystemSample, machine: String?) {
        var list = parked.system[machine ?? ""] ?? []
        list.append(s)
        if list.count > Self.systemLimit { list.removeFirst(list.count - Self.systemLimit) }
        parked.system[machine ?? ""] = list
    }

    private func applyMetrics(machine: String?, name: String, _ samples: [MetricSample]) {
        guard let last = samples.last else { return }
        let key: String
        if let machine {
            guard let i = machines.firstIndex(where: { $0.name == machine }) else { return }
            if let j = machines[i].services?.firstIndex(where: { $0.name == name }) { machines[i].services?[j].metrics = last }
            key = "\(name)@\(machine)"
        } else {
            if let i = services.firstIndex(where: { $0.name == name }) { services[i].metrics = last }
            key = name
        }
        var h = history[key] ?? []
        let after = h.last?.ts ?? -.infinity
        h.append(contentsOf: samples.filter { $0.ts > after })
        if h.count > Self.historyLimit { h.removeFirst(h.count - Self.historyLimit) }
        history[key] = h
    }

    private func applySystem(machine: String?, _ samples: [SystemSample]) {
        if let machine {
            guard let i = machines.firstIndex(where: { $0.name == machine }) else { return }
            var sys = machines[i].system ?? MachineSystem()
            var list = sys.samples ?? []
            let after = list.last?.ts ?? -.infinity
            list.append(contentsOf: samples.filter { $0.ts > after })
            if list.count > Self.systemLimit { list.removeFirst(list.count - Self.systemLimit) }
            sys.samples = list
            machines[i].system = sys
        } else {
            guard system != nil else { return }
            let after = system!.samples.last?.ts ?? -.infinity
            system!.samples.append(contentsOf: samples.filter { $0.ts > after })
            if system!.samples.count > Self.systemLimit { system!.samples.removeFirst(system!.samples.count - Self.systemLimit) }
        }
    }

    private func applyProcesses(machine: String?, _ list: [ProcInfo]) {
        if let machine {
            if let i = machines.firstIndex(where: { $0.name == machine }) { machines[i].processes = list }
        } else {
            processes = list
        }
    }

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
            if let m = snap.machine { localName = m }
            for s in services where s.isRunning { Task { await loadHistory(s) } }
            Task {
                await loadSystem()
                await refreshSettings()
                await refreshRemote()
                await loadProcesses()
            }
        case "processes":
            guard let e = try? dec.decode(ProcessesEvent.self, from: data) else { return }
            if isLive { processes = e.processes } else { parked.processes[""] = e.processes }
        case "machines":
            guard let e = try? dec.decode(MachinesEvent.self, from: data) else { return }
            localName = e.local
            machines = e.machines.map { var m = $0; m.services = m.services?.sorted { $0.name < $1.name }; return m }
        case "peer":
            guard let e = try? dec.decode(PeerEvent.self, from: data) else { return }
            handlePeer(e.machine, e.event, e.data.data)
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
            parked.metrics[e.name] = nil
        case "metrics":
            guard let e = try? dec.decode(MetricsEvent.self, from: data) else { return }
            if isLive { applyMetrics(machine: nil, name: e.name, [e.metrics]) } else { park(metrics: e.metrics, name: e.name, machine: nil) }
        case "system":
            guard let s = try? dec.decode(SystemSample.self, from: data) else { return }
            if isLive { applySystem(machine: nil, [s]) } else { park(system: s, machine: nil) }
        default:
            break
        }
    }

    private func handlePeer(_ name: String, _ event: String, _ data: Data) {
        let dec = JSONDecoder()
        if event == "removed-machine" {
            machines.removeAll { $0.name == name }
            return
        }
        if event == "machine" {
            guard var m = try? dec.decode(Machine.self, from: data) else { return }
            if let i = machines.firstIndex(where: { $0.name == name }) {
                m.services = machines[i].services
                m.processes = machines[i].processes
                m.configErrors = machines[i].configErrors
                if m.system == nil { m.system = machines[i].system }
                machines[i] = m
            } else {
                machines.append(m)
                machines.sort { $0.name < $1.name }
            }
            return
        }
        guard let i = machines.firstIndex(where: { $0.name == name }) else { return }
        switch event {
        case "snapshot":
            guard let snap = try? dec.decode(Snapshot.self, from: data) else { return }
            machines[i].services = snap.services.sorted { $0.name < $1.name }
            machines[i].configErrors = snap.configErrors
        case "service":
            guard var s = try? dec.decode(ServiceState.self, from: data) else { return }
            var list = machines[i].services ?? []
            if let j = list.firstIndex(where: { $0.name == s.name }) {
                if s.metrics == nil, s.isRunning { s.metrics = list[j].metrics }
                list[j] = s
            } else {
                list.append(s)
                list.sort { $0.name < $1.name }
            }
            machines[i].services = list
        case "removed":
            guard let e = try? dec.decode(NameEvent.self, from: data) else { return }
            machines[i].services?.removeAll { $0.name == e.name }
        case "metrics":
            guard let e = try? dec.decode(MetricsEvent.self, from: data) else { return }
            if isLive { applyMetrics(machine: name, name: e.name, [e.metrics]) } else { park(metrics: e.metrics, name: e.name, machine: name) }
        case "processes":
            guard let e = try? dec.decode(ProcessesEvent.self, from: data) else { return }
            if isLive { machines[i].processes = e.processes } else { parked.processes[name] = e.processes }
        case "system":
            guard let x = try? dec.decode(SystemSample.self, from: data) else { return }
            if isLive { applySystem(machine: name, [x]) } else { park(system: x, machine: name) }
        default:
            break
        }
    }

    /// Every service on every machine (for counts in the menu bar).
    var allServices: [ServiceState] { services + machines.flatMap { $0.services ?? [] } }

    /// API path of a service on this Mac or on a paired machine.
    func path(_ s: ServiceState) -> String { Self.path(name: s.name, machine: s.machine) }

    static func path(name: String, machine: String?) -> String {
        if let machine { return "/api/machines/\(machine.urlPath)/services/\(name.urlPath)" }
        return "/api/services/\(name.urlPath)"
    }

    /// `name` or `name@machine` (the logs window's id).
    static func split(_ id: String) -> (name: String, machine: String?) {
        guard let at = id.lastIndex(of: "@"), at != id.startIndex, id.index(after: at) != id.endIndex else { return (id, nil) }
        return (String(id[..<at]), String(id[id.index(after: at)...]))
    }

    func service(id: String) -> ServiceState? { allServices.first { $0.id == id } }

    func machine(_ name: String?) -> Machine? { name.flatMap { n in machines.first { $0.name == n } } }

    /// Whether start/stop/restart are possible (always on this Mac; per the other Mac's setting otherwise).
    func canControl(_ s: ServiceState) -> Bool {
        guard let m = machine(s.machine) else { return true }
        return m.online && m.canControl
    }

    func loadProcesses() async {
        struct R: Decodable { let processes: [ProcInfo] }
        if let r = try? await client?.send("GET", "/api/processes", as: R.self) { processes = r.processes }
    }

    func loadSystem() async {
        if let s = try? await client?.send("GET", "/api/system", as: SystemInfo.self) { system = s }
    }

    func loadHistory(_ s: ServiceState) async {
        struct R: Decodable { let samples: [MetricSample] }
        guard let client, let r = try? await client.send("GET", "\(path(s))/metrics?range=15m&points=120", as: R.self) else { return }
        history[s.id] = r.samples
    }

    // MARK: actions

    func perform(_ name: String, _ action: String) {
        if let s = service(id: name) { perform(s, action); return }
        Task {
            do { try await client?.action(name, action) } catch { flash(error.localizedDescription) }
        }
    }

    func perform(_ s: ServiceState, _ action: String) {
        Task {
            do { try await client?.send("POST", "\(path(s))/\(action)", as: ServiceState.self) } catch { flash(error.localizedDescription) }
        }
    }

    /// Open a service URL in the default browser.
    func open(_ u: ServiceURL) {
        if let url = URL(string: u.url) { NSWorkspace.shared.open(url) }
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

    func splitCommand(_ command: String, cwd: String) async -> CommandSplit? {
        try? await client?.send("POST", "/api/split-command", body: ["command": command, "cwd": cwd], as: CommandSplit.self)
    }

    func openTerminal(_ name: String, terminal: String? = nil) {
        openInTerminal("/api/services/\(name.urlPath)/open-terminal", terminal: terminal)
    }

    /// The terminal dashboard (`lstack top`) in the preferred terminal app.
    func openDashboard(terminal: String? = nil) {
        openInTerminal("/api/open-dashboard", terminal: terminal)
    }

    private func openInTerminal(_ path: String, terminal: String?) {
        Task {
            do {
                let q = terminal.map { "?terminal=\($0)" } ?? ""
                let r = try await client?.send("POST", path + q, as: OpenResult.self)
                if r?.cmuxDenied == true {
                    cmuxDenied = CmuxDenial(path: path, message: r?.error ?? "cmux refused the connection")
                } else if r?.ok == false {
                    flash(r?.error ?? "Couldn't open the terminal")
                } else {
                    cmuxDenied = nil
                }
            } catch { flash(error.localizedDescription) }
        }
    }

    /// The web dashboard (remote access) in the browser.
    func openWeb() {
        guard let u = remote?.urls.first, let url = URL(string: u) else { flash("Turn on the web dashboard in Settings first"); return }
        NSWorkspace.shared.open(url)
    }

    var webAvailable: Bool { remote?.running == true && !(remote?.urls.isEmpty ?? true) }

    /// Switch cmux to password mode for LStack, then retry what was refused.
    func fixCmux(retry: Bool = true) async -> String? {
        guard let client else { return DaemonError.notRunning.localizedDescription }
        do {
            let r = try await client.send("POST", "/api/terminal/cmux/enable-password", as: CmuxFixResult.self)
            await refreshSettings()
            let pending = cmuxDenied
            cmuxDenied = nil
            if retry, let pending {
                try? await Task.sleep(for: .milliseconds(600))
                openInTerminal(pending.path, terminal: nil)
            }
            if let note = r.note { flash(note) }
            return nil
        } catch { return error.localizedDescription }
    }

    /// Retry what cmux refused in Terminal instead.
    func useTerminalInstead() {
        guard let pending = cmuxDenied else { return }
        cmuxDenied = nil
        openInTerminal(pending.path, terminal: "terminal")
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
        if let d = setup?.daemon, d.running, d.stale == true, !holdingForDiskAccess {
            daemonOutdated = d.staleReason ?? "LStack was updated after the background service started"
        } else {
            daemonOutdated = nil
        }
    }

    /// Set when the running daemon is older than this app; offers a restart.
    var daemonOutdated: String?
    var restartingDaemon = false

    func restartDaemon() {
        Task { await restartDaemonNow() }
    }

    /// Restarts the daemon; scripts that were running are started again by the new one.
    private func restartDaemonNow() async {
        restartingDaemon = true
        let r = await Helper.run(["daemon", "restart"])
        restartingDaemon = false
        if r.status != 0 { flash(r.output.isEmpty ? "Couldn't restart the daemon" : r.output.trimmingCharacters(in: .whitespacesAndNewlines)) }
        await refreshSetup()
    }

    /// Ask (once per launch) whether to restart an out-of-date daemon. "Later" leaves a banner in the menu.
    private func offerDaemonRestart() {
        guard let why = daemonOutdated else { return }
        let alert = NSAlert()
        alert.messageText = "Restart the LStack background service?"
        alert.informativeText = "\(why.prefix(1).uppercased() + why.dropFirst()), so new features and process info may not work until it restarts.\n\nScripts LStack started will stop and autostart ones start again. Containers keep running."
        alert.addButton(withTitle: "Restart Now")
        alert.addButton(withTitle: "Later")
        bringToFront()
        if alert.runModal() == .alertFirstButtonReturn { restartDaemon() }
    }

    /// Called once at launch. Returns true if the setup window should be shown.
    func launch() async -> Bool {
        guard Helper.url != nil, ProcessInfo.processInfo.environment["LSTACK_SNAPSHOT"] == nil else { return false }
        // Without Full Disk Access a restarted daemon would start scripts that can't reach other apps' data, so
        // leave whatever daemon is running (even an older one) and its services alone until access is granted.
        holdingForDiskAccess = onboarded && !Self.hasDiskAccess() && !diskAccessDismissed
        if onboarded {
            // Keep login items / CLI link pointing at this copy of the app (it may have moved or been updated).
            _ = await Helper.run(["setup", "repair", "--quiet"] + (holdingForDiskAccess ? ["--keep-daemon"] : []))
        }
        await refreshSetup()
        if setup?.daemon.running == false { startDaemon() }
        if holdingForDiskAccess {
            watchDiskAccess()
        } else if onboarded {
            offerDaemonRestart()
        }
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

    /// Begin two-factor enrollment. Returns the secret + otpauth URI to show as a QR code.
    func totpBegin() async throws -> TotpSetup {
        guard let client else { throw DaemonError.notRunning }
        return try await client.send("POST", "/api/security/totp", as: TotpSetup.self)
    }

    /// Returns an error message, or nil once the code is accepted and 2FA is on.
    func totpConfirm(_ code: String) async -> String? {
        guard let client else { return DaemonError.notRunning.localizedDescription }
        do {
            remote = try await client.send("POST", "/api/security/totp/confirm", body: ["code": code], as: RemoteStatus.self)
            return nil
        } catch { return error.localizedDescription }
    }

    func totpDisable() async -> String? {
        guard let client else { return DaemonError.notRunning.localizedDescription }
        do {
            remote = try await client.send("DELETE", "/api/security/totp", as: RemoteStatus.self)
            return nil
        } catch { return error.localizedDescription }
    }

    // MARK: daemon settings

    var settings: DaemonSettings?

    func refreshSettings() async {
        guard let client else { settings = nil; return }
        settings = try? await client.send("GET", "/api/settings", as: DaemonSettings.self)
    }

    // MARK: Full Disk Access

    var diskAccessDismissed = UserDefaults.standard.bool(forKey: "diskAccessDismissed") {
        didSet {
            UserDefaults.standard.set(diskAccessDismissed, forKey: "diskAccessDismissed")
            // The user doesn't want to grant it: stop holding back the daemon update.
            if diskAccessDismissed, holdingForDiskAccess {
                holdingForDiskAccess = false
                diskAccessPoll?.cancel()
                Task { await refreshSetup(); offerDaemonRestart() }
            }
        }
    }
    /// LStack (the app, and so the daemon inside it) has Full Disk Access. Checked from the app because
    /// the running daemon may be an older build that can't report it.
    var hasDiskAccess = AppStore.hasDiskAccess()
    /// A daemon restart/update is waiting for Full Disk Access (running services are left alone until then).
    var holdingForDiskAccess = false
    private var diskAccessPoll: Task<Void, Never>?

    /// Files only readable with Full Disk Access (same probe as the daemon's).
    static func hasDiskAccess() -> Bool {
        let home = NSHomeDirectory()
        for p in ["/Library/Application Support/com.apple.TCC/TCC.db",
                  home + "/Library/Application Support/com.apple.TCC/TCC.db",
                  home + "/Library/Safari/Bookmarks.plist"] {
            let fd = Darwin.open(p, O_RDONLY)
            if fd >= 0 { Darwin.close(fd); return true }
            if errno != ENOENT { return false }
        }
        return false
    }

    /// Wait for Full Disk Access to be granted (System Settings may be opened outside LStack), then restart.
    private func watchDiskAccess() {
        diskAccessPoll?.cancel()
        diskAccessPoll = Task {
            while !Task.isCancelled {
                if Self.hasDiskAccess() {
                    await diskAccessGranted()
                    return
                }
                try? await Task.sleep(for: .seconds(holdingForDiskAccess ? 3 : 2))
            }
        }
    }

    /// Now that services can get Full Disk Access, restart them: finish a held-back daemon update (the new
    /// daemon starts the scripts that were running again), or else restart the running scripts.
    private func diskAccessGranted() async {
        hasDiskAccess = true
        let held = holdingForDiskAccess
        holdingForDiskAccess = false
        let pid = setup?.daemon.pid
        if held { _ = await Helper.run(["setup", "repair", "--quiet"]) }
        await refreshSetup()
        if daemonOutdated != nil {
            await restartDaemonNow()
            flash("Full Disk Access granted — restarted the background service and its scripts")
        } else if setup?.daemon.pid != nil, setup?.daemon.pid != pid {
            flash("Full Disk Access granted — restarted the background service and its scripts")
        } else {
            let scripts = services.filter { $0.type == "process" && $0.isRunning }
            for s in scripts { perform(s.name, "restart") }
            if !scripts.isEmpty { flash("Full Disk Access granted — restarted \(scripts.count) script\(scripts.count == 1 ? "" : "s")") }
        }
        await refreshSettings()
    }

    /// macOS has no prompt for Full Disk Access, so take the user straight to the pane, then watch for
    /// the grant. Granting LStack.app covers the daemon too (it runs from inside the app bundle), so
    /// there's no separate helper to add.
    func requestDiskAccess() {
        // Touching a protected file makes macOS list LStack in the pane (switched off), ready to enable.
        _ = FileManager.default.isReadableFile(atPath: NSHomeDirectory() + "/Library/Safari/Bookmarks.plist")
        _ = try? FileHandle(forReadingFrom: URL(fileURLWithPath: "/Library/Application Support/com.apple.TCC/TCC.db")).close()
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles") {
            NSWorkspace.shared.open(url)
        }
        watchDiskAccess()
    }

    func setTerminal(_ id: String) {
        Task {
            do {
                settings = try await client?.send("PUT", "/api/settings", body: ["terminal": id], as: DaemonSettings.self)
            } catch { flash(error.localizedDescription) }
        }
    }

    // MARK: secrets (write-only: values go in, never come back out)

    var secrets: SecretsStatus?

    func refreshSecrets() async {
        secrets = try? await client?.send("GET", "/api/secrets", as: SecretsStatus.self)
    }

    func setSecret(_ name: String, _ value: String) async -> String? {
        guard let client else { return DaemonError.notRunning.localizedDescription }
        do {
            try await client.send("PUT", "/api/secrets/\(name.urlPath)", body: ["value": value])
            await refreshSecrets()
            return nil
        } catch { return error.localizedDescription }
    }

    func removeSecret(_ name: String) async -> String? {
        guard let client else { return DaemonError.notRunning.localizedDescription }
        do {
            try await client.send("DELETE", "/api/secrets/\(name.urlPath)")
            await refreshSecrets()
            return nil
        } catch { return error.localizedDescription }
    }

    func unlockSecrets() async -> String? {
        guard let client else { return DaemonError.notRunning.localizedDescription }
        do {
            secrets = try await client.send("POST", "/api/secrets/unlock", as: SecretsStatus.self)
            return nil
        } catch { return error.localizedDescription }
    }

    /// Which secrets a service receives (env var name = secret name).
    func setServiceSecrets(_ name: String, _ names: [String]) async -> String? {
        let e = await patchService(name, ["secrets": names.isEmpty ? NSNull() : names])
        await refreshSecrets()
        return e
    }

    // MARK: settings patches (heartbeats, process thresholds)

    /// Send a free-form JSON object (NSNull clears a key). Throws DaemonError.http on failure.
    private func sendJSON(_ method: String, _ path: String, _ object: [String: Any]) async throws -> Data {
        guard let client else { throw DaemonError.notRunning }
        var req = client.request(method, path)
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try JSONSerialization.data(withJSONObject: object)
        let (data, resp) = try await URLSession.shared.data(for: req)
        let code = (resp as? HTTPURLResponse)?.statusCode ?? 0
        if code >= 400 {
            throw DaemonError.http(code, (try? JSONDecoder().decode([String: String].self, from: data))?["error"] ?? "")
        }
        return data
    }

    /// PUT /api/settings with a partial JSON object. Returns an error message, or nil.
    func updateSettings(_ patch: [String: Any]) async -> String? {
        do {
            let data = try await sendJSON("PUT", "/api/settings", patch)
            settings = try? JSONDecoder().decode(DaemonSettings.self, from: data)
            if settings == nil { await refreshSettings() }
            return nil
        } catch { return error.localizedDescription }
    }

    /// PATCH a service definition (JSON merge patch; NSNull removes a key).
    func patchService(_ name: String, _ patch: [String: Any]) async -> String? {
        do {
            _ = try await sendJSON("PATCH", "/api/services/\(name.urlPath)", patch)
            return nil
        } catch { return error.localizedDescription }
    }

    func testHeartbeat(_ service: String? = nil) async -> String? {
        guard let client else { return DaemonError.notRunning.localizedDescription }
        do {
            let st = try await client.send("POST", "/api/heartbeat/test", body: service.map { ["name": $0] } ?? [:], as: HeartbeatStatus.self)
            await refreshSettings()
            return st.ok ? nil : (st.error ?? "ping failed")
        } catch { return error.localizedDescription }
    }

    // MARK: machines

    /// Pair with another Mac: its one-time code (+ its 2FA code if it uses two-factor authentication).
    func addMachine(address: String, code: String, totp: String?, name: String?) async -> Result<PairResult, Error> {
        guard let client else { return .failure(DaemonError.notRunning) }
        var body: [String: String] = ["address": address, "code": code]
        if let totp, !totp.isEmpty { body["totp"] = totp }
        if let name, !name.isEmpty { body["name"] = name }
        do {
            let r = try await client.send("POST", "/api/machines", body: body, as: PairResult.self)
            await refreshMachines()
            return .success(r)
        } catch { return .failure(error) }
    }

    func refreshMachines() async {
        struct List: Decodable { let machines: [Machine] }
        if let l = try? await client?.send("GET", "/api/machines", as: List.self) {
            machines = l.machines.sorted { $0.name < $1.name }
        }
    }

    func removeMachine(_ name: String) async -> String? {
        guard let client else { return DaemonError.notRunning.localizedDescription }
        do {
            try await client.send("DELETE", "/api/machines/\(name.urlPath)")
            machines.removeAll { $0.name == name }
            return nil
        } catch { return error.localizedDescription }
    }

    func renameMachine(_ name: String, to: String) async -> String? {
        guard let client else { return DaemonError.notRunning.localizedDescription }
        do {
            try await client.send("PATCH", "/api/machines/\(name.urlPath)", body: ["name": to], as: Machine.self)
            return nil
        } catch { return error.localizedDescription }
    }

    func pairCode() async throws -> PairCode {
        guard let client else { throw DaemonError.notRunning }
        return try await client.send("POST", "/api/remote/pair-code", as: PairCode.self)
    }

    func sharedPeers() async -> [SharedPeer] {
        (try? await client?.send("GET", "/api/remote/peers", as: [SharedPeer].self)) ?? []
    }

    func setGrants(_ id: String, _ grants: PeerGrants) async -> String? {
        guard let client else { return DaemonError.notRunning.localizedDescription }
        do {
            _ = try await client.send("PATCH", "/api/remote/peers/\(id.urlPath)", body: grants, as: [SharedPeer].self)
            return nil
        } catch { return error.localizedDescription }
    }

    func linkActivity() async -> LinkActivity? {
        try? await client?.send("GET", "/api/remote/activity", as: LinkActivity.self)
    }

    func linkIdentity() async -> LinkIdentity? {
        try? await client?.send("GET", "/api/identity", as: LinkIdentity.self)
    }

    func rekey() async -> String? {
        guard let client else { return DaemonError.notRunning.localizedDescription }
        do {
            _ = try await client.send("POST", "/api/identity/rekey", as: LinkIdentity.self)
            return nil
        } catch { return error.localizedDescription }
    }

    func forwards() async -> [ForwardInfo] {
        (try? await client?.send("GET", "/api/forwards", as: [ForwardInfo].self)) ?? []
    }

    func setForwardPaused(_ f: ForwardInfo, _ paused: Bool) async -> String? {
        guard let client else { return DaemonError.notRunning.localizedDescription }
        do {
            _ = try await client.send("POST", "/api/forwards/\(paused ? "pause" : "resume")", body: ["machine": f.machine, "service": f.service], as: [ForwardInfo].self)
            return nil
        } catch { return error.localizedDescription }
    }

    func revokePeer(_ id: String) async -> String? {
        guard let client else { return DaemonError.notRunning.localizedDescription }
        do {
            try await client.send("DELETE", "/api/remote/peers/\(id.urlPath)")
            return nil
        } catch { return error.localizedDescription }
    }

    func flash(_ msg: String) {
        toast = msg
        toastTask?.cancel()
        toastTask = Task {
            try? await Task.sleep(for: .seconds(4))
            if !Task.isCancelled { toast = nil }
        }
    }

    func logStream(_ id: String) -> AsyncThrowingStream<(String, Data), Error>? {
        let (name, machine) = Self.split(id)
        return client?.events("\(Self.path(name: name, machine: machine))/logs/stream?tail=1000")
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

struct CmuxDenial: Equatable {
    /// The open-terminal / open-dashboard request to retry.
    let path: String
    let message: String
}
