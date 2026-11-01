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
        VStack(spacing: 0) {
            Form {
                if let logins {
                    let summary = LoginsSummary(logins)
                    Section {
                        ForEach(summary.rows) { row in
                            LabeledContent {
                                Text(row.account).textSelection(.enabled).lineLimit(1).truncationMode(.middle)
                            } label: {
                                Label(row.tool, systemImage: row.symbol)
                            }
                        }
                        if model.toolbox?.state == .open {
                            Label("The toolbox shell is open. What you log in to there shows here once you exit it.",
                                systemImage: "terminal")
                                .foregroundStyle(.secondary)
                        }
                    } header: {
                        Text(logins.home.map { "Linux home v\($0)" } ?? "Linux home")
                    } footer: {
                        Text(summary.headline).foregroundStyle(.secondary)
                    }
                    Section("Claude") {
                        HStack(spacing: 8) {
                            Image(systemName: summary.needsClaude ? "exclamationmark.circle.fill" : "checkmark.circle.fill")
                                .foregroundStyle(summary.needsClaude ? Palette.attention : Palette.signal)
                                .accessibilityHidden(true)
                            Text(summary.claude).fixedSize(horizontal: false, vertical: true)
                            Spacer()
                            if summary.needsClaude {
                                Button("Log In…") { model.runInTerminal(["setup-token"], exec: true) }
                                    .help("Runs vibe setup-token in Terminal: one browser sign-in, kept in your Keychain")
                            }
                        }
                    }
                } else if let error {
                    ErrorText(error: error)
                } else {
                    ProgressView().frame(maxWidth: .infinity)
                }
                Section {
                } footer: {
                    Text("Linux agents use the logins and dotfiles you set up once in the toolbox shell. Secrets stay "
                        + "in the Linux home; this shows only which logins are there, and whose. Anything else you set "
                        + "up there, such as ~/.aws or ~/.npmrc, reaches agents too.")
                        .foregroundStyle(.secondary)
                }
            }
            .formStyle(.grouped)
            Divider()
            HStack {
                Button("Open Shell") { Task { await model.openToolbox() } }
                    .disabled(!model.toolboxCanOpen)
                Spacer()
                Button("Refresh") { Task { await load() } }
            }
            .padding(14)
        }
        .frame(width: 480, height: 440)
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
