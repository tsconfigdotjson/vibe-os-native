import AppKit
import SwiftUI
import VibeMenu
import VibeRPC

/// Linux Toolbox → See Logins… (PRD: Menu bar, Footer): what every new
/// linux agent starts logged in to. The Linux home's logins were noted when
/// the toolbox saved it, and Claude's lives on the Mac, so reading them
/// boots nothing.
struct LoginsView: View {
    @ObservedObject var model: MenuModel
    @State private var logins: ToolboxLogins?
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Linux agents use the logins and dotfiles you set up once in the Linux toolbox. "
                + "Secrets stay in the Linux home: this shows only which logins are there, and whose. "
                + "Anything else you set up there, such as ~/.aws or ~/.npmrc, reaches agents too.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            if let logins {
                let summary = LoginsSummary(logins)
                Text(summary.headline).fixedSize(horizontal: false, vertical: true)
                if !summary.rows.isEmpty {
                    Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 6) {
                        ForEach(summary.rows) { row in
                            GridRow {
                                Text(row.tool).foregroundStyle(.secondary)
                                Text(row.account).textSelection(.enabled)
                            }
                        }
                    }
                    .padding(.leading, 8)
                }
                if model.toolbox?.state == .open {
                    Text("The toolbox shell is open now: what you log in to there shows here once you exit it.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                Divider()
                HStack {
                    Text(summary.claude)
                    if summary.needsClaude {
                        Button("Log In to Claude for Linux Agents…") {
                            model.runInTerminal(["setup-token"], exec: true)
                        }
                    }
                }
            } else if error == nil {
                ProgressView().controlSize(.small)
            }
            ErrorText(error: error)
            HStack {
                Button("Open Shell") { Task { await model.openToolbox() } }
                    .disabled(!model.toolboxCanOpen)
                Spacer()
                Button("Refresh") { Task { await load() } }
            }
        }
        .padding()
        .frame(width: 460)
        // Again after each toolbox change, as a save notes a new home's,
        // and on coming back to the app, as from `vibe setup-token`.
        .task(id: model.toolbox) { await load() }
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
            Task { await load() }
        }
    }

    private func load() async {
        do {
            logins = try await model.toolboxLogins()
            error = nil
        } catch {
            self.error = describe(error)
        }
    }
}
