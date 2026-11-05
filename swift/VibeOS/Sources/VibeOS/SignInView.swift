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
    @State private var answerError: String?
    @State private var text = ""
    @State private var opened = false
    @State private var answering = false
    /// Goes up with Try Again: a run belongs to the window, and goes when it does.
    @State private var tries = 0

    private struct Run: Hashable {
        let request: SignInRequest?
        let tries: Int
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(title).font(.headline)
            if let error {
                ErrorText(error: error)
                tryAgain
            } else if let now {
                content(now)
            } else {
                Working(text: "Starting…")
            }
        }
        .padding(20)
        .frame(width: 420, alignment: .leading)
        .task(id: Run(request: model.signInRequest, tries: tries)) { await run() }
        .onDisappear { cancelIfRunning() }
    }

    private var tryAgain: some View {
        Button("Try Again") { tries += 1 }
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
            tryAgain
        case .running:
            if now.finishing {
                Working(text: "Signed in. Giving Linux agents the login…")
            } else {
                running(now)
            }
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
            let place = URL(string: url)?.host ?? "the Page"
            if !now.what.isOwnPage(url) {
                Text("This sign-in goes to \(place), which isn't the provider's own site. Open it only if you expected that.")
                    .font(.callout)
                    .foregroundStyle(Palette.attention)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Button(now.what.isOwnPage(url) ? "Open \(place) Again" : "Open \(place)") { open(url) }
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
                if let why = answerError ?? now.error {
                    ErrorText(error: why)
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

    /// Starts it, and follows it to the end. A window closed, or a new request, ends this
    /// and gives up on the flow, even one that's only now answering that it's started.
    private func run() async {
        guard let request = model.signInRequest else { return }
        now = nil
        error = nil
        answerError = nil
        text = ""
        opened = false
        // Not this task's, so going away doesn't lose the answer, and the flow with it.
        let starting = Task { @MainActor in
            try await model.call(
                SignInStart.self, SignInStartParams(host: request.host, what: request.what), timeout: 30)
        }
        var current: SignIn
        do {
            current = try await starting.value
        } catch {
            if !Task.isCancelled { self.error = describe(error) }
            return
        }
        guard !Task.isCancelled else {
            giveUp(on: current, host: request.host)
            return
        }
        show(current)
        // A blip's not a failure: the flow's still going, and it's asked again.
        var missed = 0
        while current.state == .running {
            do {
                let next = try await model.call(
                    SignInGet.self, SignInGetParams(host: request.host, id: current.id, seen: current.seq), timeout: 40)
                guard !Task.isCancelled else {
                    giveUp(on: next, host: request.host)
                    return
                }
                missed = 0
                current = next
                show(current)
            } catch {
                guard !Task.isCancelled else {
                    giveUp(on: current, host: request.host)
                    return
                }
                missed += 1
                if missed >= 5 {
                    self.error = describe(error)
                    return
                }
                try? await Task.sleep(for: .seconds(2))
            }
        }
        if current.state == .done {
            await model.refresh()
            model.setupMayHaveChanged()
        }
    }

    private func show(_ current: SignIn) {
        now = current
        if let url = current.url, !opened, !current.opensItself, current.what.isOwnPage(url) {
            opened = true
            open(url)
        }
    }

    private func answer(_ current: SignIn) async {
        guard let request = model.signInRequest, !text.isEmpty else { return }
        answering = true
        answerError = nil
        defer { answering = false }
        do {
            let value = text.trimmingCharacters(in: .whitespacesAndNewlines)
            let next = try await model.call(
                SignInAnswer.self, SignInAnswerParams(host: request.host, id: current.id, value: value), timeout: 30)
            if next.seq >= (now?.seq ?? 0) { show(next) }
            text = ""
        } catch {
            // What's typed stays, for another go: the flow's still waiting for it.
            answerError = describe(error)
        }
    }

    /// Closing the window gives up on one under way, but for one that's been approved.
    private func cancelIfRunning() {
        guard let request = model.signInRequest, let current = now else { return }
        giveUp(on: current, host: request.host)
    }

    private func giveUp(on current: SignIn, host: String?) {
        guard current.state == .running, !current.finishing else { return }
        let params = SignInParams(host: host, id: current.id)
        // Not this task's, which is likely what's just been cancelled.
        Task { _ = try? await model.call(SignInCancel.self, params, timeout: 10) }
    }
}
