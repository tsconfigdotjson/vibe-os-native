import Foundation
import VibeVMKit

let processStart = ContinuousClock.now

func parseCommand() -> Command {
    do {
        return try VibeVM.parse(Array(CommandLine.arguments.dropFirst()))
    } catch {
        FileHandle.standardError.write(Data("vibe-vm: \(error)\n\n\(VibeVM.usage)\n".utf8))
        exit(2)
    }
}

MainActor.assumeIsolated {
    switch parseCommand() {
    case .version:
        print("vibe-vm \(VibeVM.version)")
    case .help:
        print(VibeVM.usage)
    case .check:
        let support = HostSupport.current()
        print(support.json)
        exit(support.ready ? 0 : 1)
    case .create(let config):
        exit(Runner.create(configPath: config))
    case .boot(let config, let socket):
        Runner.launch(.boot, configPath: config, socketPath: socket, since: processStart)
    case .restore(let config, let socket):
        Runner.launch(.restore, configPath: config, socketPath: socket, since: processStart)
    }
}
