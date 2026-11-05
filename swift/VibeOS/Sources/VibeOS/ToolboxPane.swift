import SwiftUI
import VibeMenu
import VibeRPC

/// Settings → Linux Toolbox (PRD: The toolbox shell): the shell the Linux
/// template and home are made in, what it's doing now (open in Terminal,
/// or in the background with none attached), Save & Close, and the logins
/// new Linux agents start with. The Linux home's logins were noted when
/// the toolbox saved it, so reading them boots nothing.
struct ToolboxPane: View {
    @Environment(\.openWindow) private var openWindow
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
        // And after a sign-in, which may leave the toolbox just as it was.
        .task(id: model.settingsRevision) { await loadLogins() }
    }

    private var toolbox: some View {
        let summary = ToolboxSummary(model.toolbox)
        // vibed says it's saving only once the shell has gone; until then,
        // the click is what's known.
        let saving = model.toolboxClosing && !summary.working
        return Form {
            Section {
                LabeledContent("Shell") {
                    HStack(spacing: 6) {
                        if summary.working || saving { ProgressView().controlSize(.small) }
                        Text(saving ? "Saving…" : summary.state)
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
                ErrorText(error: model.toolboxCloseError)
            } footer: {
                Text("A Linux shell in Terminal. What you install or log in to there, every new Linux agent "
                    + "starts with. Exit the shell or close its window to save; Control-\\ leaves it open. "
                    + "Pi signs in from Settings → Agents, with no shell at all.")
                    .foregroundStyle(.secondary)
            }
            logInsSection
            Section {
                HStack {
                    // VibeOS moves the template by itself. This is for when that would
                    // lose something, or to apply toolbox.toml now. In Terminal, as it
                    // can take many minutes and says how it goes.
                    if !(model.toolbox?.rebuildHeld ?? []).isEmpty {
                        Button("Move and Open Shell") { model.moveTemplateAndOpenShell() }
                            .disabled(model.toolbox?.state != .closed)
                            .help("Moves the template to the new base image, then opens the shell to put back what it lacks")
                    } else {
                        Button("Rebuild from toolbox.toml", action: rebuild)
                            .disabled(model.toolbox?.state != .closed)
                            .help("Makes a fresh template on the newest base image from toolbox.toml, in Terminal")
                    }
                    Spacer()
                    if summary.closable || model.toolboxClosing {
                        Button("Save & Close", action: saveAndClose)
                            .disabled(model.toolboxClosing)
                            .help("Ends the shell and saves the template and Linux home, as exiting does")
                    }
                    Button("Open Shell") { Task { await model.openToolbox() } }
                        .buttonStyle(.borderedProminent)
                        .disabled(!model.toolboxCanOpen)
                        .help(summary.closable ? "Shows the shell in Terminal" : "Opens the shell in Terminal")
                }
            }
        }
        .formStyle(.grouped)
    }

    /// Always asks: what's in the template and not in toolbox.toml may not come along.
    private func rebuild() {
        let detail = "New Linux agents get a fresh template on the newest base image, from toolbox.toml. VibeOS "
            + "puts back the Debian and npm packages you installed by hand; anything else it can't, and it says what."
        guard confirm("Rebuild the template?", detail: detail, action: "Rebuild") else { return }
        model.runInTerminal(["toolbox", "rebuild"], exec: false)
    }

    private func signIn(_ what: SignInWhat) {
        model.signInRequest = SignInRequest(what: what)
        show(WindowID.signIn, with: openWindow)
    }

    /// Always asks: a detached shell is how a long install is left running,
    /// and what it has done so far would become the new template.
    private func saveAndClose() {
        let detail = "The shell ends, and anything still running in it stops. The template and Linux home are "
            + "saved as they are now, for new Linux agents."
        guard confirm("Save and close the toolbox?", detail: detail, action: "Save & Close") else { return }
        Task { await model.closeToolbox() }
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
                if !(logins?.logins ?? []).contains(where: { $0.tool == .gh }) {
                    LabeledContent {
                        Button("Sign In…") { signIn(.toolboxGithub) }
                            .disabled(model.toolbox?.state != .closed)
                            .help("One browser sign-in, kept in the Linux home")
                    } label: {
                        Text("Linux agents aren't signed in to GitHub.").foregroundStyle(.secondary)
                    }
                }
                // Agents' own sign-in: a hint, and a way to do it, no more.
                if let hint = summary.signInHint {
                    LabeledContent {
                        Button("Sign In…") { signIn(.toolboxClaude) }
                            .help("One browser sign-in, kept in your Keychain")
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
