import AppKit
import SwiftUI
import VibeRPC

/// The menu bar app: UI only, holding no state. Everything it shows comes
/// from vibed over the socket. For now that's the daemon's hello.
@main
struct VibeOSApp: App {
    @StateObject private var daemon = DaemonModel()

    var body: some Scene {
        MenuBarExtra {
            MenuView(daemon: daemon)
        } label: {
            Image(systemName: daemon.isConnected ? "square.stack.3d.up.fill" : "square.stack.3d.up.slash")
        }
        .menuBarExtraStyle(.window)
    }
}

struct MenuView: View {
    @ObservedObject var daemon: DaemonModel

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("VibeOS").font(.headline)
            Text(daemon.summary)
                .font(.callout)
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
            Divider()
            HStack {
                Button("Refresh") {
                    Task { await daemon.refresh() }
                }
                Spacer()
                // Quitting the menu leaves vibed and every agent running.
                Button("Quit Menu") {
                    NSApplication.shared.terminate(nil)
                }
            }
        }
        .padding(12)
        .frame(width: 320)
    }
}

/// vibed's state as the menu shows it, refreshed every few seconds.
@MainActor
final class DaemonModel: ObservableObject {
    enum Status: Equatable {
        case connecting
        case connected(HelloResult)
        case unavailable(String)
    }

    @Published private(set) var status: Status = .connecting
    private let client: VibedClient

    init(client: VibedClient = VibedClient()) {
        self.client = client
        // Says hello at launch, then keeps the menu current. The loop ends
        // once the model is gone.
        Task { [weak self] in
            while self != nil {
                await self?.refresh()
                try? await Task.sleep(for: .seconds(10))
            }
        }
    }

    var isConnected: Bool {
        if case .connected = status { true } else { false }
    }

    var summary: String {
        switch status {
        case .connecting:
            "Connecting to vibed…"
        case .connected(let hello):
            "vibed \(hello.version) · protocol \(hello.protocolVersion) · \(hello.os)/\(hello.arch) · pid \(hello.pid)"
        case .unavailable(let why):
            "vibed is unavailable: \(why)"
        }
    }

    func refresh() async {
        let client = self.client
        // The client blocks on the socket, so it runs off the main actor.
        let outcome = await Task.detached { () -> Result<HelloResult, VibedClientError> in
            do {
                return .success(try client.hello(client: "VibeOS.app"))
            } catch let error as VibedClientError {
                return .failure(error)
            } catch {
                return .failure(.badResponse("\(error)"))
            }
        }.value
        switch outcome {
        case .success(let hello): status = .connected(hello)
        case .failure(let error): status = .unavailable(error.description)
        }
    }
}
