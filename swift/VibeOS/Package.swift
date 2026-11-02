// swift-tools-version: 6.0
import Foundation
import PackageDescription

// The menu's targets build in the Swift 5 language mode: AppKit's and
// UserNotifications' delegate and scripting APIs aren't annotated for
// Swift 6's checks yet. Their state lives on the main actor regardless.
let menuSettings: [SwiftSetting] = [.swiftLanguageMode(.v5)]

// Sparkle, for a release's updates (Updater.swift), only when
// scripts/release-macos.sh asks with VIBEOS_SPARKLE=1: every other build
// has nothing to fetch, and no updater. The app finds the framework in its
// bundle's Contents/Frameworks.
let sparkle = ProcessInfo.processInfo.environment["VIBEOS_SPARKLE"] == "1"
var packages: [Package.Dependency] = []
var appDependencies: [Target.Dependency] = ["VibeRPC", "VibeMenu"]
var appLinking: [LinkerSetting] = []
if sparkle {
    packages.append(.package(url: "https://github.com/sparkle-project/Sparkle", exact: "2.10.0"))
    appDependencies.append(.product(name: "Sparkle", package: "Sparkle"))
    appLinking.append(.unsafeFlags(["-Xlinker", "-rpath", "-Xlinker", "@executable_path/../Frameworks"]))
}

let package = Package(
    name: "VibeOS",
    platforms: [.macOS("26.0")],
    products: [
        .executable(name: "VibeOS", targets: ["VibeOS"]),
        .library(name: "VibeRPC", targets: ["VibeRPC"]),
    ],
    dependencies: packages,
    targets: [
        // The vibed socket contract and client; see protocol/README.md.
        .target(name: "VibeRPC"),
        // What the menu shows and does, apart from its views: the model,
        // Terminal attach and notifications, so tests can drive them.
        .target(name: "VibeMenu", dependencies: ["VibeRPC"], swiftSettings: menuSettings),
        // The menu bar app. scripts/sign-macos.sh wraps it in VibeOS.app.
        .executableTarget(
            name: "VibeOS",
            dependencies: appDependencies,
            swiftSettings: menuSettings,
            linkerSettings: appLinking),
        .testTarget(name: "VibeRPCTests", dependencies: ["VibeRPC"]),
        .testTarget(name: "VibeMenuTests", dependencies: ["VibeMenu", "VibeRPC"], swiftSettings: menuSettings),
    ]
)
