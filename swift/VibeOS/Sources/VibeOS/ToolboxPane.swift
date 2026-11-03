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
        let summary = ToolboxSummary(model.toolbox)
        Form {
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
            if available {
                logInsSection
            }
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
            } else if error == nil {
                ProgressView().frame(maxWidth: .infinity)
            }
            ErrorText(error: error)
        } header: {
            Text("Logins")
        } footer: {
            if let summary {
                VStack(alignment: .leading, spacing: 4) {
                    if !summary.rows.isEmpty { Text(summary.headline) }
                    if let hint = summary.signInHint { Text(hint) }
                }
                .foregroundStyle(.secondary)
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
