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

    private static let streams = EventStreams()
    private static let streamSession: URLSession = {
        let cfg = URLSessionConfiguration.default
        cfg.timeoutIntervalForResource = .infinity
        cfg.requestCachePolicy = .reloadIgnoringLocalCacheData
        let queue = OperationQueue()
        queue.maxConcurrentOperationCount = 1
        return URLSession(configuration: cfg, delegate: streams, delegateQueue: queue)
    }()

    /// Stream server-sent events. Each event is delivered as (name, raw JSON data).
    ///
    /// Uses a delegate-driven data task rather than `URLSession.bytes(for:)`: an `AsyncBytes` that isn't
    /// read to the end (an error status, or cancellation before the first read) keeps its task, request,
    /// response and metrics alive inside the session forever, and the reconnect loops leaked one per retry.
    func events(_ path: String) -> AsyncThrowingStream<(String, Data), Error> {
        let req = request("GET", path, timeout: .infinity)
        return AsyncThrowingStream { continuation in
            let task = Self.streamSession.dataTask(with: req)
            Self.streams.register(task, continuation)
            continuation.onTermination = { _ in task.cancel() }
            task.resume()
        }
    }
}

/// Routes the stream session's delegate callbacks to each stream's continuation and parses SSE lines.
private final class EventStreams: NSObject, URLSessionDataDelegate, @unchecked Sendable {
    typealias Continuation = AsyncThrowingStream<(String, Data), Error>.Continuation

    private final class Stream {
        let continuation: Continuation
        var buffer = Data()
        var event = "message"
        var failed = false
        init(_ continuation: Continuation) { self.continuation = continuation }
    }

    private let lock = NSLock()
    private var streams: [Int: Stream] = [:]

    func register(_ task: URLSessionTask, _ continuation: Continuation) {
        lock.withLock { streams[task.taskIdentifier] = Stream(continuation) }
    }

    private func stream(_ task: URLSessionTask) -> Stream? {
        lock.withLock { streams[task.taskIdentifier] }
    }

    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive response: URLResponse,
                    completionHandler: @escaping (URLSession.ResponseDisposition) -> Void) {
        if let code = (response as? HTTPURLResponse)?.statusCode, code >= 400, let s = stream(dataTask) {
            s.failed = true
            s.continuation.finish(throwing: DaemonError.http(code, ""))
            completionHandler(.cancel)
            return
        }
        completionHandler(.allow)
    }

    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data) {
        guard let s = stream(dataTask), !s.failed else { return }
        s.buffer.append(data)
        while let nl = s.buffer.firstIndex(of: UInt8(ascii: "\n")) {
            var line = s.buffer[s.buffer.startIndex..<nl]
            s.buffer.removeSubrange(s.buffer.startIndex...nl)
            if line.last == UInt8(ascii: "\r") { line = line.dropLast() }
            handle(Data(line), s)
        }
    }

    private func handle(_ line: Data, _ s: Stream) {
        let event = Data("event: ".utf8), data = Data("data: ".utf8)
        if line.starts(with: event) {
            s.event = String(decoding: line.dropFirst(event.count), as: UTF8.self)
        } else if line.starts(with: data) {
            s.continuation.yield((s.event, Data(line.dropFirst(data.count))))
            s.event = "message"
        }
    }

    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        guard let s = lock.withLock({ streams.removeValue(forKey: task.taskIdentifier) }), !s.failed else { return }
        if let error { s.continuation.finish(throwing: error) } else { s.continuation.finish() }
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
