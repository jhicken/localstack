import AppKit
import Foundation
import Network
import Observation
import UserNotifications

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
        watchNetwork()
        if Bundle.main.bundleURL.pathExtension == "app", ProcessInfo.processInfo.environment["LSTACK_SNAPSHOT"] == nil {
            notificationRouter.store = self
            UNUserNotificationCenter.current().delegate = notificationRouter
            NotificationRouter.registerCategories()
        }
    }

    // MARK: reconnecting

    @ObservationIgnored private var pathMonitor: NWPathMonitor?
    @ObservationIgnored private var lastPath: String?

    /// After sleep or a network change (Wi-Fi, a VPN going on or off), machines that are down are redialed now
    /// instead of after their backoff.
    private func watchNetwork() {
        guard ProcessInfo.processInfo.environment["LSTACK_SNAPSHOT"] == nil else { return }
        NSWorkspace.shared.notificationCenter.addObserver(forName: NSWorkspace.didWakeNotification, object: nil, queue: .main) { [weak self] _ in
            Task { @MainActor in self?.networkChanged() }
        }
        let m = NWPathMonitor()
        m.pathUpdateHandler = { [weak self] path in
            let key = "\(path.status)|" + path.availableInterfaces.map(\.name).joined(separator: ",")
            Task { @MainActor in
                guard let self else { return }
                let first = self.lastPath == nil
                if self.lastPath != key {
                    self.lastPath = key
                    if !first { self.networkChanged() }
                }
            }
        }
        m.start(queue: .global(qos: .utility))
        pathMonitor = m
    }

    private func networkChanged() {
        Task {
            // Give the new network a moment (DHCP, VPN routes) before dialing.
            try? await Task.sleep(for: .seconds(1))
            await reconnectMachines()
            await refreshNetwork()
        }
    }

    func reconnectMachines() async {
        _ = try? await client?.send("POST", "/api/machines/reconnect", as: EmptyResponse.self)
    }

    // MARK: notifications

    @ObservationIgnored private var notificationsAsked = false
    @ObservationIgnored private let notificationRouter = NotificationRouter()

    /// The daemon's notices (e.g. 1Password changes to review, values shared by a paired Mac).
    private func notify(title: String, body: String, review: String? = nil, category: String? = nil, info: [String: Any] = [:]) {
        // Notifications need a real app bundle (not `swift run`).
        guard Bundle.main.bundleIdentifier != nil, Bundle.main.bundleURL.pathExtension == "app" else {
            flash("\(title): \(body)")
            return
        }
        let center = UNUserNotificationCenter.current()
        let post = {
            let c = UNMutableNotificationContent()
            c.title = title
            c.body = body
            // Clicking opens that service's 1Password review (nothing is read until a button there is clicked).
            var userInfo = info
            if let review { userInfo["review"] = review }
            c.userInfo = userInfo
            // Approve / Accept buttons right on the notification.
            if let category { c.categoryIdentifier = category }
            center.add(UNNotificationRequest(identifier: UUID().uuidString, content: c, trigger: nil))
        }
        if notificationsAsked {
            post()
        } else {
            notificationsAsked = true
            center.requestAuthorization(options: [.alert, .sound]) { ok, _ in if ok { post() } }
        }
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
            Task { await registerApprovals() }
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
        case "notice":
            struct Ask: Decodable { let peer: String; let machine: String; let items: [Item]?
                struct Item: Decodable { let ref: String } }
            struct Pushed: Decodable { let machine: String; let refs: [String]? }
            struct Notice: Decodable { let title: String; let body: String; let approval: Approval?; let request: Ask?; let pushed: Pushed? }
            guard let n = try? dec.decode(Notice.self, from: data) else { return }
            if let a = n.approval {
                if let client { Approver.shared.enqueue(a, client) }
                return
            }
            // "<service>: review 1Password changes for KEY, …"
            var review: String?
            if n.title == "1Password changes to review", let colon = n.body.range(of: ": review ") {
                review = String(n.body[..<colon.lowerBound])
            }
            if let r = n.request {
                notify(title: n.title, body: n.body, category: NotificationRouter.request,
                       info: ["peer": r.peer, "machine": r.machine, "refs": (r.items ?? []).map(\.ref)])
            } else if let p = n.pushed {
                notify(title: n.title, body: n.body, category: NotificationRouter.pushed, info: ["machine": p.machine, "refs": p.refs ?? []])
            } else {
                notify(title: n.title, body: n.body, review: review)
            }
            Task { await refreshRemote() }
        case "service":
            guard let s = try? dec.decode(ServiceState.self, from: data) else { return }
            if let a = s.approval, let client { Approver.shared.enqueue(a, client) }
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

    /// Open a service URL in the default browser. A paused forward is resumed first and its local URL opened.
    func open(_ u: ServiceURL, in s: ServiceState? = nil) {
        guard u.blocked == "paused", let s, let machine = s.machine else {
            if let url = URL(string: u.url) { NSWorkspace.shared.open(url) }
            return
        }
        Task {
            guard let client else { return }
            do {
                let list = try await client.send("POST", "/api/forwards/resume", body: ["machine": machine, "service": s.name], as: [ForwardInfo].self)
                let f = list.first { $0.machine == machine && $0.service == s.name && $0.port == u.port }
                if let str = f?.url, let url = URL(string: str) {
                    NSWorkspace.shared.open(url)
                } else {
                    flash(f?.error ?? "Resumed forwarding \(s.name), but it isn't listening on this Mac yet")
                }
            } catch { flash(error.localizedDescription) }
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
    /// Touch ID approvals (nil until registered with the daemon).
    var approvals: ApprovalsStatus?
    var approvalProblem: String?

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

    // MARK: 1Password (the daemon never runs `op`; the app reads values after a click — see OnePassword.swift)

    /// A service whose 1Password review should open (from a notification or the menu); MenuBarLabel opens it.
    var reviewRequest: String?

    /// Services on this Mac with 1Password references waiting for review.
    var pendingReviews: [ServiceState] { services.filter { $0.secretsPending != nil } }

    func openReview(_ service: String) { reviewRequest = service }

    func opReview(_ service: String) async throws -> OPReview {
        guard let client else { throw DaemonError.notRunning }
        return try await client.send("GET", "/api/onepassword/review/\(service.urlPath)", as: OPReview.self)
    }

    /// Every variable in a service's env files (`files` overrides its list), or in a folder's.
    func envPreview(service: String?, dir: String?, files: [String]) async throws -> EnvPreview {
        var body: [String: Any] = ["files": files]
        if let service { body["service"] = service }
        if let dir { body["dir"] = dir }
        return try JSONDecoder().decode(EnvPreview.self, from: await sendJSON("POST", "/api/env/preview", body))
    }

    /// Edit a service's env files: `set`, `remove`, `secret` (store in LStack) or `link` ops.
    func envApply(_ service: String, _ ops: [[String: Any]]) async throws -> EnvPreview {
        try JSONDecoder().decode(EnvPreview.self, from: await sendJSON("POST", "/api/services/\(service.urlPath)/env", ["ops": ops]))
    }

    /// Approve `lstack://` lines waiting for review (nil: all of them).
    func envApprove(_ service: String, keys: [String]? = nil) async throws -> EnvPreview {
        var body: [String: Any] = [:]
        if let keys { body["keys"] = keys }
        return try JSONDecoder().decode(EnvPreview.self, from: await sendJSON("POST", "/api/services/\(service.urlPath)/env/approve", body))
    }

    func opDetect(_ dir: String) async -> OPDetect? {
        try? await client?.send("POST", "/api/onepassword/detect", body: ["cwd": dir], as: OPDetect.self)
    }

    func opPlan(_ body: [String: Any]) async throws -> OPPlan {
        try JSONDecoder().decode(OPPlan.self, from: await sendJSON("POST", "/api/onepassword/plan", body))
    }

    func opCancelPlan(_ id: String) async {
        _ = try? await client?.send("DELETE", "/api/onepassword/plan/\(id.urlPath)", as: EmptyResponse.self)
    }

    /// Hands values to the daemon (request body only; never logged or kept).
    func opImport(_ body: [String: Any]) async throws -> OPImportResult {
        try JSONDecoder().decode(OPImportResult.self, from: await sendJSON("POST", "/api/onepassword/import", body))
    }

    func opShares() async throws -> OPShares {
        guard let client else { throw DaemonError.notRunning }
        return try await client.send("GET", "/api/onepassword/shares", as: OPShares.self)
    }

    /// Lets (or stops letting) a paired Mac take cached values; items are secret names or refs.
    func opShare(peer: String, items: [String], allow: Bool) async throws -> OPShares {
        let body: [String: Any] = ["peer": peer, "items": items, "allow": allow, "actor": "app"]
        return try JSONDecoder().decode(OPShares.self, from: await sendJSON("POST", "/api/onepassword/shares", body))
    }

    /// Declines a paired Mac's requests (refs).
    func opDeclineRequest(peer: String, refs: [String]) async throws -> OPShares {
        let body: [String: Any] = ["peer": peer, "items": refs, "actor": "app"]
        return try JSONDecoder().decode(OPShares.self, from: await sendJSON("POST", "/api/onepassword/shares/decline", body))
    }

    /// Offers cached 1Password values to a paired Mac; it must accept them there.
    func opPush(peer: String, items: [String]) async throws -> OPShares {
        let body: [String: Any] = ["peer": peer, "items": items, "actor": "app"]
        return try JSONDecoder().decode(OPShares.self, from: await sendJSON("POST", "/api/onepassword/push", body))
    }

    /// Approves a paired Mac's request (matched by reference). Values cached here are shared; ones that
    /// aren't are read from 1Password first (it asks to unlock) under a free name — never replacing an
    /// existing value — and shared once stored. Returns a summary for the user.
    @discardableResult
    func approveRequest(peer: String, refs: [String]) async -> String {
        do {
            let shares = try await opShares()
            guard let p = shares.peers?.first(where: { $0.id == peer }) else { return "That Mac is no longer paired." }
            let wanted = Set(refs)
            let have = p.requests.filter { wanted.isEmpty || wanted.contains($0.ref) }
            let missing = p.missing.filter { wanted.isEmpty || wanted.contains($0.ref) }
            var done: [String] = [], errors: [String] = []
            if !have.isEmpty {
                _ = try await opShare(peer: peer, items: have.map(\.ref), allow: true)
                done += have.map { $0.name ?? $0.ref }
            }
            // One plan per value (each gets its own free name), but one read — one approval in 1Password.
            var runs: [(OPShares.Want, OnePasswordRun)] = []
            for w in missing {
                let run = OnePasswordRun(store: self)
                var body: [String: Any] = ["kind": "link", "ref": w.ref, "sharePeer": peer]
                if let k = w.key { body["hint"] = k }
                if await run.prepare(body) { runs.append((w, run)) } else { errors.append("\(w.ref): \(run.error ?? "couldn't plan")") }
            }
            if !runs.isEmpty {
                do {
                    let cli = try await Task.detached(priority: .userInitiated) { try OnePasswordCLI.find() }.value
                    let refs = runs.flatMap { $0.1.plan?.reads.map(\.ref) ?? [] }
                    var (got, readErrors) = await Task.detached(priority: .userInitiated) { OnePasswordRun.readBatch(cli, refs) }.value
                    await Task.detached(priority: .utility) { cli.signout() }.value
                    for (w, run) in runs {
                        await run.run(prefetched: got, account: cli.account)
                        if run.phase == .done, run.result?.sharedWith != nil {
                            done.append(run.plan?.items.first?.name ?? w.ref)
                        } else {
                            errors.append(readErrors[w.ref] ?? "\(w.ref): \(run.error ?? "not read from 1Password")")
                        }
                    }
                    got.removeAll()
                    readErrors.removeAll()
                } catch {
                    for (_, run) in runs { await run.cancel() }
                    errors.append(error.localizedDescription)
                }
            }
            await refreshSecrets()
            if done.isEmpty && errors.isEmpty { return "Nothing left to approve for \(p.name)." }
            var msg = done.isEmpty ? "" : "Shared \(done.joined(separator: ", ")) with \(p.name)."
            if !errors.isEmpty { msg += (msg.isEmpty ? "" : " ") + "Failed: " + errors.joined(separator: "; ") }
            return msg
        } catch { return error.localizedDescription }
    }

    /// From a notification: approve a request / accept a push, then tell the user how it went.
    func answerFromNotification(_ action: String, _ info: [AnyHashable: Any]) async {
        let refs = info["refs"] as? [String] ?? []
        switch action {
        case NotificationRouter.approve:
            guard let peer = info["peer"] as? String else { return }
            let msg = await approveRequest(peer: peer, refs: refs)
            notify(title: "1Password request", body: msg)
        case NotificationRouter.decline:
            guard let peer = info["peer"] as? String else { return }
            _ = try? await opDeclineRequest(peer: peer, refs: refs)
        case NotificationRouter.accept, NotificationRouter.reject:
            guard let machine = info["machine"] as? String else { return }
            do {
                try await opAnswerPush(machine: machine, refs: refs.isEmpty ? nil : refs, accept: action == NotificationRouter.accept)
            } catch { notify(title: "Couldn't answer \(machine)", body: error.localizedDescription) }
        default: break
        }
    }

    /// Accepts (or rejects) values a paired Mac pushed here; nil refs = all from that Mac.
    func opAnswerPush(machine: String, refs: [String]?, accept: Bool) async throws {
        var body: [String: Any] = ["machine": machine]
        if let refs { body["refs"] = refs }
        _ = try await sendJSON("POST", accept ? "/api/onepassword/peers/accept" : "/api/onepassword/peers/decline", body)
        if accept { await refreshSecrets() }
    }

    /// Asks paired Macs for values our services wait for, and takes the ones they approved (from their
    /// cache; never 1Password).
    func opPeerSync(service: String? = nil) async throws -> OPPeerSync {
        var body: [String: Any] = [:]
        if let service { body["service"] = service }
        let r = try JSONDecoder().decode(OPPeerSync.self, from: await sendJSON("POST", "/api/onepassword/peers/sync", body))
        await refreshSecrets()
        return r
    }

    func opAudit(_ n: Int = 100) async -> [OPAuditEntry] {
        struct R: Decodable { let entries: [OPAuditEntry] }
        return (try? await client?.send("GET", "/api/onepassword/audit?n=\(n)", as: R.self))?.entries ?? []
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

    func shareAddresses() async -> [ShareAddress] {
        (try? await client?.send("GET", "/api/remote/addresses", as: [ShareAddress].self)) ?? []
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

    // MARK: network

    var vpn: VpnStatus?

    func refreshNetwork() async {
        struct N: Decodable { let vpn: VpnStatus? }
        vpn = (try? await client?.send("GET", "/api/network", as: N.self))?.vpn
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
        return client?.events("\(Self.path(name: name, machine: machine))/logs/stream?tail=\(LogPaging.initial)")
    }

    /// Up to `count` lines logged before the line at cursor `before` (oldest first); fewer once the start is reached.
    func olderLogs(_ id: String, before: String, count: Int = LogPaging.page) async throws -> [LogLine] {
        struct R: Decodable { let lines: [LogLine] }
        let (name, machine) = Self.split(id)
        let q = before.addingPercentEncoding(withAllowedCharacters: .alphanumerics) ?? before
        guard let client else { throw URLError(.notConnectedToInternet) }
        return try await client.send("GET", "\(Self.path(name: name, machine: machine))/logs?tail=\(count)&before=\(q)", as: R.self).lines
    }

    func registerApprovals() async {
        guard let client else { return }
        approvals = await Approver.shared.register(client)
        approvalProblem = Approver.shared.problem
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

/// Routes a click on a notification (e.g. "1Password changes to review") back into the app.
final class NotificationRouter: NSObject, UNUserNotificationCenterDelegate {
    weak var store: AppStore?

    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification,
                                withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void) {
        completionHandler([.banner, .sound])
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
                                withCompletionHandler completionHandler: @escaping () -> Void) {
        let info = response.notification.request.content.userInfo
        let service = info["review"] as? String
        let action = response.actionIdentifier
        // Plain values only cross into the main actor.
        let peer = info["peer"] as? String, machine = info["machine"] as? String, refs = info["refs"] as? [String]
        Task { @MainActor [weak self] in
            if [Self.approve, Self.decline, Self.accept, Self.reject].contains(action) {
                var i: [AnyHashable: Any] = ["refs": refs ?? []]
                if let peer { i["peer"] = peer }
                if let machine { i["machine"] = machine }
                await self?.store?.answerFromNotification(action, i)
            } else if let service {
                self?.store?.openReview(service)
            }
            completionHandler()
        }
    }

    static let request = "lstack.request", pushed = "lstack.pushed"
    static let approve = "lstack.approve", decline = "lstack.decline", accept = "lstack.accept", reject = "lstack.reject"

    /// Approve / Decline on a paired Mac's request; Accept / Reject on values it pushed. Approving may read
    /// from 1Password (which prompts), so it brings the app forward.
    static func registerCategories() {
        let request = UNNotificationCategory(identifier: Self.request, actions: [
            UNNotificationAction(identifier: approve, title: "Approve", options: [.foreground]),
            UNNotificationAction(identifier: decline, title: "Decline", options: [.destructive]),
        ], intentIdentifiers: [])
        let pushed = UNNotificationCategory(identifier: Self.pushed, actions: [
            UNNotificationAction(identifier: accept, title: "Accept", options: []),
            UNNotificationAction(identifier: reject, title: "Reject", options: [.destructive]),
        ], intentIdentifiers: [])
        UNUserNotificationCenter.current().setNotificationCategories([request, pushed])
    }
}

struct CmuxDenial: Equatable {
    /// The open-terminal / open-dashboard request to retry.
    let path: String
    let message: String
}
