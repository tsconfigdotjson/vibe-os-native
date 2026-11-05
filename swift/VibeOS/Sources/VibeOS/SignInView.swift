import AppKit
import SwiftUI
import VibeMenu
import VibeRPC

/// One click for a sign-in (PRD: Who gets which credentials): vibed runs the provider's
/// own flow on the host that's to have the login, and this opens its page in the Mac's
/// browser and shows the code that goes with it. Nothing's asked here but, when a page
/// shows a code to bring back, that code.
struct SignInView: View {
    @ObservedObject var model: MenuModel
    @State private var now: SignIn?
    @State private var error: String?
    @State private var text = ""
    @State private var opened = false
    @State private var answering = false

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(title).font(.headline)
            if let error {
                ErrorText(error: error)
            } else if let now {
                content(now)
            } else {
                Working(text: "Starting…")
            }
        }
        .padding(20)
        .frame(width: 420, alignment: .leading)
        .task(id: model.signInRequest) { await run() }
        .onDisappear { cancelIfRunning() }
    }

    private var title: String {
        guard let request = model.signInRequest else { return "Sign In" }
        let place = request.host.map { " on \($0)" } ?? ""
        switch request.what {
        case .github: return "Sign in to GitHub\(place)"
        case .claude: return "Sign in to Claude\(place)"
        case .toolboxGithub: return "Sign in Linux agents to GitHub"
        case .toolboxClaude: return "Sign in Linux agents to Claude"
        }
    }

    @ViewBuilder private func content(_ now: SignIn) -> some View {
        switch now.state {
        case .done:
            Label("Signed in.", systemImage: "checkmark.circle.fill").foregroundStyle(Palette.signal)
        case .cancelled:
            Text("That was given up on.").foregroundStyle(.secondary)
        case .failed:
            ErrorText(error: now.error ?? "That didn't work.")
            Button("Try Again") { Task { await run() } }
        case .running:
            running(now)
        }
    }

    @ViewBuilder private func running(_ now: SignIn) -> some View {
        if let code = now.code {
            Text("Enter this code on the page.")
            HStack(spacing: 10) {
                Text(code)
                    .font(.system(.title2, design: .monospaced).weight(.semibold))
                    .textSelection(.enabled)
                Button("Copy") { WorktreeActions.copy(code) }
            }
        } else if now.url != nil {
            Text(now.opensItself ? "Finish signing in in your browser." : "Finish signing in on the page.")
                .fixedSize(horizontal: false, vertical: true)
        } else {
            Working(text: "Starting…")
        }
        if let url = now.url {
            Button("Open \(URL(string: url)?.host ?? "the Page") Again") { open(url) }
                .help(url)
        }
        if now.asksForCode {
            VStack(alignment: .leading, spacing: 6) {
                Text("If the page shows a code, paste it here.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                HStack {
                    TextField("Code", text: $text)
                        .textFieldStyle(.roundedBorder)
                        .autocorrectionDisabled()
                        .onSubmit { Task { await answer(now) } }
                    Button("Continue") { Task { await answer(now) } }
                        .disabled(text.isEmpty || answering)
                }
            }
        } else if now.url != nil {
            Working(text: "Waiting for you to finish there…")
        }
    }

    private func open(_ link: String) {
        guard let url = URL(string: link), url.scheme == "https" else { return }
        NSWorkspace.shared.open(url)
    }

    /// Starts it, and follows it to the end.
    private func run() async {
        guard let request = model.signInRequest else { return }
        now = nil
        error = nil
        text = ""
        opened = false
        do {
            var current = try await model.call(
                SignInStart.self, SignInStartParams(host: request.host, what: request.what), timeout: 30)
            show(current)
            while current.state == .running {
                try Task.checkCancellation()
                current = try await model.call(
                    SignInGet.self, SignInGetParams(host: request.host, id: current.id, seen: current.seq), timeout: 40)
                show(current)
            }
            if current.state == .done {
                await model.refresh()
                model.settingsRevision += 1
            }
        } catch is CancellationError {
            return
        } catch {
            self.error = describe(error)
        }
    }

    private func show(_ current: SignIn) {
        now = current
        if let url = current.url, !opened {
            opened = true
            if !current.opensItself { open(url) }
        }
    }

    private func answer(_ current: SignIn) async {
        guard let request = model.signInRequest, !text.isEmpty else { return }
        answering = true
        defer { answering = false }
        do {
            let value = text.trimmingCharacters(in: .whitespacesAndNewlines)
            show(
                try await model.call(
                    SignInAnswer.self, SignInAnswerParams(host: request.host, id: current.id, value: value), timeout: 30))
            text = ""
        } catch {
            self.error = describe(error)
        }
    }

    /// Closing the window gives up on one under way.
    private func cancelIfRunning() {
        guard let request = model.signInRequest, let current = now, current.state == .running else { return }
        let params = SignInParams(host: request.host, id: current.id)
        Task { _ = try? await model.call(SignInCancel.self, params, timeout: 10) }
    }
}
