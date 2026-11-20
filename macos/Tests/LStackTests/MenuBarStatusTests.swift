import XCTest
@testable import LStack

final class MenuBarStatusTests: XCTestCase {
    func testAllReportingServicesRunning() throws {
        let services = try [service()]
        let machines = [Machine(name: "Studio", status: "online", services: services)]
        let status = MenuBarStatus(connected: true, localName: "MacBook", services: services, machines: machines)

        XCTAssertEqual(status.symbol, "square.stack.3d.up.fill")
        XCTAssertEqual(status.summary, """
        LStack: all services running
        2 of 2 services running
        MacBook: 1 of 1 running
        Studio: 1 of 1 running
        """)
    }

    func testEveryNonRunningLocalStateWarns() throws {
        for state in ["stopped", "crashed", "errored", "starting", "stopping"] {
            let services = try [service(), service(status: state)]
            let status = MenuBarStatus(connected: true, localName: nil, services: services, machines: [])

            XCTAssertEqual(status.symbol, "exclamationmark.triangle.fill", state)
            XCTAssertTrue(status.summary.contains("This Mac: 1 of 2 running"), state)
        }
    }

    func testEveryNonRunningRemoteStateWarns() throws {
        for state in ["stopped", "crashed", "errored", "starting", "stopping"] {
            let machines = try [Machine(name: "Studio", status: "online", services: [service(status: state)])]
            let status = MenuBarStatus(connected: true, localName: nil, services: [], machines: machines)

            XCTAssertEqual(status.symbol, "exclamationmark.triangle.fill", state)
            XCTAssertTrue(status.summary.contains("Studio: 0 of 1 running"), state)
        }
    }

    func testDiscoveredServicesAffectWarningsAndTotals() throws {
        let services = try [service(), service(status: "stopped", parent: "docker")]
        let local = MenuBarStatus(connected: true, localName: nil, services: services, machines: [])
        let remote = MenuBarStatus(connected: true, localName: nil, services: [], machines: [
            Machine(name: "Studio", status: "online", services: services)
        ])

        for status in [local, remote] {
            XCTAssertEqual(status.symbol, "exclamationmark.triangle.fill")
            XCTAssertTrue(status.summary.contains("1 of 2 services running"))
        }
    }

    func testUnavailableMachinesWarnWithoutCountingCachedServices() throws {
        let cached = try [service(), service(status: "stopped")]
        for state in ["offline", "connecting", "unauthorized", "repair"] {
            let machines = [Machine(name: "Studio", status: state, services: cached)]
            let status = try MenuBarStatus(connected: true, localName: nil, services: [service()], machines: machines)

            XCTAssertEqual(status.symbol, "exclamationmark.triangle.fill", state)
            XCTAssertTrue(status.summary.contains("1 of 1 services running on reporting Macs"), state)
            XCTAssertTrue(status.summary.contains("Studio: \(state); service status unavailable"), state)
            XCTAssertFalse(status.summary.contains("Studio: 1 of 2"), state)
        }
    }

    func testOnlineMachineWithoutSnapshotIsNotAnEmptyMachine() {
        let machines = [Machine(name: "Studio", status: "online")]
        let status = MenuBarStatus(connected: true, localName: nil, services: [], machines: machines)

        XCTAssertEqual(status.symbol, "exclamationmark.triangle.fill")
        XCTAssertTrue(status.summary.contains("0 of 0 services running on reporting Macs"))
        XCTAssertTrue(status.summary.contains("Studio: waiting for service status"))
        XCTAssertFalse(status.summary.contains("Studio: 0 of 0"))
    }

    func testKnownEmptyMachinesDoNotWarn() {
        let status = MenuBarStatus(connected: true, localName: nil, services: [], machines: [
            Machine(name: "Studio", status: "online", services: [])
        ])

        XCTAssertEqual(status.symbol, "square.stack.3d.up.fill")
        XCTAssertTrue(status.summary.contains("LStack: no services"))
        XCTAssertTrue(status.summary.contains("Studio: 0 of 0 running"))
        XCTAssertFalse(status.summary.contains("on reporting Macs"))
    }

    func testDisconnectedDaemonTakesPrecedenceAndHidesCachedTotals() throws {
        let status = try MenuBarStatus(connected: false, localName: nil, services: [service()], machines: [
            Machine(name: "Studio", status: "offline", services: [service(status: "crashed")])
        ])

        XCTAssertEqual(status.symbol, "square.stack.3d.up.slash")
        XCTAssertEqual(status.summary, "LStack: daemon offline\nMonitoring status unavailable")
    }

    func testRecoveryClearsWarningAndRestoresFullTotals() throws {
        var machine = Machine(name: "Studio", status: "offline")
        let unavailable = MenuBarStatus(connected: true, localName: nil, services: [], machines: [machine])
        XCTAssertEqual(unavailable.symbol, "exclamationmark.triangle.fill")

        machine.status = "online"
        machine.services = try [service(status: "crashed")]
        let failed = MenuBarStatus(connected: true, localName: nil, services: [], machines: [machine])
        XCTAssertEqual(failed.symbol, "exclamationmark.triangle.fill")
        XCTAssertTrue(failed.summary.contains("Studio: 0 of 1 running"))

        machine.services = try [service()]
        let recovered = MenuBarStatus(connected: true, localName: nil, services: [], machines: [machine])
        XCTAssertEqual(recovered.symbol, "square.stack.3d.up.fill")
        XCTAssertTrue(recovered.summary.contains("1 of 1 services running"))
        XCTAssertFalse(recovered.summary.contains("on reporting Macs"))
    }

    private func service(status: String = "running", parent: String? = nil) throws -> ServiceState {
        var service = try JSONDecoder().decode(ServiceState.self, from: Data("""
        {
            "name": "service", "type": "process", "autostart": false, "tags": [],
            "status": "\(status)", "health": "none", "restarts": 0,
            "config": { "name": "service", "type": "process" }
        }
        """.utf8))
        service.parent = parent
        return service
    }
}
