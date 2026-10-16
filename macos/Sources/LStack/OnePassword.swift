import Darwin
import Foundation
import Observation
import Security

/// A failed 1Password read. `stopsBatch`: the rest would fail the same way (declined, timed out, app
/// integration off), so don't ask again for each item.
struct OnePasswordError: LocalizedError {
    let message: String
    var stopsBatch = false
    var errorDescription: String? { message }
}

/// The 1Password CLI, run only from the app after the user clicks (never on a timer or a notification).
/// Before a batch, `op` is checked to be signed by AgileBits; it runs with a minimal environment (so a stray
/// service-account token or session can't skip the approval prompt) and values only travel through a pipe.
struct OnePasswordCLI: Sendable {
    /// AgileBits Inc. (1Password) Apple Developer Team ID.
    static let teamID = "2BUA8C4S2C"
    static let readTimeout: TimeInterval = 180
    /// Optional account (sign-in address or ID) for `OP_ACCOUNT`, chosen in Settings › Secrets.
    static let accountKey = "onepassword.account"

    let bin: URL
    let account: String?

    static var savedAccount: String? {
        let a = UserDefaults.standard.string(forKey: accountKey)?.trimmingCharacters(in: .whitespaces) ?? ""
        return a.isEmpty ? nil : a
    }

    private static func candidates() -> [URL] {
        let path = ProcessInfo.processInfo.environment["PATH"] ?? ""
        var list = path.split(separator: ":").filter { $0.hasPrefix("/") }.map { URL(fileURLWithPath: String($0)).appendingPathComponent("op") }
        list += ["/opt/homebrew/bin/op", "/usr/local/bin/op"].map { URL(fileURLWithPath: $0) }
        return list
    }

    /// Find `op` and verify its signature.
    static func find(account: String? = savedAccount) throws -> OnePasswordCLI {
        var isDir: ObjCBool = false
        guard let found = candidates().first(where: { FileManager.default.fileExists(atPath: $0.path, isDirectory: &isDir) && !isDir.boolValue }) else {
            throw OnePasswordError(message: "1Password CLI (op) not found. Install it (brew install 1password-cli) and turn on Settings › Developer › Integrate with 1Password CLI in the 1Password app.", stopsBatch: true)
        }
        let bin = found.resolvingSymlinksInPath()
        try verify(bin)
        return OnePasswordCLI(bin: bin, account: account)
    }

    /// The binary is intact and signed by AgileBits.
    static func verify(_ bin: URL) throws {
        let refuse = { (why: String) in OnePasswordError(message: "\(bin.path) \(why); refusing to run it", stopsBatch: true) }
        var staticCode: SecStaticCode?
        guard SecStaticCodeCreateWithPath(bin as CFURL, [], &staticCode) == errSecSuccess, let code = staticCode else {
            throw refuse("couldn't be checked")
        }
        var requirement: SecRequirement?
        let req = "anchor apple generic and certificate leaf[subject.OU] = \"\(teamID)\""
        guard SecRequirementCreateWithString(req as CFString, [], &requirement) == errSecSuccess else { throw refuse("couldn't be checked") }
        let flags = SecCSFlags(rawValue: kSecCSStrictValidate | kSecCSCheckAllArchitectures | kSecCSCheckNestedCode)
        let status = SecStaticCodeCheckValidity(code, flags, requirement)
        if status == errSecCSReqFailed {
            throw refuse("is not signed by 1Password (\(teamID))")
        }
        guard status == errSecSuccess else { throw refuse("failed code signature verification") }
        var info: CFDictionary?
        guard SecCodeCopySigningInformation(code, SecCSFlags(rawValue: kSecCSSigningInformation), &info) == errSecSuccess,
              let dict = info as? [String: Any] else {
            throw refuse("failed code signature verification")
        }
        let team = dict[kSecCodeInfoTeamIdentifier as String] as? String ?? "none"
        guard team == teamID else { throw refuse("is signed by team \(team), not 1Password (\(teamID))") }
    }

    /// Minimal environment: no OP_SERVICE_ACCOUNT_TOKEN / OP_CONNECT_* / OP_SESSION_*, so every read goes
    /// through the 1Password app and its approval prompt.
    private var environment: [String: String] {
        let mine = ProcessInfo.processInfo.environment
        var env = ["PATH": "/usr/bin:/bin:/usr/sbin:/sbin"]
        for k in ["HOME", "USER", "LOGNAME", "TMPDIR", "LANG", "OP_CONFIG_DIR", "XDG_CONFIG_HOME"] {
            if let v = mine[k] { env[k] = v }
        }
        if env["HOME"] == nil { env["HOME"] = NSHomeDirectory() }
        if env["USER"] == nil { env["USER"] = NSUserName() }
        if let account { env["OP_ACCOUNT"] = account }
        return env
    }

    private func process(_ args: [String]) -> Process {
        let p = Process()
        p.executableURL = bin
        p.arguments = args
        p.environment = environment
        p.standardInput = FileHandle.nullDevice
        return p
    }

    /// `op read --no-newline -- <ref>`, blocking (call off the main thread). The reference is in argv (it
    /// isn't secret); the value only comes back through the stdout pipe.
    func read(_ reference: String) throws -> String {
        let p = process(["read", "--no-newline", "--", reference])
        let out = Pipe(), err = Pipe()
        p.standardOutput = out
        p.standardError = err
        do { try p.run() } catch { throw OnePasswordError(message: "op: \(error.localizedDescription)", stopsBatch: true) }
        let box = PipeBox()
        let group = DispatchGroup()
        DispatchQueue.global(qos: .userInitiated).async(group: group) { box.out = out.fileHandleForReading.readDataToEndOfFile() }
        DispatchQueue.global(qos: .userInitiated).async(group: group) { box.err = err.fileHandleForReading.readDataToEndOfFile() }
        if group.wait(timeout: .now() + Self.readTimeout) == .timedOut {
            p.terminate()
            if group.wait(timeout: .now() + 5) == .timedOut { kill(p.processIdentifier, SIGKILL) }
            p.waitUntilExit()
            box.wipe()
            throw OnePasswordError(message: "\(reference): timed out waiting for 1Password (approve the request in the 1Password app)", stopsBatch: true)
        }
        p.waitUntilExit()
        defer { box.wipe() }
        if p.terminationStatus != 0 {
            throw Self.explain(String(decoding: box.err, as: UTF8.self), reference: reference)
        }
        guard let s = String(data: box.out, encoding: .utf8) else { throw OnePasswordError(message: "\(reference): value isn't UTF-8 text") }
        guard !s.isEmpty else { throw OnePasswordError(message: "\(reference): empty value") }
        return s
    }

    /// End the CLI's app-integration session so a later `op` call needs a new approval. Errors are ignored.
    func signout() {
        let p = process(["signout"])
        p.standardOutput = FileHandle.nullDevice
        p.standardError = FileHandle.nullDevice
        let done = DispatchSemaphore(value: 0)
        p.terminationHandler = { _ in done.signal() }
        guard (try? p.run()) != nil else { return }
        if done.wait(timeout: .now() + 15) == .timedOut { p.terminate() }
    }

    /// Turn `op` errors into something actionable.
    static func explain(_ stderr: String, reference: String) -> OnePasswordError {
        var e = stderr.split(whereSeparator: \.isNewline).map { $0.trimmingCharacters(in: .whitespaces) }.last { !$0.isEmpty } ?? "op failed"
        // "[ERROR] 2024/01/01 12:00:00 message" → "message"
        if e.hasPrefix("[ERROR]") {
            e = e.dropFirst(7).trimmingCharacters(in: .whitespaces)
            let parts = e.split(separator: " ", maxSplits: 2).map(String.init)
            if parts.count == 3, parts[0].contains("/"), parts[1].contains(":") { e = parts[2] }
        }
        let low = stderr.lowercased()
        if low.contains("dismissed") || low.contains("denied") || low.contains("cancel") {
            return OnePasswordError(message: "\(reference): \(e) (the request was declined in 1Password)", stopsBatch: true)
        }
        if low.contains("not signed in") || low.contains("no accounts configured") || low.contains("connecting to desktop app") {
            return OnePasswordError(message: "\(reference): \(e) (open the 1Password app, unlock it, and turn on Settings › Developer › Integrate with 1Password CLI)", stopsBatch: true)
        }
        if low.contains("isn't a vault") || low.contains("isn't an item") || low.contains("could not find") || low.contains("no item") {
            return OnePasswordError(message: "\(reference): \(e) (check the vault, item and field names)")
        }
        if low.contains("multiple accounts") || (low.contains("account") && low.contains("specify")) {
            return OnePasswordError(message: "\(reference): \(e) (pick an account in Settings › Secrets › 1Password)", stopsBatch: true)
        }
        return OnePasswordError(message: "\(reference): \(e)")
    }

    /// Read a 1Password Environment mount (a named pipe the 1Password app serves; opening it asks for
    /// approval), blocking (call off the main thread). Gives up after `readTimeout` instead of hanging.
    static func readMount(_ url: URL) throws -> String {
        let path = url.path
        var st = stat()
        guard stat(path, &st) == 0 else { throw OnePasswordError(message: "\(path): \(String(cString: strerror(errno)))") }
        guard st.st_uid == getuid() else { throw OnePasswordError(message: "\(path) isn't owned by you") }
        let kind = st.st_mode & S_IFMT
        guard kind == S_IFIFO || kind == S_IFREG else { throw OnePasswordError(message: "\(path) isn't a file or 1Password Environment mount") }
        let box = PipeBox()
        let done = DispatchSemaphore(value: 0)
        let limit = 1 << 20
        Thread.detachNewThread {
            defer { done.signal() }
            // open() on the pipe blocks until 1Password approves and starts writing.
            let fd = open(path, O_RDONLY | O_CLOEXEC)
            guard fd >= 0 else { box.error = String(cString: strerror(errno)); return }
            defer { close(fd) }
            var chunk = [UInt8](repeating: 0, count: 16384)
            while box.out.count < limit {
                let n = Darwin.read(fd, &chunk, chunk.count)
                if n < 0 { if errno == EINTR { continue }; box.error = String(cString: strerror(errno)); break }
                if n == 0 { break }
                box.out.append(contentsOf: chunk[0..<n])
            }
            chunk.withUnsafeMutableBufferPointer { $0.update(repeating: 0) }
        }
        if done.wait(timeout: .now() + readTimeout) == .timedOut {
            // Unblock the reader: open the pipe's write end (succeeds only while our reader waits) and close it.
            if kind == S_IFIFO {
                let w = open(path, O_WRONLY | O_NONBLOCK | O_CLOEXEC)
                if w >= 0 { close(w) }
            }
            _ = done.wait(timeout: .now() + 2)
            box.wipe()
            throw OnePasswordError(message: "\(path): timed out waiting for 1Password (approve the request in the 1Password app)", stopsBatch: true)
        }
        defer { box.wipe() }
        if let e = box.error { throw OnePasswordError(message: "\(path): \(e)") }
        guard let s = String(data: box.out, encoding: .utf8) else { throw OnePasswordError(message: "the Environment isn't UTF-8 text") }
        return s
    }
}

/// Buffers filled on reader threads; zeroed when done (best effort — Swift strings can't be wiped).
private final class PipeBox: @unchecked Sendable {
    var out = Data()
    var err = Data()
    var error: String?

    func wipe() {
        out.resetBytes(in: 0..<out.count)
        out = Data()
        err = Data()
    }
}

// MARK: - plan → read → import

/// One 1Password import: a plan from the daemon, the reads it needs (with approval in the 1Password app),
/// then the import. Values live only inside `run()`; they're never stored on this object or shown.
@MainActor
@Observable
final class OnePasswordRun {
    enum Phase: Equatable { case idle, planning, ready, reading, importing, done, failed, cancelled }
    enum ItemState: Equatable {
        case waiting, reading, read, noRead
        case failed(String)
        case skipped
    }

    private(set) var phase: Phase = .idle
    private(set) var plan: OPPlan?
    private(set) var states: [String: ItemState] = [:]
    private(set) var result: OPImportResult?
    private(set) var error: String?
    /// Items that couldn't be read (they stay waiting for review).
    private(set) var failed: [String] = []
    /// Stop passing keys that are gone from the env files.
    var applyRemovals = true
    /// Called after an import (to reload a review, the secrets list…).
    @ObservationIgnored var onFinish: (() async -> Void)?

    @ObservationIgnored private let store: AppStore

    init(store: AppStore) {
        self.store = store
    }

    var busy: Bool { [.planning, .reading, .importing].contains(phase) }
    var finished: Bool { [.done, .failed, .cancelled].contains(phase) }

    func reset() {
        phase = .idle; plan = nil; states = [:]; result = nil; error = nil; failed = []
    }

    /// Ask the daemon for a plan (nothing is read yet). Returns false (with `error` set) on failure.
    @discardableResult
    func prepare(_ body: [String: Any]) async -> Bool {
        reset()
        phase = .planning
        do {
            let p = try await store.opPlan(body)
            plan = p
            for i in p.items { states[i.name] = i.needsRead ? .waiting : .noRead }
            phase = .ready
            return true
        } catch {
            self.error = error.localizedDescription
            phase = .failed
            return false
        }
    }

    /// Prepare, then run straight away (the caller's button is the confirmation).
    func pull(_ body: [String: Any]) async {
        // Values from a paired Mac are never granted on the same click that asked for them: the plan
        // stops at .ready so the review shows where they came from, and a second click imports.
        if await prepare(body), plan?.fromPeers.isEmpty ?? true { await run() }
    }

    /// The user declined: drop the plan.
    func cancel() async {
        if let p = plan, !finished { await store.opCancelPlan(p.id) }
        phase = .cancelled
    }

    /// Read what the plan needs from 1Password, then import. Only call from a user's click.
    func run() async {
        guard let plan, phase == .ready else { return }
        var values: [String: String] = [:]
        defer { values.removeAll() }
        var account: String?
        let reads = plan.reads
        if !reads.isEmpty {
            phase = .reading
            let cli: OnePasswordCLI
            do {
                cli = try await Task.detached(priority: .userInitiated) { try OnePasswordCLI.find() }.value
            } catch {
                await store.opCancelPlan(plan.id)
                self.error = error.localizedDescription
                phase = .failed
                return
            }
            account = cli.account
            var stop = false
            for item in reads {
                if stop { states[item.name] = .skipped; failed.append(item.name); continue }
                states[item.name] = .reading
                let ref = item.ref
                do {
                    let v = try await Task.detached(priority: .userInitiated) { try cli.read(ref) }.value
                    values[item.name] = v
                    states[item.name] = .read
                } catch {
                    let e = error as? OnePasswordError ?? OnePasswordError(message: error.localizedDescription)
                    states[item.name] = .failed(e.message)
                    failed.append(item.name)
                    // A declined prompt or a locked app will fail the rest the same way.
                    stop = e.stopsBatch
                }
            }
            await Task.detached(priority: .utility) { cli.signout() }.value
        }
        let okItems = plan.items.filter { !$0.needsRead || values[$0.name] != nil }.map(\.name)
        if okItems.isEmpty && (plan.removals.isEmpty || !applyRemovals) {
            await store.opCancelPlan(plan.id)
            error = reads.isEmpty ? "Nothing to import." : "Nothing was read from 1Password."
            phase = .failed
            return
        }
        phase = .importing
        var body: [String: Any] = [
            "plan": plan.id,
            "values": values,
            "keys": okItems + plan.removals.map(\.key),
            "applyRemovals": applyRemovals,
            "actor": "app",
        ]
        if let account { body["account"] = account }
        values.removeAll()
        do {
            result = try await store.opImport(body)
            body.removeAll()
            if !failed.isEmpty { error = "Couldn't read \(failed.joined(separator: ", ")) (still waiting for review)." }
            phase = .done
        } catch {
            body.removeAll()
            self.error = error.localizedDescription
            phase = .failed
        }
        await store.refreshSecrets()
        await onFinish?()
    }

    /// One line per thing that happened, for the result panel.
    var resultLines: [String] {
        guard let r = result else { return [] }
        var lines: [String] = []
        if let s = r.stored, !s.isEmpty { lines.append("Stored (encrypted): \(s.joined(separator: ", "))") }
        if let g = r.granted, !g.isEmpty {
            let list = g.keys.sorted().map { k in g[k] == k ? k : "\(k) → \(g[k] ?? "?")" }
            lines.append("Passed to \(plan?.service ?? "the service"): \(list.joined(separator: ", "))")
        }
        if let s = r.removed, !s.isEmpty { lines.append("No longer passed: \(s.joined(separator: ", "))") }
        if let s = r.deleted, !s.isEmpty { lines.append("Forgotten (unused): \(s.joined(separator: ", "))") }
        if let s = r.skipped, !s.isEmpty { lines.append("Skipped: \(s.joined(separator: ", "))") }
        if let s = r.restarting, !s.isEmpty {
            lines.append("Restarting: \(s.joined(separator: ", "))")
        } else if plan?.service != nil, !(r.granted ?? [:]).isEmpty {
            lines.append("Running services pick these up on their next restart.")
        }
        if lines.isEmpty { lines.append("Nothing changed.") }
        return lines
    }
}
