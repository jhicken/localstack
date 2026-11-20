import XCTest
@testable import LStack

final class LaunchWindowTests: XCTestCase {
    func testManualLaunchAfterSetupShowsServices() {
        XCTAssertEqual(LaunchWindow.initial(needsSetup: false, arguments: [], environment: [:]), .panel)
    }

    func testFirstManualLaunchShowsSetup() {
        XCTAssertEqual(LaunchWindow.initial(needsSetup: true, arguments: [], environment: [:]), .setup)
    }

    func testLaunchServicesManualLaunchIsNotTheLoginAgent() {
        let environment = ["XPC_SERVICE_NAME": "application.dev.lstack.menubar.123.456"]
        XCTAssertEqual(LaunchWindow.initial(needsSetup: false, arguments: [], environment: environment), .panel)
    }

    func testLoginAgentStaysInBackground() {
        for needsSetup in [true, false] {
            XCTAssertNil(LaunchWindow.initial(needsSetup: needsSetup, arguments: [], environment: [
                "XPC_SERVICE_NAME": "dev.lstack.menubar"
            ]))
        }
    }

    func testExplicitBackgroundLaunchDoesNotOpenAWindow() {
        for needsSetup in [true, false] {
            XCTAssertNil(LaunchWindow.initial(needsSetup: needsSetup, arguments: ["LStack", "--background"], environment: [:]))
        }
    }

    func testSnapshotDoesNotOpenAnInteractiveWindow() {
        for needsSetup in [true, false] {
            XCTAssertNil(LaunchWindow.initial(needsSetup: needsSetup, arguments: [], environment: [
                "LSTACK_SNAPSHOT": "/tmp/lstack-snapshot.png"
            ]))
        }
    }
}
