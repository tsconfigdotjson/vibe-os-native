// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "VibeOS",
    platforms: [.macOS("26.0")],
    products: [
        .executable(name: "VibeOS", targets: ["VibeOS"]),
        .library(name: "VibeRPC", targets: ["VibeRPC"]),
    ],
    targets: [
        // The vibed socket contract and client; see protocol/README.md.
        .target(name: "VibeRPC"),
        // The menu bar app. scripts/sign-macos.sh wraps it in VibeOS.app.
        .executableTarget(name: "VibeOS", dependencies: ["VibeRPC"]),
        .testTarget(name: "VibeRPCTests", dependencies: ["VibeRPC"]),
    ]
)
