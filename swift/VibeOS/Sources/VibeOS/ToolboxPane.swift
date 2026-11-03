import SwiftUI
import VibeMenu
import VibeRPC

/// Settings → Linux Toolbox (PRD: The toolbox shell): the shell the Linux
/// template and home are made in, what it's doing now, and the logins new
/// Linux agents start with. The Linux home's logins were noted when the
/// toolbox saved it, so reading them boots nothing.
struct ToolboxPane: View {
    @ObservedObject var model: MenuModel
    @State private var logins: ToolboxLogins?
    @State private var error: String?

    private var available: Bool { model.toolbox?.state != .unavailable }

    var body: some View {
        Group {
            if available {
                toolbox
            } else {
                // Nothing to open or rebuild, and it says why.
                ContentUnavailableView {
                    Label("No Linux Toolbox", systemImage: "shippingbox")
                } description: {
                    Text("The toolbox comes with Linux agents in VMs on a Mac, and this one has none. On a "
                        + "Linux host, install tools on the server itself.")
                }
            }
        }
        .navigationTitle("Linux Toolbox")
        // What vibed says now, while this shows: the shell opens and exits
        // in Terminal, out of sight.
        .task {
            repeat {
                await model.refreshToolbox()
                try? await Task.sleep(for: .seconds(3))
            } while !Task.isCancelled
        }
        // Again after each change, as a save notes a new home's logins.
        .task(id: model.toolbox) { await loadLogins() }
    }

    private var toolbox: some View {
        let summary = ToolboxSummary(model.toolbox)
        return Form {
            Section {
                LabeledContent("Shell") {
                    HStack(spacing: 6) {
                        if summary.working { ProgressView().controlSize(.small) }
                        Text(summary.state)
                    }
                }
                if let versions = summary.versions {
                    LabeledContent("New agents boot", value: versions)
                }
                ForEach(summary.notes, id: \.self) { note in
                    Text(note)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            } footer: {
                Text("A Linux shell in Terminal. What you install or log in to there, every new Linux agent "
                    + "starts with. Exit the shell to save.")
                    .foregroundStyle(.secondary)
            }
            logInsSection
            Section {
                HStack {
                    // In Terminal, as it can take many minutes and says how it goes.
                    Button("Rebuild Template") { model.runInTerminal(["toolbox", "rebuild"], exec: false) }
                        .disabled(model.toolbox?.state != .closed)
                        .help("Replays toolbox.toml onto the newest base image, in Terminal")
                    Spacer()
                    Button("Open Shell") { Task { await model.openToolbox() } }
                        .buttonStyle(.borderedProminent)
                        .disabled(!model.toolboxCanOpen)
                }
            }
        }
        .formStyle(.grouped)
    }

    @ViewBuilder private var logInsSection: some View {
        let summary = logins.map(LoginsSummary.init)
        Section {
            if let summary {
                ForEach(summary.rows) { row in
                    LabeledContent {
                        Text(row.account).textSelection(.enabled).lineLimit(1).truncationMode(.middle)
                    } label: {
                        Label(row.tool, systemImage: row.symbol)
                    }
                }
                if summary.rows.isEmpty {
                    Text(summary.headline).foregroundStyle(.secondary)
                }
                // Agents' own sign-in: a hint, and a way to do it, no more.
                if let hint = summary.signInHint {
                    LabeledContent {
                        Button("Sign In…") { model.runInTerminal(["setup-token"], exec: true) }
                            .help("Runs vibe setup-token in Terminal: one browser sign-in, kept in your Keychain")
                    } label: {
                        Text(hint).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                    }
                }
            } else if error == nil {
                ProgressView().frame(maxWidth: .infinity)
            }
            ErrorText(error: error)
        } header: {
            Text("Logins")
        } footer: {
            if let summary, !summary.rows.isEmpty {
                Text(summary.headline).foregroundStyle(.secondary)
            }
        }
    }

    private func loadLogins() async {
        guard available else { return }
        do {
            logins = try await model.toolboxLogins()
            error = nil
        } catch {
            self.error = describe(error)
        }
    }
}
