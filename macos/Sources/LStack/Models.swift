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

    var online: Bool { status == "online" }
    var canControl: Bool { allowControl ?? false }
    /// Paired with an older LStack (before the encrypted link): must pair again.
    var needsRepair: Bool { status == "repair" || status == "unauthorized" }
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

    enum CodingKeys: String, CodingKey {
        case name, setAt, usedBy, missing
        case isInternal = "internal"
    }

    var isSet: Bool { !(missing ?? false) }
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

    var links: [ServiceURL] { urls ?? [] }
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
    var linkPort: Int?
    var pairedMachines: Int?
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
