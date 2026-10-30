import Foundation
import VibeVMKit

func parseCommand() -> Command {
    do {
        return try VibeVM.parse(Array(CommandLine.arguments.dropFirst()))
    } catch {
        FileHandle.standardError.write(Data("vibe-vm: \(error)\n\n\(VibeVM.usage)\n".utf8))
        exit(2)
    }
}

switch parseCommand() {
case .version:
    print("vibe-vm \(VibeVM.version)")
case .help:
    print(VibeVM.usage)
case .check:
    let support = HostSupport.current()
    print(support.json)
    exit(support.ready ? 0 : 1)
}
