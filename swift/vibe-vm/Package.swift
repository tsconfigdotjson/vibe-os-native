// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "vibe-vm",
    platforms: [.macOS("26.0")],
    products: [
        .executable(name: "vibe-vm", targets: ["vibe-vm"])
    ],
    targets: [
        .target(
            name: "VibeVMKit",
            linkerSettings: [
                .linkedFramework("Virtualization"),
                .linkedFramework("Security"),
            ]),
        // Must be signed with vibe-vm.entitlements to run VMs; see
        // scripts/sign-macos.sh.
        .executableTarget(name: "vibe-vm", dependencies: ["VibeVMKit"]),
        .testTarget(name: "VibeVMKitTests", dependencies: ["VibeVMKit"]),
    ],
    // Virtualization.framework's classes aren't annotated for Swift 6's
    // strict concurrency checks. vibe-vm keeps every one of them on the main
    // actor, as the framework requires, and builds in the Swift 5 mode so the
    // checks it can't satisfy stay warnings.
    swiftLanguageModes: [.v5]
)
