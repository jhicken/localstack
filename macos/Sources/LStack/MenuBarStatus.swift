struct MenuBarStatus {
    let symbol: String
    let summary: String

    init(connected: Bool, localName: String?, services: [ServiceState], machines: [Machine]) {
        guard connected else {
            symbol = "square.stack.3d.up.slash"
            summary = "LStack: daemon offline\nMonitoring status unavailable"
            return
        }

        var running = services.filter(\.isRunning).count
        var total = services.count
        var needsAttention = running < total
        var lines = ["\(localName ?? "This Mac"): \(running) of \(total) running"]
        var partial = false

        for machine in machines {
            guard machine.online else {
                needsAttention = true
                partial = true
                lines.append("\(machine.name): \(machine.status); service status unavailable")
                continue
            }
            guard let services = machine.services else {
                needsAttention = true
                partial = true
                lines.append("\(machine.name): waiting for service status")
                continue
            }
            let up = services.filter(\.isRunning).count
            running += up
            total += services.count
            needsAttention = needsAttention || up < services.count
            lines.append("\(machine.name): \(up) of \(services.count) running")
        }

        symbol = needsAttention ? "exclamationmark.triangle.fill" : "square.stack.3d.up.fill"
        let headline = needsAttention ? "attention needed" : total == 0 ? "no services" : "all services running"
        summary = ([
            "LStack: \(headline)",
            "\(running) of \(total) services running\(partial ? " on reporting Macs" : "")"
        ] + lines).joined(separator: "\n")
    }
}
