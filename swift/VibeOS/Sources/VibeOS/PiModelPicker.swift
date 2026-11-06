import SwiftUI
import VibeMenu
import VibeRPC

/// The Pi a profile's agents run: on a Mac, a Linux environment's is in its VMs. A
/// Linux host's is its own, whichever it's called.
func piTarget(host: String, environment: AgentEnvironment) -> PiTarget {
    host == VibeRPC.localHost && environment == .linux ? .vm : .native
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
    }

    // MARK: Under the button

    /// What's worth saying of what's chosen, in words.
    @ViewBuilder private var note: some View {
        if loading && list == nil {
            Working(text: "Asking Pi for its models…")
        } else if let error = listError ?? list?.error {
            noteText("Pi's list isn't here (\(sentence(error))). You can still type a model.")
        } else if case .several(let choices) = matchPiModel(value, in: providers) {
            VStack(alignment: .trailing, spacing: 2) {
                noteText("More than one provider has this, and Pi won't guess. Choose one:")
                ForEach(choices, id: \.self) { choice in
                    Button(choice) { choose(choice) }
                        .buttonStyle(.link)
                        .font(.callout)
                }
            }
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
                    .help("It may sign in another way that VibeOS can't see from here.")
            } else {
                Text("Not signed in").font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
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
