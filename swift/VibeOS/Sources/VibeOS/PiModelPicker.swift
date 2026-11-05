import AppKit
import SwiftUI
import VibeMenu
import VibeRPC

/// A sign-in the app has been asked for: which Pi it's for, and who to, if that's known.
struct PiSignInRequest: Identifiable, Hashable {
    let host: String
    let target: PiTarget
    var provider: String?

    var id: String { "\(host)#\(target.rawValue)#\(provider ?? "")" }
}

/// The Pi a profile's agents run: on a Mac, a Linux environment's is in its VMs. A
/// Linux host's is its own, whichever it's called.
func piTarget(host: String, environment: AgentEnvironment) -> PiTarget {
    host == VibeRPC.localHost && environment == .linux ? .vm : .native
}

/// Whose agents a sign-in is for, in words.
func piSignInPlace(_ host: String, _ target: PiTarget) -> String {
    if host != VibeRPC.localHost { return "agents on \(host)" }
    return target == .vm ? "Linux agents on this Mac" : "macOS agents on this Mac"
}

/// A Pi profile's model (PRD: Profiles): Pi's own list for that host and environment,
/// searchable, with Pi's default first and who's signed in said. Anything else that's
/// safe to pass on can be typed, as Pi may know what it doesn't list.
struct PiModelPicker: View {
    @ObservedObject var model: MenuModel
    let host: String
    let target: PiTarget
    /// What's saved, empty for Pi's default.
    let value: String
    /// Saves a model, empty for Pi's default; true if it took.
    let commit: (String) async -> Bool

    @State private var list: PiModels?
    @State private var listError: String?
    @State private var loading = false
    @State private var open = false
    @State private var query = ""
    @State private var problem: String?
    @State private var signIn: PiSignInRequest?

    private var providers: [PiProvider] { list?.providers ?? [] }

    var body: some View {
        VStack(alignment: .trailing, spacing: 4) {
            Button {
                query = ""
                problem = nil
                open = true
            } label: {
                HStack(spacing: 6) {
                    Text(value.isEmpty ? "Pi's default" : value)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Spacer(minLength: 0)
                    Image(systemName: "chevron.up.chevron.down")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
                .frame(width: 240)
            }
            .help("Pi's own list of models for these agents. Type to search, or to use one Pi doesn't list.")
            .accessibilityLabel("Model")
            .accessibilityValue(value.isEmpty ? "Pi's default" : value)
            .popover(isPresented: $open, arrowEdge: .bottom) { chooser }
            note
        }
        .task(id: "\(host)#\(target.rawValue)#\(model.settingsRevision)") { await load() }
        .sheet(item: $signIn) { request in
            PiSignInSheet(model: model, request: request) { Task { await load() } }
        }
    }

    // MARK: Under the button

    /// What's worth saying of what's chosen, in words.
    @ViewBuilder private var note: some View {
        if loading && list == nil {
            Working(text: "Asking Pi for its models…")
        } else if let error = listError ?? list?.error {
            noteText("Pi's list isn't here (\(sentence(error))). You can still type a model.")
        } else if !value.isEmpty, let who = piProviderToSignIn(for: value, in: providers) {
            signInNote("Pi isn't signed in to \(who.name) for these agents.", provider: who.id)
        } else if case .several(let choices) = matchPiModel(value, in: providers) {
            VStack(alignment: .trailing, spacing: 2) {
                noteText("More than one provider has this, and Pi won't guess. Choose one:")
                ForEach(choices, id: \.self) { choice in
                    Button(choice) { choose(choice) }
                        .buttonStyle(.link)
                        .font(.callout)
                }
            }
        } else if value.isEmpty && piHasNoLogin(providers) {
            signInNote("Pi isn't signed in to anyone for these agents yet.", provider: nil)
        } else if !value.isEmpty && !providers.isEmpty && matchPiModel(value, in: providers) == .unlisted {
            noteText("Pi doesn't list this. It'll try it, and say if it can't.")
        }
    }

    private func noteText(_ text: String) -> some View {
        Text(text)
            .font(.callout)
            .foregroundStyle(.secondary)
            .multilineTextAlignment(.trailing)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: 320, alignment: .trailing)
    }

    private func signInNote(_ text: String, provider: String?) -> some View {
        HStack(spacing: 8) {
            noteText(text)
            Button("Sign In…") { signIn = PiSignInRequest(host: host, target: target, provider: provider) }
                .controlSize(.small)
        }
    }

    // MARK: The list

    private var chooser: some View {
        VStack(alignment: .leading, spacing: 8) {
            TextField("Search Pi's models, or type one", text: $query)
                .textFieldStyle(.roundedBorder)
                .autocorrectionDisabled()
                .onSubmit { submit() }
                .onChange(of: query) { problem = nil }
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 2) {
                    if query.isEmpty { defaultRow }
                    ForEach(piModels(matching: query, in: providers), id: \.id) { provider in
                        providerHeader(provider)
                        ForEach(provider.listedModels, id: \.id) { row($0, of: provider) }
                    }
                }
                .padding(.trailing, 8)
            }
            .frame(height: 300)
            if let problem {
                Text(problem)
                    .font(.callout)
                    .foregroundStyle(Palette.trouble)
                    .fixedSize(horizontal: false, vertical: true)
            } else if case .several = matchPiModel(query, in: providers) {
                Text("More than one provider has this, and Pi won't guess. Choose one.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            } else if !trimmedQuery.isEmpty, matchPiModel(query, in: providers) == .unlisted {
                Button("Use “\(trimmedQuery)”") { submit() }
                    .help("Pi doesn't list this. It'll try it, and say if it can't.")
            }
            Divider()
            Button("Sign In to a Provider…") {
                open = false
                signIn = PiSignInRequest(host: host, target: target)
            }
            .buttonStyle(.link)
        }
        .padding(12)
        .frame(width: 400)
    }

    private var trimmedQuery: String { query.trimmingCharacters(in: .whitespaces) }

    private var defaultRow: some View {
        Button { choose("") } label: {
            HStack {
                Text("Pi's default")
                Spacer()
                if value.isEmpty { Image(systemName: "checkmark").foregroundStyle(Palette.signal) }
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .padding(.vertical, 3)
        .help("What Pi's own settings and logins give")
    }

    private func providerHeader(_ provider: PiProvider) -> some View {
        HStack(spacing: 6) {
            Text(provider.name).font(.subheadline.weight(.semibold))
            if provider.signedIn {
                Text("Signed in").font(.caption).foregroundStyle(Palette.signal)
            } else if provider.ambient {
                Text("From what's around it").font(.caption).foregroundStyle(.secondary)
                    .help("It can sign in from a cloud profile or role, which can't be told from here.")
            } else {
                Text("Not signed in").font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
            if !provider.signedIn && (provider.oauth || provider.apiKey) {
                Button("Sign In…") {
                    open = false
                    signIn = PiSignInRequest(host: host, target: target, provider: provider.id)
                }
                .buttonStyle(.link)
                .font(.caption)
            }
        }
        .padding(.top, 8)
    }

    private func row(_ model: PiModel, of provider: PiProvider) -> some View {
        let full = "\(provider.id)/\(model.id)"
        return Button { choose(full) } label: {
            HStack(spacing: 8) {
                Text(model.id).lineLimit(1).truncationMode(.middle)
                if let name = model.name {
                    Text(name).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                }
                Spacer(minLength: 0)
                if value == full { Image(systemName: "checkmark").foregroundStyle(Palette.signal) }
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .padding(.vertical, 2)
        .accessibilityLabel(full)
    }

    // MARK: Choosing

    /// What was typed: one Pi lists goes in full, one it doesn't goes as it is if it's
    /// safe to pass on, and several wait to be chosen between in the list.
    private func submit() {
        let typed = trimmedQuery
        guard !typed.isEmpty else { return }
        switch matchPiModel(typed, in: providers) {
        case .listed(let full): choose(full)
        case .several: break
        case .unlisted:
            if let why = piModelProblem(typed) {
                problem = why
            } else {
                choose(typed)
            }
        }
    }

    private func choose(_ chosen: String) {
        open = false
        guard chosen != value else { return }
        Task { _ = await commit(chosen) }
    }

    private func load() async {
        loading = true
        defer { loading = false }
        do {
            // The first ask may have to get Pi first.
            let now = try await model.call(PiModelsGet.self, PiModelsParams(host: host, target: target), timeout: 60 * 60)
            guard !Task.isCancelled else { return }
            list = now
            listError = nil
        } catch {
            guard !Task.isCancelled else { return }
            listError = describe(error)
        }
    }
}

/// Pi's own sign-in, in the app (PRD: Who gets which credentials): who to sign in to,
/// how, and then whatever Pi asks. Pi does the signing in, and vibed carries its
/// questions here. A key typed here goes to Pi and isn't kept.
struct PiSignInSheet: View {
    @ObservedObject var model: MenuModel
    let request: PiSignInRequest
    /// Called once it's worked.
    let finished: () -> Void

    @Environment(\.dismiss) private var dismiss
    @State private var providers: [PiProvider] = []
    @State private var provider: PiProvider?
    @State private var login: PiLogin?
    @State private var working: String?
    @State private var error: String?
    @State private var text = ""
    @State private var opened: Set<String> = []
    @State private var pasting = false

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 2) {
                Text(provider.map { "Sign In to \($0.name)" } ?? "Sign In to a Provider").font(.headline)
                Text("For \(piSignInPlace(request.host, request.target)). Pi keeps the login, and they all share it.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            content
            ErrorText(error: error)
            HStack {
                if let working { Working(text: working) }
                Spacer()
                if login?.state == .done {
                    Button("Done") { dismiss() }.keyboardShortcut(.defaultAction)
                } else {
                    Button("Cancel", role: .cancel) { dismiss() }.keyboardShortcut(.cancelAction)
                }
            }
        }
        .padding(20)
        .frame(width: 480)
        .task { await loadProviders() }
        .task(id: login?.id) { await follow() }
        // A sheet closed part way gives the sign-in up, so Pi isn't left waiting.
        .onDisappear { giveUp() }
    }

    @ViewBuilder private var content: some View {
        if let login {
            loginView(login)
        } else if let provider {
            methods(provider)
        } else if !providers.isEmpty {
            providerList
        } else if working == nil && error == nil {
            ProgressView().frame(maxWidth: .infinity)
        }
    }

    // MARK: Who and how

    private var providerList: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 4) {
                ForEach(providers.filter { $0.oauth || $0.apiKey }, id: \.id) { candidate in
                    Button { chose(candidate) } label: {
                        HStack {
                            Text(candidate.name)
                            Spacer()
                            if candidate.signedIn {
                                Text("Signed in").font(.caption).foregroundStyle(Palette.signal)
                            }
                        }
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .padding(.vertical, 3)
                }
            }
        }
        .frame(height: 300)
    }

    private func methods(_ provider: PiProvider) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            if provider.oauth {
                Button(provider.oauthLabel ?? "Sign In with an Account") {
                    Task { await start(provider, .oauth) }
                }
            }
            if provider.apiKey {
                Button("Use an API Key") { Task { await start(provider, .apiKey) } }
            }
        }
        .disabled(working != nil)
    }

    // MARK: The sign-in

    @ViewBuilder private func loginView(_ login: PiLogin) -> some View {
        switch login.state {
        case .done:
            Label("Signed in. Pi's models for \(provider?.name ?? login.provider) are in the list now.", systemImage: "checkmark.circle.fill")
                .foregroundStyle(Palette.signal)
        case .cancelled:
            Text("That was given up on.").foregroundStyle(.secondary)
            tryAgain
        case .failed:
            ErrorText(error: login.error ?? "That didn't work.")
            tryAgain
        case .running:
            running(login)
        }
    }

    private var tryAgain: some View {
        Button("Try Again") {
            self.login = nil
            error = nil
            if let provider, !provider.oauth || !provider.apiKey {
                Task { await start(provider, provider.oauth ? .oauth : .apiKey) }
            }
        }
    }

    @ViewBuilder private func running(_ login: PiLogin) -> some View {
        if let code = login.code { codeView(code, login) } else if let url = login.url { pageView(url, login) }
        if let prompt = login.prompt {
            if prompt.kind == .manualCode {
                pasteView(prompt, login)
            } else {
                question(prompt, login)
            }
        } else if login.code == nil && login.url == nil {
            Working(text: login.progress ?? "Pi's on it…")
        } else {
            Working(text: "Waiting for you to finish there…")
        }
    }

    private func pageView(_ url: String, _ login: PiLogin) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Your browser has opened a page to sign in. Finish there, and VibeOS takes it from here.")
                .fixedSize(horizontal: false, vertical: true)
            Button("Open It Again") { openPage(url) }
        }
    }

    private func codeView(_ code: String, _ login: PiLogin) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Enter this code on the page your browser opened.")
            HStack(spacing: 10) {
                Text(code)
                    .font(.system(.title2, design: .monospaced).weight(.semibold))
                    .textSelection(.enabled)
                Button("Copy") { WorktreeActions.copy(code) }
            }
            if let url = login.url { Button("Open the Page Again") { openPage(url) } }
        }
    }

    /// Where a browser ended, for when nothing could bring that back by itself.
    private func pasteView(_ prompt: PiLoginPrompt, _ login: PiLogin) -> some View {
        DisclosureGroup("Did the page end somewhere that won't load, or show a code?", isExpanded: $pasting) {
            VStack(alignment: .leading, spacing: 6) {
                Text("Paste its address, or the code, here.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                HStack {
                    TextField(prompt.placeholder ?? "", text: $text)
                        .textFieldStyle(.roundedBorder)
                        .autocorrectionDisabled()
                        .onSubmit { submit(prompt, login) }
                    Button("Continue") { submit(prompt, login) }
                        .disabled(text.isEmpty || working != nil)
                }
            }
            .padding(.top, 4)
        }
        .font(.callout)
    }

    @ViewBuilder private func question(_ prompt: PiLoginPrompt, _ login: PiLogin) -> some View {
        Text(prompt.message).fixedSize(horizontal: false, vertical: true)
        if prompt.kind == .select {
            VStack(alignment: .leading, spacing: 6) {
                ForEach(prompt.options ?? [], id: \.id) { option in
                    Button(option.label) { send(option.id, prompt, login) }
                }
            }
            .disabled(working != nil)
        } else {
            HStack {
                Group {
                    if prompt.kind == .secret {
                        SecureField(prompt.placeholder ?? "", text: $text)
                    } else {
                        TextField(prompt.placeholder ?? "", text: $text)
                    }
                }
                .textFieldStyle(.roundedBorder)
                .autocorrectionDisabled()
                .onSubmit { submit(prompt, login) }
                // Pi's text questions can be left blank; a key can't.
                Button("Continue") { submit(prompt, login) }
                    .keyboardShortcut(.defaultAction)
                    .disabled((prompt.kind == .secret && text.isEmpty) || working != nil)
            }
        }
    }

    private func submit(_ prompt: PiLoginPrompt, _ login: PiLogin) {
        if prompt.kind != .text && text.isEmpty { return }
        let value = text
        text = ""
        send(value, prompt, login)
    }

    // MARK: Calls

    private func loadProviders() async {
        do {
            let list = try await model.call(
                PiModelsGet.self, PiModelsParams(host: request.host, target: request.target), timeout: 60 * 60)
            providers = list.providers ?? []
            if let why = list.error { error = why }
            if let named = request.provider {
                if let found = providers.first(where: { $0.id == named }) {
                    chose(found)
                } else if !providers.isEmpty {
                    error = "Pi doesn't know \(named). Choose who to sign in to."
                }
            }
        } catch {
            self.error = describe(error)
        }
    }

    private func chose(_ candidate: PiProvider) {
        provider = candidate
        error = nil
        // One way in needs no asking.
        if candidate.oauth != candidate.apiKey {
            Task { await start(candidate, candidate.oauth ? .oauth : .apiKey) }
        }
    }

    private func start(_ provider: PiProvider, _ method: PiLoginMethod) async {
        working = "Starting Pi…"
        error = nil
        opened = []
        pasting = false
        defer { working = nil }
        do {
            let params = PiLoginStartParams(
                host: request.host, target: request.target, provider: provider.id, method: method)
            take(try await model.call(PiLoginStart.self, params, timeout: 60 * 60))
        } catch {
            self.error = describe(error)
        }
    }

    private func send(_ value: String, _ prompt: PiLoginPrompt, _ login: PiLogin) {
        Task {
            working = "Telling Pi…"
            error = nil
            defer { working = nil }
            do {
                let params = PiLoginAnswerParams(host: request.host, id: login.id, prompt: prompt.id, value: value)
                take(try await model.call(PiLoginAnswer.self, params, timeout: 90))
            } catch {
                self.error = describe(error)
            }
        }
    }

    /// Hears how it stands as it changes, for as long as it's going.
    private func follow() async {
        while !Task.isCancelled, let now = login, now.state == .running {
            do {
                let params = PiLoginGetParams(host: request.host, id: now.id, seen: now.seq)
                let next = try await model.call(PiLoginGet.self, params, timeout: 60)
                guard !Task.isCancelled else { return }
                take(next)
            } catch {
                guard !Task.isCancelled else { return }
                self.error = describe(error)
                return
            }
        }
    }

    /// The newest word on it: an answer and a wait can each come back with one.
    private func take(_ next: PiLogin) {
        if let now = login, now.id == next.id, now.seq > next.seq { return }
        let wasDone = login?.id == next.id && login?.state == .done
        login = next
        if let url = next.url, opened.insert(url).inserted { openPage(url) }
        if next.state == .done && !wasDone { finished() }
    }

    /// vibed only gives `https`, and it's held to that here as well, as it can be
    /// another host's word.
    private func openPage(_ address: String) {
        guard let url = URL(string: address), url.scheme?.lowercased() == "https" else { return }
        NSWorkspace.shared.open(url)
    }

    private func giveUp() {
        text = ""
        guard let login, login.state == .running else { return }
        let (client, params) = (model, PiLoginParams(host: request.host, id: login.id))
        Task { _ = try? await client.call(PiLoginCancel.self, params, timeout: 30) }
    }
}
