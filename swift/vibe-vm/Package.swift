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
    ]
)
