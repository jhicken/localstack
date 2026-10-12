import Foundation

enum DaemonError: LocalizedError {
    case notRunning
    case http(Int, String)

    var errorDescription: String? {
        switch self {
        case .notRunning: return "The lstack daemon is not running."
        case let .http(code, msg): return msg.isEmpty ? "HTTP \(code)" : msg
        }
    }
}

/// Talks to the lstack daemon over its localhost HTTP + SSE API.
struct DaemonClient: Sendable {
    let info: DaemonInfo

    static var home: URL {
        if let h = ProcessInfo.processInfo.environment["LSTACK_HOME"] { return URL(fileURLWithPath: h) }
        return FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".localstack")
    }

    static func readInfo() -> DaemonInfo? {
        guard let data = try? Data(contentsOf: home.appendingPathComponent("daemon.json")) else { return nil }
        return try? JSONDecoder().decode(DaemonInfo.self, from: data)
    }

    static func load() throws -> DaemonClient {
        guard let info = readInfo(), info.running != false else { throw DaemonError.notRunning }
        return DaemonClient(info: info)
    }

    private static let session: URLSession = {
        let cfg = URLSessionConfiguration.default
        cfg.timeoutIntervalForRequest = 30
        cfg.timeoutIntervalForResource = .infinity
        cfg.requestCachePolicy = .reloadIgnoringLocalCacheData
        return URLSession(configuration: cfg)
    }()

    func request(_ method: String, _ path: String, body: Encodable? = nil, timeout: TimeInterval = 120) -> URLRequest {
        var req = URLRequest(url: URL(string: "http://127.0.0.1:\(info.port)\(path)")!)
        req.httpMethod = method
        req.timeoutInterval = timeout
        req.setValue("Bearer \(info.token)", forHTTPHeaderField: "Authorization")
        if let body {
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            req.httpBody = try? JSONEncoder().encode(AnyEncodable(body))
        }
        return req
    }

    @discardableResult
    func send<T: Decodable>(_ method: String, _ path: String, body: Encodable? = nil, as: T.Type = EmptyResponse.self) async throws -> T {
        let (data, resp) = try await Self.session.data(for: request(method, path, body: body))
        let code = (resp as? HTTPURLResponse)?.statusCode ?? 0
        if code >= 400 {
            let msg = (try? JSONDecoder().decode([String: String].self, from: data))?["error"] ?? ""
            throw DaemonError.http(code, msg)
        }
        return try JSONDecoder().decode(T.self, from: data)
    }

    func action(_ name: String, _ action: String) async throws {
        try await send("POST", "/api/services/\(name.urlPath)/\(action)", as: ServiceState.self)
    }

    func metrics(_ name: String, range: String = "15m", points: Int = 90) async throws -> [MetricSample] {
        struct R: Decodable { let samples: [MetricSample] }
        return try await send("GET", "/api/services/\(name.urlPath)/metrics?range=\(range)&points=\(points)", as: R.self).samples
    }

    /// Stream server-sent events. Each event is delivered as (name, raw JSON data).
    func events(_ path: String) -> AsyncThrowingStream<(String, Data), Error> {
        let req = request("GET", path, timeout: .infinity)
        return AsyncThrowingStream { continuation in
            let task = Task {
                do {
                    let (bytes, resp) = try await Self.session.bytes(for: req)
                    if let code = (resp as? HTTPURLResponse)?.statusCode, code >= 400 { throw DaemonError.http(code, "") }
                    var event = "message"
                    for try await line in bytes.lines {
                        if line.hasPrefix("event: ") {
                            event = String(line.dropFirst(7))
                        } else if line.hasPrefix("data: ") {
                            continuation.yield((event, Data(line.dropFirst(6).utf8)))
                            event = "message"
                        }
                    }
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }
}

struct EmptyResponse: Decodable {}

private struct AnyEncodable: Encodable {
    let value: Encodable
    init(_ value: Encodable) { self.value = value }
    func encode(to encoder: Encoder) throws { try value.encode(to: encoder) }
}

extension String {
    var urlPath: String { addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? self }
}
