import AppKit
import CryptoKit
import Foundation
import LocalAuthentication

/// A Touch ID approval the daemon is waiting on (a changed service holding secrets, or a console into one).
struct Approval: Codable, Hashable {
    let id: String?
    let kind: String?
    let service: String?
    let summary: [String]?
    let message: String?
    let status: String?
    /// Remote copies only say that one is pending.
    let pending: Bool?
}

struct ApprovalsStatus: Decodable {
    let enabled: Bool
    let publicKey: String?
    let approved: [String]
    let pending: [Approval]
}

/// Holds this Mac's approval key: a P-256 key inside the Secure Enclave that signs only after Touch ID
/// (or your login password). The key never leaves the chip; this file is just an encrypted handle to it
/// that only this Mac's Secure Enclave can use. The daemon knows the public key and checks each signature.
@MainActor
final class Approver {
    static let shared = Approver()
    private var shown = Set<String>()
    private var queue: [Approval] = []
    private var busy = false
    private(set) var problem: String?

    private var keyURL: URL {
        let dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent("LStack", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        return dir.appendingPathComponent("approval-key")
    }

    private func loadOrCreate(_ ctx: LAContext? = nil) throws -> SecureEnclave.P256.Signing.PrivateKey {
        if let blob = try? Data(contentsOf: keyURL) {
            return try SecureEnclave.P256.Signing.PrivateKey(dataRepresentation: blob, authenticationContext: ctx)
        }
        var err: Unmanaged<CFError>?
        guard let access = SecAccessControlCreateWithFlags(nil, kSecAttrAccessibleWhenUnlockedThisDeviceOnly, [.privateKeyUsage, .userPresence], &err) else {
            throw err!.takeRetainedValue() as Error
        }
        let key = try SecureEnclave.P256.Signing.PrivateKey(accessControl: access)
        FileManager.default.createFile(atPath: keyURL.path, contents: key.dataRepresentation, attributes: [.posixPermissions: 0o600])
        return key
    }

    private static func hex(_ d: Data) -> String { d.map { String(format: "%02x", $0) }.joined() }

    /// Register this Mac's key with the daemon (the first one is trusted; it can't be swapped silently).
    func register(_ client: DaemonClient) async -> ApprovalsStatus? {
        guard SecureEnclave.isAvailable else {
            problem = "This Mac has no Secure Enclave, so Touch ID approvals are off."
            return nil
        }
        do {
            let pk = Self.hex(try loadOrCreate().publicKey.x963Representation)
            let st = try await client.send("POST", "/api/approvals/key", body: ["publicKey": pk], as: ApprovalsStatus.self)
            problem = nil
            for a in st.pending { enqueue(a, client) }
            return st
        } catch DaemonError.http(409, let msg) where msg.contains("different approval key") {
            problem = "LStack's background service trusts a different approval key, so approvals from this app won't be accepted. See README → Touch ID approvals to reset."
        } catch DaemonError.http(409, _) {
            problem = nil // store locked; try again later
        } catch {
            problem = "Touch ID approvals: \(error.localizedDescription)"
        }
        return nil
    }

    func enqueue(_ a: Approval, _ client: DaemonClient) {
        guard let id = a.id, a.message != nil, !shown.contains(id) else { return }
        shown.insert(id)
        queue.append(a)
        Task { await drain(client) }
    }

    private func drain(_ client: DaemonClient) async {
        guard !busy else { return }
        busy = true
        defer { busy = false }
        while !queue.isEmpty {
            let a = queue.removeFirst()
            // It may have been answered (or replaced) meanwhile.
            if let now = try? await client.send("GET", "/api/approvals/\(a.id!)", as: Approval.self), now.status != "pending" { continue }
            await ask(a, client)
        }
    }

    private func ask(_ a: Approval, _ client: DaemonClient) async {
        let svc = a.service ?? "a service"
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = a.kind == "console" ? "Allow a console into \(svc)?" : "Approve changes to \(svc)?"
        let lines = (a.summary ?? []).map { "• \($0)" }.joined(separator: "\n")
        alert.informativeText = (a.kind == "console"
            ? "Something asked to type into or run commands inside \(svc), which holds secrets.\n\n"
            : "\(svc) holds secrets. It gets them only after you approve what changed:\n\n") + lines
            + "\n\nIf you didn't just do this, choose Deny."
        alert.addButton(withTitle: "Approve with Touch ID…")
        alert.addButton(withTitle: "Deny")
        alert.addButton(withTitle: "Later")
        NSApp.activate(ignoringOtherApps: true)
        switch alert.runModal() {
        case .alertFirstButtonReturn:
            await sign(a, client)
        case .alertSecondButtonReturn:
            _ = try? await client.send("POST", "/api/approvals/\(a.id!)/deny", as: Approval.self)
        default:
            shown.remove(a.id!) // asked again next time it comes up
        }
    }

    private func sign(_ a: Approval, _ client: DaemonClient) async {
        let svc = a.service ?? "a service"
        let reason = a.kind == "console" ? "allow a console into \(svc)" : "approve changes to \(svc), which holds secrets"
        let message = Data(a.message!.utf8)
        let url = keyURL
        let result: Result<String, Error> = await Task.detached {
            do {
                let ctx = LAContext()
                ctx.localizedReason = reason
                guard let blob = try? Data(contentsOf: url) else { throw CocoaError(.fileNoSuchFile) }
                let key = try SecureEnclave.P256.Signing.PrivateKey(dataRepresentation: blob, authenticationContext: ctx)
                return .success(Self.hexNonisolated(try key.signature(for: message).derRepresentation))
            } catch { return .failure(error) }
        }.value
        switch result {
        case .success(let sig):
            do {
                _ = try await client.send("POST", "/api/approvals/\(a.id!)", body: ["signature": sig], as: Approval.self)
            } catch {
                alert("Couldn't approve", error.localizedDescription)
            }
        case .failure(let e):
            if (e as? LAError)?.code == .userCancel || (e as NSError).code == errSecUserCanceled { shown.remove(a.id!); return }
            alert("Touch ID failed", e.localizedDescription)
            shown.remove(a.id!)
        }
    }

    nonisolated private static func hexNonisolated(_ d: Data) -> String { d.map { String(format: "%02x", $0) }.joined() }

    private func alert(_ title: String, _ text: String) {
        let al = NSAlert()
        al.messageText = title
        al.informativeText = text
        al.runModal()
    }
}
