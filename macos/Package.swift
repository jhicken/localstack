// swift-tools-version: 5.10
import PackageDescription

let package = Package(
    name: "LStack",
    platforms: [.macOS(.v14)],
    targets: [
        .executableTarget(name: "LStack", path: "Sources/LStack")
    ]
)
