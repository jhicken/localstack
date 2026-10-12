import Foundation

struct MetricSample: Codable, Hashable {
    let ts: Double
    let cpu: Double
    let mem: Double
    let netRx: Double?
    let netTx: Double?

    var date: Date { Date(timeIntervalSince1970: ts / 1000) }
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
    var id: String { name }
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
    let config: ServiceConfig

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
}

struct RemotePatch: Encodable {
    var enabled: Bool?
    var port: Int?
    var username: String?
    var password: String?
    var allowControl: Bool?
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
}
