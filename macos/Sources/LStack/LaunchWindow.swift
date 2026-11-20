import Foundation

enum LaunchWindow: String {
    case setup, panel

    static func initial(needsSetup: Bool,
                        arguments: [String] = ProcessInfo.processInfo.arguments,
                        environment: [String: String] = ProcessInfo.processInfo.environment) -> LaunchWindow? {
        guard environment["LSTACK_SNAPSHOT"] == nil else { return nil }
        // The existing login agent runs the executable directly, without LaunchServices.
        guard environment["XPC_SERVICE_NAME"] != "dev.lstack.menubar",
              !arguments.contains("--background") else { return nil }
        return needsSetup ? .setup : .panel
    }
}
