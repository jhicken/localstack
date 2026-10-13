import AppKit
import Foundation

/// The bundled Rust `lstack` binary (daemon + CLI) and the self-install steps it performs.
enum Helper {
    /// LStack.app/Contents/Helpers/lstack, falling back to an `lstack` on disk for dev builds.
    static var url: URL? {
        let bundled = Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/lstack")
        var candidates = [bundled.path]
        if let b = DaemonClient.readInfo()?.bin { candidates.append(b) }
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        candidates += ["\(home)/.local/bin/lstack", "/usr/local/bin/lstack", "/opt/homebrew/bin/lstack"]
        return candidates.first { FileManager.default.isExecutableFile(atPath: $0) }.map { URL(fileURLWithPath: $0) }
    }

    static var isBundled: Bool {
        FileManager.default.isExecutableFile(atPath: Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/lstack").path)
    }

    /// Apps run from Downloads or a mounted DMG get translocated; login items would point at a path that vanishes.
    static var inApplications: Bool {
        let p = Bundle.main.bundleURL.resolvingSymlinksInPath().path
        return p.hasPrefix("/Applications/") || p.hasPrefix(FileManager.default.homeDirectoryForCurrentUser.path + "/Applications/")
    }

    struct Result: Sendable {
        let status: Int32
        let output: String
    }

    /// Run the helper off the main thread.
    static func run(_ args: [String]) async -> Result {
        guard let url else { return Result(status: 127, output: "lstack helper not found") }
        return await withCheckedContinuation { cont in
            DispatchQueue.global().async {
                let p = Process()
                p.executableURL = url
                p.arguments = args
                var env = ProcessInfo.processInfo.environment
                env["NO_COLOR"] = "1"
                // Apps launched from Finder get a minimal PATH; make docker & common toolchains reachable.
                let extra = ["/usr/local/bin", "/opt/homebrew/bin", "\(NSHomeDirectory())/.local/bin"]
                let path = (env["PATH"] ?? "/usr/bin:/bin:/usr/sbin:/sbin").split(separator: ":").map(String.init)
                env["PATH"] = (path + extra.filter { !path.contains($0) }).joined(separator: ":")
                p.environment = env
                let pipe = Pipe()
                p.standardOutput = pipe
                p.standardError = pipe
                p.standardInput = FileHandle.nullDevice
                do { try p.run() } catch {
                    cont.resume(returning: Result(status: 126, output: error.localizedDescription))
                    return
                }
                let data = pipe.fileHandleForReading.readDataToEndOfFile()
                p.waitUntilExit()
                cont.resume(returning: Result(status: p.terminationStatus, output: String(decoding: data, as: UTF8.self)))
            }
        }
    }

    static func status() async -> SetupStatus? {
        let r = await run(["setup", "status", "--json"])
        guard r.status == 0, let data = r.output.data(using: .utf8) else { return nil }
        return try? JSONDecoder().decode(SetupStatus.self, from: data)
    }

    /// Symlink the CLI into /usr/local/bin with an admin prompt (for users who want it system-wide).
    static func installCLIWithAdmin() -> String? {
        guard let url else { return "lstack helper not found" }
        let target = url.path.replacingOccurrences(of: "'", with: "'\\''")
        let cmd = "mkdir -p /usr/local/bin && ln -sf '\(target)' /usr/local/bin/lstack"
        let script = "do shell script \"\(cmd.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\""))\" with administrator privileges"
        var err: NSDictionary?
        NSAppleScript(source: script)?.executeAndReturnError(&err)
        if let err, (err[NSAppleScript.errorNumber] as? Int) != -128 {
            return err[NSAppleScript.errorMessage] as? String ?? "install failed"
        }
        return nil
    }

    /// Copy this app into /Applications, launch the copy, and quit.
    @MainActor
    static func moveToApplications() -> String? {
        let dest = URL(fileURLWithPath: "/Applications/LStack.app")
        let fm = FileManager.default
        do {
            if fm.fileExists(atPath: dest.path) { try fm.trashItem(at: dest, resultingItemURL: nil) }
            try fm.copyItem(at: Bundle.main.bundleURL, to: dest)
        } catch {
            return error.localizedDescription
        }
        let cfg = NSWorkspace.OpenConfiguration()
        cfg.createsNewApplicationInstance = true
        NSWorkspace.shared.openApplication(at: dest, configuration: cfg) { _, _ in
            Task { @MainActor in NSApp.terminate(nil) }
        }
        return nil
    }
}

struct SetupStatus: Codable, Sendable {
    struct Cli: Codable, Sendable {
        let installed: Bool
        let path: String?
        let onPath: Bool
        let shadowedBy: String?
    }
    struct Login: Codable, Sendable {
        let installed: Bool
        let appInstalled: Bool
        let loaded: Bool
        let pid: Int?
        let program: String?
        let current: Bool
    }
    struct Daemon: Codable, Sendable {
        let running: Bool
        let pid: Int?
        let port: Int?
        let version: String?
        /// The daemon is running an older binary than this app (e.g. after an in-place update).
        let stale: Bool?
        let staleReason: String?
    }
    let version: String
    let bin: String
    let app: String?
    let home: String
    let cli: Cli
    let login: Login
    let daemon: Daemon
}
