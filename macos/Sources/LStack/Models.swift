import Foundation

struct MetricSample: Codable, Hashable {
    let ts: Double
    let cpu: Double
    let mem: Double
    let netRx: Double?
    let netTx: Double?

    var date: Date { Date(timeIntervalSince1970: ts / 1000) }
}

/// Whole-machine load with LStack's share (`/api/system`, SSE "system").
struct SystemSample: Codable, Hashable {
    let ts: Double
    let cpu: Double      // % of all cores
    let mem: Double      // bytes in use
    let lsCpu: Double    // LStack services, % of all cores
    let lsMem: Double

    var date: Date { Date(timeIntervalSince1970: ts / 1000) }
}

struct SystemInfo: Codable {
    var cores: Int
    var memTotal: Double
    var samples: [SystemSample]
}

/// Another Mac's load, as relayed by the daemon (fields are null until the peer has answered).
struct MachineSystem: Codable, Equatable {
    var cores: Int?
    var memTotal: Double?
    var samples: [SystemSample]?
}

struct HealthCheck: Codable, Hashable {
    var type: String
    var url: String?
    var port: Int?
}

struct ServiceConfig: Codable, Hashable {
    var name: String
    var type: String
    var description: String?
    var autostart: Bool?
    var restart: String?
    var maxRestarts: Int?
    var command: String?
    var cwd: String?
    var env: [String: String]?
    var source: [String]?
    var image: String?
    var container: String?
    var ports: [String]?
    var volumes: [String]?
    var composeFile: String?
    var project: String?
    var healthcheck: HealthCheck?
    var secrets: SecretRefs?
    var heartbeat: HeartbeatConfig?
    /// `.env` files (relative to `cwd`) read like `op run --env-file`.
    var envFiles: [String]?
}

/// `secrets:` in a service definition: `[NAME, …]` or `{ENV_VAR: SECRET_NAME}`.
enum SecretRefs: Codable, Hashable {
    case list([String])
    case map([String: String])

    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if let l = try? c.decode([String].self) { self = .list(l) }
        else if let m = try? c.decode([String: String].self) { self = .map(m) }
        else { self = .list([]) }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case let .list(l): try c.encode(l)
        case let .map(m): try c.encode(m)
        }
    }

    /// Secret names (not values — those never leave the daemon).
    var names: [String] {
        switch self {
        case let .list(l): return l
        case let .map(m): return Array(Set(m.values)).sorted()
        }
    }

    /// Env var → stored secret name.
    var pairs: [(key: String, name: String)] {
        switch self {
        case let .list(l): return l.map { ($0, $0) }
        case let .map(m): return m.sorted { $0.key < $1.key }.map { ($0.key, $0.value) }
        }
    }

    var isMap: Bool { if case .map = self { return true }; return false }

    /// The simple list form when every env var has its secret's name (unless it was a map already).
    static func make(_ pairs: [(String, String)], asMap: Bool) -> SecretRefs? {
        if pairs.isEmpty { return nil }
        if !asMap, pairs.allSatisfy({ $0.0 == $0.1 }) { return .list(pairs.map(\.0)) }
        return .map(Dictionary(pairs, uniquingKeysWith: { _, b in b }))
    }
}

/// healthchecks.io-style pings for a service.
struct HeartbeatConfig: Codable, Hashable {
    var url: String?
    var slug: String?
    var interval: Double?
    var start: Bool?
    var fail: Bool?
    var logs: Bool?
    var enabled: Bool?
}

struct HeartbeatStatus: Codable, Hashable {
    let lastPing: Double
    let kind: String
    let ok: Bool
    let error: String?

    var date: Date { Date(timeIntervalSince1970: lastPing / 1000) }
}

/// A web address a service serves (detected listening port or `urls:` from its definition).
struct ServiceURL: Codable, Hashable {
    let url: String
    let label: String?
    let port: Int?
    let source: String
    let localOnly: Bool?
    /// Forwarded from this paired Mac over the encrypted link: the URL is on this Mac.
    var via: String? = nil
    /// "paused": its forward is paused (opening resumes it). "lan": a LAN address of a Mac reached over Tailscale.
    var blocked: String? = nil

    var display: String {
        if let label, !label.isEmpty { return label }
        return url.replacingOccurrences(of: "http://", with: "").replacingOccurrences(of: "https://", with: "")
    }
}

/// A busy process (over the CPU / memory / network thresholds).
struct ProcInfo: Codable, Hashable, Identifiable {
    var id: Int { pid }
    let pid: Int
    let name: String
    let cpu: Double
    let mem: Double
    let net: Double?
}

struct ProcessThresholds: Codable, Equatable {
    var enabled: Bool
    var cpu: Double
    var mem: Double
    var net: Double
    var max: Int
}

/// Another Mac this one watches (`/api/machines`, SSE "machines"/"peer").
struct Machine: Codable, Identifiable, Equatable {
    var id: String { name }
    var name: String
    var url: String?
    var address: String?
    var status: String
    var error: String?
    var hostname: String?
    var version: String?
    var fingerprint: String?
    /// What that Mac lets this one do (control + tunnels); `allowControl` mirrors `grants.control`.
    var grants: PeerGrants?
    var allowControl: Bool?
    var lastSeen: Double?
    var services: [ServiceState]?
    var processes: [ProcInfo]?
    var configErrors: [String]?
    var system: MachineSystem?
    /// How the link travels: "local" (LAN) or "tailscale".
    var transport: String?
    /// Its Tailscale device name (e.g. lstack-studio.tailnet.ts.net), once known.
    var tailnetName: String?
    /// The live link's path: kind "lan" or "tailscale" + the address dialed.
    var via: LinkVia?
    /// Transport asked for but not yet confirmed by that Mac.
    var pendingTransport: String?
    var transportChange: TransportChange?
    /// That Mac has Tailscale turned on.
    var tailscaleAvailable: Bool?

    var online: Bool { status == "online" }
    var onTailscale: Bool { transport == "tailscale" }
    /// "LAN" / "Tailscale" plus what the live link is going through, for tooltips.
    var linkDescription: String {
        let t = onTailscale ? "Tailscale" : "local network"
        guard online, let v = via else { return "Connects over the \(t)" }
        return "Connected over \(v.kind == "tailscale" ? "Tailscale" : "the local network") (\(v.address))"
    }
    var canControl: Bool { allowControl ?? false }
    /// Paired with an older LStack (before the encrypted link): must pair again.
    var needsRepair: Bool { status == "repair" || status == "unauthorized" }
}

struct LinkVia: Codable, Equatable {
    let kind: String
    let address: String
}

/// A transport switch in progress or stuck: `state` is preparing, waiting-login, waiting-approval, switching, error
/// or warning.
struct TransportChange: Codable, Equatable {
    let to: String
    let state: String
    let message: String?

    var busy: Bool { ["preparing", "waiting-login", "waiting-approval", "switching"].contains(state) }
}

/// Per-Mac permissions: `{control, tunnel: "all" | "none" | [services]}`.
struct PeerGrants: Codable, Equatable, Hashable {
    enum Tunnel: Equatable, Hashable {
        case all, none
        case only([String])
    }
    var control: Bool
    var tunnel: Tunnel

    init(control: Bool, tunnel: Tunnel) {
        self.control = control
        self.tunnel = tunnel
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        control = (try? c.decode(Bool.self, forKey: .control)) ?? false
        if let s = try? c.decode(String.self, forKey: .tunnel) {
            tunnel = s == "all" ? .all : .none
        } else if let list = try? c.decode([String].self, forKey: .tunnel) {
            tunnel = list.isEmpty ? .none : .only(list)
        } else {
            tunnel = .none
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(control, forKey: .control)
        switch tunnel {
        case .all: try c.encode("all", forKey: .tunnel)
        case .none: try c.encode("none", forKey: .tunnel)
        case .only(let l): try c.encode(l, forKey: .tunnel)
        }
    }

    enum CodingKeys: String, CodingKey { case control, tunnel }

    var summary: String {
        let t: String
        switch tunnel {
        case .all: t = "tunnels to all services"
        case .none: t = "no tunnels"
        case .only(let l): t = "tunnels to \(l.joined(separator: ", "))"
        }
        return "\(control ? "view + control" : "view only") · \(t)"
    }
}

struct MachinesEvent: Decodable {
    let local: String
    let machines: [Machine]
}

/// SSE "peer": something changed on another machine.
struct PeerEvent: Decodable {
    let machine: String
    let event: String
    let data: RawJSON
}

/// Keeps a JSON sub-document undecoded so it can be decoded later by event type.
struct RawJSON: Decodable {
    let data: Data
    init(from decoder: Decoder) throws {
        let v = try decoder.singleValueContainer().decode(JSONValue.self)
        data = (try? JSONEncoder().encode(v)) ?? Data()
    }
}

indirect enum JSONValue: Codable {
    case null, bool(Bool), number(Double), string(String), array([JSONValue]), object([String: JSONValue])

    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let b = try? c.decode(Bool.self) { self = .bool(b) }
        else if let n = try? c.decode(Double.self) { self = .number(n) }
        else if let s = try? c.decode(String.self) { self = .string(s) }
        else if let a = try? c.decode([JSONValue].self) { self = .array(a) }
        else { self = .object(try c.decode([String: JSONValue].self)) }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case let .bool(b): try c.encode(b)
        case let .number(n): try c.encode(n)
        case let .string(s): try c.encode(s)
        case let .array(a): try c.encode(a)
        case let .object(o): try c.encode(o)
        }
    }
}

struct ProcessesEvent: Decodable {
    let processes: [ProcInfo]
}

struct SecretInfo: Decodable, Identifiable, Hashable {
    var id: String { name }
    let name: String
    let setAt: Double?
    let usedBy: [String]
    let isInternal: Bool?
    let missing: Bool?
    /// "local" (typed in), "op" (1Password reference), "op-env" (1Password Environment) or "peer:<machine>".
    let source: String?
    let ref: String?
    let template: String?
    let account: String?
    let origin: String?
    /// When the value was last read from 1Password (ms).
    let fetchedAt: Double?

    enum CodingKeys: String, CodingKey {
        case name, setAt, usedBy, missing, source, ref, template, account, origin, fetchedAt
        case isInternal = "internal"
    }

    var isSet: Bool { !(missing ?? false) }
    var fromOnePassword: Bool { source == "op" }
    var peer: String? { source?.hasPrefix("peer:") == true ? String(source!.dropFirst(5)) : nil }

    /// Where the value came from, for display.
    var sourceLabel: String? {
        switch source {
        case "op": return "1Password reference"
        case "op-env": return "1Password Environment"
        case "local": return "typed in"
        default: return peer.map { "from \($0)" }
        }
    }
}

struct SecretsStatus: Decodable {
    let locked: Bool
    let error: String?
    let storage: String
    let secrets: [SecretInfo]
}

struct PairCode: Decodable {
    let code: String
    let expiresAt: Double
    let urls: [String]
    /// A 2FA code is needed too.
    var totp: Bool?
}

/// What `POST /api/machines` returns: the new machine plus the pairing's confirmation phrase.
struct PairResult: Decodable {
    let name: String
    let phrase: String?
    let fingerprint: String?
    let grants: PeerGrants?
}

/// A Mac paired with this one (this Mac's side, `GET /api/remote/peers`).
struct SharedPeer: Decodable, Identifiable, Equatable {
    let id: String
    let name: String
    let fingerprint: String?
    let phrase: String?
    let grants: PeerGrants
    let createdAt: Double
    let lastSeen: Double?
    let online: Bool?
    let address: String?
    let streams: Int?
    /// "local" or "tailscale" (what this Mac accepts from it).
    let transport: String?
    /// The live link: "lan" or "tailscale".
    let via: String?
}

/// This install's link key.
struct LinkIdentity: Decodable {
    let name: String
    let fingerprint: String
}

/// A paired Mac's service forwarded to localhost (`GET /api/forwards`).
struct ForwardInfo: Decodable, Identifiable, Hashable {
    var id: String { "\(machine)/\(service)/\(port)" }
    let machine: String
    let service: String
    let port: Int
    let localPort: Int?
    let url: String?
    let paused: Bool
    let online: Bool
    let connections: Int
    let error: String?
}

/// Recent link activity on this Mac (`GET /api/remote/activity`).
struct LinkActivity: Decodable {
    struct Event: Decodable, Hashable {
        let event: String
        let at: Double
        let peer: String?
        let service: String?
        let port: Int?
        let address: String?
        let reason: String?
    }
    struct Tunnel: Decodable, Hashable {
        let peer: String
        let service: String
        let port: Int
        let connections: Int
        let bytes: Double
        let lastAt: Double
    }
    let events: [Event]
    let tunnels: [Tunnel]
}

/// Result of `POST /api/split-command`: leading `cd`/env/`source` steps peeled off a command.
struct CommandSplit: Decodable {
    struct Parts: Decodable {
        let command: String
        let cwd: String?
        let env: [String: String]
        let source: [String]
    }
    let changed: Bool
    let split: Parts
    let shell: String
    let defaultSource: [String]
}

struct ServiceState: Codable, Identifiable, Hashable {
    /// Unique across machines (names can repeat on different Macs).
    var id: String { machine.map { "\(name)@\($0)" } ?? name }
    let name: String
    let type: String
    let description: String?
    let autostart: Bool
    let tags: [String]
    let status: String
    let health: String
    let pid: Int?
    let startedAt: Double?
    let exitCode: Int?
    let restarts: Int
    /// Set while a crash-loop restart is pending (ms since epoch).
    let restartAt: Double?
    let lastError: String?
    var metrics: MetricSample?
    let urls: [ServiceURL]?
    let heartbeat: HeartbeatStatus?
    /// nil = this Mac; otherwise the paired machine's name.
    let machine: String?
    let config: ServiceConfig
    /// 1Password references in its env files waiting for review (keys only).
    var secretsPending: SecretsPending? = nil

    /// Addresses that can be opened from here (LAN ones are hidden while that Mac is on Tailscale).
    var links: [ServiceURL] { (urls ?? []).filter { $0.blocked != "lan" } }
    var isRemote: Bool { machine != nil }

    var isRunning: Bool { status == "running" }
    var isTransitioning: Bool { status == "starting" || status == "stopping" }
    var isFailed: Bool { status == "crashed" || status == "errored" }
    /// Crashed and waiting to be restarted; Stop cancels the pending restart.
    var isRestarting: Bool { restartAt != nil }
    /// Whether Stop applies (running, starting/stopping, or between crash-loop attempts).
    var isStoppable: Bool { isRunning || isTransitioning || isRestarting }

    func restartsIn(now: Date = Date()) -> Int {
        guard let r = restartAt else { return 0 }
        return max(0, Int(ceil((r / 1000) - now.timeIntervalSince1970)))
    }

    var uptime: TimeInterval? {
        guard let s = startedAt, isRunning else { return nil }
        return Date().timeIntervalSince1970 - s / 1000
    }

    var summary: String {
        switch type {
        case "process": return config.command ?? ""
        case "docker": return config.image ?? config.container ?? ""
        case "compose": return (config.composeFile as NSString?)?.abbreviatingWithTildeInPath ?? ""
        default: return ""
        }
    }
}

struct LogLine: Codable, Hashable, Identifiable {
    let ts: Double
    let stream: String
    let line: String
    /// Cursor for paging to older lines (nil from daemons that predate log paging).
    var pos: String? = nil
    var id: String { "\(ts)-\(line.hashValue)" }
}

struct DaemonInfo: Codable {
    let pid: Int
    let port: Int
    let token: String
    let version: String
    let bin: String?
    let running: Bool?
}

struct Snapshot: Codable {
    let services: [ServiceState]
    let configErrors: [String]
    var machine: String?
}

struct MetricsEvent: Codable {
    let name: String
    let metrics: MetricSample
}

struct NameEvent: Codable {
    let name: String
}

// MARK: - Formatting

enum Fmt {
    static func bytes(_ n: Double?) -> String {
        guard let n, n.isFinite else { return "—" }
        let f = ByteCountFormatter()
        f.countStyle = .memory
        f.allowsNonnumericFormatting = false
        return f.string(fromByteCount: Int64(n))
    }

    static func rate(_ n: Double?) -> String {
        guard let n else { return "—" }
        return bytes(n) + "/s"
    }

    static func gb(_ n: Double) -> String {
        let g = n / 1_073_741_824
        return String(format: g >= 10 ? "%.0f GB" : "%.1f GB", g)
    }

    static func pct(_ n: Double?) -> String {
        guard let n else { return "—" }
        return String(format: n < 10 ? "%.1f%%" : "%.0f%%", n)
    }

    static func duration(_ t: TimeInterval?) -> String {
        guard let t, t > 0 else { return "—" }
        let s = Int(t)
        if s < 60 { return "\(s)s" }
        if s < 3600 { return "\(s / 60)m \(s % 60)s" }
        if s < 86400 { return "\(s / 3600)h \((s % 3600) / 60)m" }
        return "\(s / 86400)d \((s % 86400) / 3600)h"
    }
}

struct RemoteStatus: Codable, Equatable {
    var enabled: Bool
    var running: Bool
    var port: Int
    var bind: String
    var username: String
    var hasPassword: Bool
    var allowControl: Bool
    var totp: Bool?
    var hostname: String?
    var urls: [String]
    var error: String?
    /// Other devices may open the web dashboard (false = this Mac only).
    var webRemote: Bool?
    /// Paired Macs can watch this one.
    var sharing: Bool?
    var shareUrls: [String]?
    /// The address chosen to show for this Mac (nil = automatic).
    var shareAddress: String?
    var linkPort: Int?
    var pairedMachines: Int?
    /// Transport changes asked for by paired Macs are applied without asking (default on).
    var autoApproveTransport: Bool?
    var transportRequests: [TransportRequest]?
}

/// A paired Mac asked to change how it connects (waiting for Allow/Deny here).
struct TransportRequest: Codable, Equatable, Identifiable {
    var id: String { peerId }
    let peerId: String
    let name: String
    let transport: String
    let at: Double
}

/// `GET /api/tailnet`: this Mac's Tailscale helper.
struct TailnetStatus: Decodable, Equatable {
    struct SelfNode: Decodable, Equatable {
        let dnsName: String?
        let ips: [String]?
    }
    struct Peer: Decodable, Equatable, Identifiable {
        var id: String { dnsName }
        let dnsName: String
        let hostName: String?
        let ips: [String]?
        let online: Bool?
        let os: String?
        let isLStack: Bool?
        /// "direct", "relay" or "" (no traffic yet).
        let path: String?
        let relay: String?
    }
    let enabled: Bool
    /// off, installing, starting, needs-login, running, error
    let phase: String
    let error: String?
    let autoUpdate: Bool?
    let acknowledged: Bool?
    let installedVersion: String?
    let version: String?
    let lastCheck: Double?
    let checking: Bool?
    let devBinary: Bool?
    let deviceName: String?
    let authURL: String?
    let `self`: SelfNode?
    let tailnet: String?
    let user: String?
    let health: [String]?
    let peers: [Peer]?

    var running: Bool { phase == "running" }
}

/// `GET /api/network`: an active VPN can block the local network or push Tailscale onto a relay.
struct VpnStatus: Decodable, Equatable {
    let active: Bool
    let interface: String?
    let fullTunnel: Bool?
    let tailscaleApp: Bool?
}

/// `GET /api/remote/addresses`: a way other Macs might reach this one, checked by resolving it.
struct ShareAddress: Decodable, Identifiable, Hashable {
    let address: String
    /// ip, dns, bonjour or tailscale
    let kind: String
    let source: String?
    let resolves: [String]
    let pointsHere: Bool
    let selected: Bool
    var id: String { address }
}

struct RemotePatch: Encodable {
    var enabled: Bool?
    var webRemote: Bool?
    var sharing: Bool?
    var port: Int?
    var username: String?
    var password: String?
    var allowControl: Bool?
    var linkPort: Int?
    /// "" = automatic.
    var shareAddress: String?
    var autoApproveTransport: Bool?
}

struct TotpSetup: Decodable {
    let secret: String
    let uri: String
}

/// `GET /api/settings`: daemon-wide preferences plus a little system info.
struct DaemonSettings: Decodable, Equatable {
    struct TerminalApp: Decodable, Equatable, Identifiable {
        let id: String
        let name: String
        let installed: Bool
    }
    var terminal: String
    var terminals: [TerminalApp]
    var fullDiskAccess: Bool
    var helperPath: String
    var cmux: CmuxStatus?
    var healthchecks: HealthchecksStatus?
    var processes: ProcessThresholds?
    var onepassword: OnePasswordSettings?
}

struct CmuxStatus: Decodable, Equatable {
    var installed: Bool
    var mode: String
    var hasPassword: Bool
    /// cmux only lets LStack (started outside cmux) open workspaces in password / allowAll mode.
    var allowsLStack: Bool { mode == "password" ? hasPassword : mode == "allowAll" }
}

struct HealthchecksStatus: Decodable, Equatable {
    var baseUrl: String
    var logLines: Int?
    var machine: HeartbeatConfig?
    var hasPingKey: Bool
    var machineStatus: HeartbeatStatus?
}

/// `open-dashboard` / `open-terminal`: cmux refusal comes back as 200 + `cmuxDenied`.
struct OpenResult: Decodable {
    let ok: Bool?
    let cmuxDenied: Bool?
    let error: String?
}

struct CmuxFixResult: Decodable {
    let reloaded: Bool?
    let backup: String?
    let note: String?
}

// MARK: - 1Password

/// `secretsPending` on a service: references in its env files that need a look (keys only, never values).
struct SecretsPending: Codable, Hashable {
    var count: Int
    var new: [String]?
    var changed: [String]?
    var missing: [String]?
    var blocked: [String]?
    var removed: [String]?
    var errors: [String]?

    var summary: String {
        var parts: [String] = []
        if count > 0 { parts.append("\(count) to review") }
        if let b = blocked, !b.isEmpty { parts.append("\(b.count) blocked") }
        if let e = errors, !e.isEmpty { parts.append("\(e.count) file error\(e.count == 1 ? "" : "s")") }
        return parts.isEmpty ? "needs a look" : parts.joined(separator: " · ")
    }
}

/// Settings › 1Password (`onepassword` in `/api/settings`).
struct OnePasswordSettings: Codable, Equatable {
    var watch: Bool?
    var autoApplyRemovals: Bool?
    var restartOnChange: Bool?
    var allowedVaults: [String]?
    var pushToPeers: Bool?
    var pullFromPeers: Bool?
}

/// `GET /api/onepassword/review/:service`.
struct OPReview: Decodable {
    struct File: Decodable, Hashable {
        let path: String
        /// ok, fifo (an Environment mount), missing or error.
        let state: String
        let error: String?
    }
    struct Ref: Decodable, Hashable, Identifiable {
        var id: String { key }
        let key: String
        let ref: String
        let template: String?
        let file: String?
        let line: Int?
        /// ok, new, changed, missing or blocked.
        let status: String
        let secret: String?
        let vault: String?
        let item: String?
        let field: String?
        let warnings: [String]?
        let blocked: String?
        /// Paired Macs that share this reference's value (cache to cache, no 1Password).
        let offeredBy: [String]?
    }
    struct Removed: Decodable, Hashable {
        let key: String
        let secret: String
        let ref: String?
    }
    let service: String
    let files: [File]
    let refs: [Ref]
    let removed: [Removed]
    let plain: [String]
    let warnings: [String]
    let locked: Bool

    /// Something "Pull from 1Password" would do.
    var actionable: Bool { refs.contains { ["new", "changed", "missing"].contains($0.status) } || !removed.isEmpty }
    var hasCached: Bool { refs.contains { $0.status == "ok" } }
}

/// `POST /api/onepassword/detect`: env files in a folder (names and keys, never values).
struct OPDetect: Decodable {
    struct File: Decodable, Hashable, Identifiable {
        var id: String { name }
        struct Ref: Decodable, Hashable {
            let key: String
            let ref: String
        }
        let name: String
        let example: Bool
        let refs: [Ref]?
        let plain: [String]?
        let fifo: Bool?
        let error: String?

        var isMount: Bool { fifo == true }
        var summary: String {
            if isMount { return "1Password Environment mount" }
            if let error { return error }
            let r = refs?.count ?? 0, p = plain?.count ?? 0
            var parts: [String] = []
            if r > 0 { parts.append("\(r) op:// reference\(r == 1 ? "" : "s")") }
            if p > 0 { parts.append("\(p) plain value\(p == 1 ? "" : "s")") }
            return parts.isEmpty ? "empty" : parts.joined(separator: ", ")
        }
    }
    let dir: String
    let files: [File]
}

/// `GET/POST /api/onepassword/shares`: cached 1Password values shared with paired Macs (never 1Password itself).
struct OPShares: Decodable {
    struct Granted: Decodable, Hashable {
        let ref: String
        let name: String?
        let have: Bool?
        let fetchedAt: Double?
    }
    struct Want: Decodable, Hashable {
        let ref: String
        let name: String?
        let have: Bool?
        let granted: Bool?
        let fetchedAt: Double?
    }
    struct Peer: Decodable, Identifiable {
        let id: String
        let name: String
        let granted: [Granted]?
        let wants: [Want]?
        let wantsAt: Double?
    }
    struct Received: Decodable, Hashable {
        let name: String
        let ref: String?
        let from: String
        let fetchedAt: Double?
    }
    struct Available: Decodable, Hashable {
        let machine: String
        let ref: String
    }
    let peers: [Peer]?
    let received: [Received]?
    let available: [Available]?
}

/// `POST /api/onepassword/peers/sync`.
struct OPPeerSync: Decodable {
    struct Received: Decodable, Hashable {
        let machine: String
        let stored: [String]?
        let updated: [String]?
    }
    let received: [Received]?
    let errors: [String]?

    var summary: String {
        let lines = (received ?? []).compactMap { r -> String? in
            let n = (r.stored ?? []) + (r.updated ?? [])
            return n.isEmpty ? nil : "From \(r.machine): \(n.joined(separator: ", "))"
        }
        return lines.isEmpty ? "Nothing new from paired Macs." : lines.joined(separator: "\n")
    }
}

/// `POST /api/onepassword/plan`: what would be read from 1Password and where it would go.
struct OPPlan: Decodable, Identifiable {
    struct Item: Decodable, Hashable, Identifiable {
        var id: String { name }
        let key: String?
        let name: String
        let ref: String
        let template: String?
        /// "op" (read with the CLI) or "op-env" (already read from an Environment).
        let source: String
        let cached: Bool
        let status: String
        let warnings: [String]?
        let vault: String?
        let item: String?
        let field: String?
        let needsRead: Bool
        /// A cached value received from this paired Mac (not read from 1Password here).
        let from: String?
        let fetchedAt: Int64?

        /// `KEY → NAME`, or just the one when they match.
        var target: String {
            guard let key, key != name else { return key ?? name }
            return "\(key) → \(name)"
        }
    }
    struct Removal: Decodable, Hashable {
        let key: String
        let secret: String
        let ref: String?
    }
    let id: String
    let kind: String
    let service: String?
    let expiresAt: Double?
    let items: [Item]
    let removals: [Removal]

    var reads: [Item] { items.filter(\.needsRead) }
    /// Values a paired Mac sent: always shown and confirmed before they're granted to a service.
    var fromPeers: [Item] { items.filter { $0.cached && $0.from != nil } }
    var peerNames: [String] { Array(Set(fromPeers.compactMap(\.from))).sorted() }
}

/// `POST /api/onepassword/import`.
struct OPImportResult: Decodable {
    let stored: [String]?
    let granted: [String: String]?
    let removed: [String]?
    let deleted: [String]?
    let skipped: [String]?
    let restarting: [String]?
}

/// One line of `GET /api/onepassword/audit` (no values, ever).
struct OPAuditEntry: Decodable {
    let ts: String
    let actor: String
    let action: String
    let detail: JSONValue?

    var summary: String {
        guard case let .object(d)? = detail else { return "" }
        var parts: [String] = []
        if case let .string(s)? = d["service"] { parts.append(s) }
        for (k, label) in [("stored", "stored"), ("removed", "removed"), ("deleted", "deleted")] {
            guard case let .array(list)? = d[k] else { continue }
            let names: [String] = list.compactMap {
                if case let .string(s) = $0 { return s }
                if case let .object(o) = $0, case let .string(s)? = o["name"] { return s }
                return nil
            }
            if !names.isEmpty { parts.append("\(label) \(names.joined(separator: ", "))") }
        }
        return parts.joined(separator: " · ")
    }

    var date: Date? { ISO8601DateFormatter().date(from: ts) ?? Self.fractional.date(from: ts) }

    private static let fractional: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return f
    }()
}
