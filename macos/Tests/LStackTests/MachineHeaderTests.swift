import SwiftUI
import XCTest
@testable import LStack

final class MachineHeaderTests: XCTestCase {
    func testAllServicesRunning() throws {
        let services = try Array(repeating: service(status: "running"), count: 10)
        let header = MachineHeader(name: "This Mac", status: "online", detail: nil, services: services)

        XCTAssertEqual(header.serviceSummary, "10 of 10")
        XCTAssertEqual(header.color, .green)
    }

    func testAnyNonRunningServiceWarns() throws {
        let running = try Array(repeating: service(status: "running"), count: 9)
        for status in ["stopped", "crashed", "errored", "starting", "stopping"] {
            let services = try running + [service(status: status)]
            let header = MachineHeader(name: "Other Mac", status: "online", detail: nil, services: services)

            XCTAssertEqual(header.serviceSummary, "9 of 10", status)
            XCTAssertEqual(header.color, .orange, status)
        }
    }

    func testNoServicesRunning() throws {
        let services = try [service(status: "stopped"), service(status: "crashed")]
        let header = MachineHeader(name: "Other Mac", status: "online", detail: nil, services: services)

        XCTAssertEqual(header.serviceSummary, "0 of 2")
        XCTAssertEqual(header.color, .orange)
    }

    func testEmptyAndUnavailableServices() {
        let empty = MachineHeader(name: "This Mac", status: "online", detail: nil, services: [])
        XCTAssertEqual(empty.serviceSummary, "0 of 0")
        XCTAssertEqual(empty.color, .green)

        let unavailable = MachineHeader(name: "Other Mac", status: "online", detail: nil)
        XCTAssertNil(unavailable.serviceSummary)
        XCTAssertEqual(unavailable.color, .green)
    }

    func testConnectionStatusTakesPrecedenceOverServices() throws {
        let colors: [(String, Color)] = [
            ("offline", .gray), ("connecting", .yellow), ("unauthorized", .red), ("repair", .orange)
        ]
        for serviceStatus in ["running", "stopped"] {
            let services = try [service(status: serviceStatus)]
            for (status, color) in colors {
                let header = MachineHeader(name: "Other Mac", status: status, detail: nil, services: services)
                XCTAssertEqual(header.color, color, status)
            }
        }
    }

    private func service(status: String) throws -> ServiceState {
        try JSONDecoder().decode(ServiceState.self, from: Data("""
        {
            "name": "service", "type": "process", "autostart": false, "tags": [],
            "status": "\(status)", "health": "none", "restarts": 0,
            "config": { "name": "service", "type": "process" }
        }
        """.utf8))
    }
}
