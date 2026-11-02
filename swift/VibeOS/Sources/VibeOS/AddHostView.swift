import SwiftUI
import VibeMenu
import VibeRPC

/// Add Linux Host…: a name and an SSH destination. vibed installs itself on
/// the server over SSH and links it to this Mac (PRD: Adding a Linux host).
struct AddHostView: View {
    @ObservedObject var model: MenuModel
    @Environment(\.dismissWindow) private var dismissWindow
    @State private var name = ""
    @State private var address = ""
    @State private var installing = false
    @State private var error: String?
    @State private var installed: HostInstall?
    @State private var lastSuggestion = ""
    /// Bumped each time the window closes: an install still going when it
    /// closed carries on in vibed, and its host shows in the menu, but its
    /// answer doesn't land on the next opening.
    @State private var opening = 0

    init(model: MenuModel, preview: HostInstall? = nil) {
        self.model = model
        _installed = State(initialValue: preview)
    }

    private var trimmedName: String { name.trimmingCharacters(in: .whitespaces) }
    private var trimmedAddress: String { address.trimmingCharacters(in: .whitespaces) }

    private var canAdd: Bool {
        !installing && !trimmedName.isEmpty && !trimmedAddress.isEmpty
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            if let installed {
                AddedHost(installed: installed)
                HStack {
                    Spacer()
                    Button("Done") { close() }
                        .keyboardShortcut(.defaultAction)
                }
            } else {
                form
            }
        }
        .padding(20)
        .frame(width: 480)
        // Escape closes it in either state; Return is Add Host, or Done.
        .onExitCommand { close() }
    }

    private var form: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(alignment: .top, spacing: 12) {
                IconTile(symbol: "server.rack", color: .indigo, size: 36)
                VStack(alignment: .leading, spacing: 3) {
                    Text("Add a Linux Host").font(.headline)
                    Text("VibeOS signs in with your SSH keys, installs vibed there as a service of your own, and "
                        + "links it to this Mac. Its agents then show in this menu as \(shownName):quiet-amber-otter.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 10) {
                GridRow {
                    Text("SSH destination").foregroundStyle(.secondary).gridColumnAlignment(.trailing)
                    TextField("SSH destination", text: $address, prompt: Text("me@vps.example.com"))
                        .labelsHidden()
                        .autocorrectionDisabled()
                        .onChange(of: address) { suggestName() }
                }
                GridRow {
                    Text("Name").foregroundStyle(.secondary).gridColumnAlignment(.trailing)
                    TextField("Name", text: $name, prompt: Text("vps"))
                        .labelsHidden()
                        .autocorrectionDisabled()
                        .onSubmit { if canAdd { Task { await add() } } }
                }
            }
            .disabled(installing)
            ErrorText(error: error)
            HStack {
                if installing {
                    Working(text: "Installing vibed on \(trimmedAddress)…")
                }
                Spacer()
                Button("Cancel") { close() }
                    .keyboardShortcut(.cancelAction)
                Button("Add Host") { Task { await add() } }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!canAdd)
            }
        }
    }

    private var shownName: String { trimmedName.isEmpty ? "vps" : trimmedName }

    /// A name from the destination's host, until one is typed: `vps` for
    /// `me@vps.example.com`.
    private func suggestName() {
        let host = trimmedAddress.split(separator: "@").last.map(String.init) ?? ""
        let first = host.split(separator: ".").first.map(String.init) ?? ""
        // What a host name may hold: ASCII letters, digits, - and _.
        let suggested = String(
            first.filter { $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "-" || $0 == "_") }.prefix(32))
        if name.isEmpty || name == lastSuggestion {
            name = suggested
            lastSuggestion = suggested
        }
    }

    private func add() async {
        guard canAdd else { return }
        let mine = opening
        installing = true
        do {
            let done = try await model.addHost(name: trimmedName, address: trimmedAddress)
            guard mine == opening else { return }
            installed = done
            error = nil
        } catch {
            guard mine == opening else { return }
            self.error = describe(error)
        }
        installing = false
    }

    private func close() {
        opening += 1
        installing = false
        dismissWindow(id: WindowID.addHost)
        name = ""
        address = ""
        lastSuggestion = ""
        error = nil
        installed = nil
    }
}

/// What an install did, and what the server still needs.
struct AddedHost: View {
    let installed: HostInstall
    @State private var appeared = false

    var body: some View {
        let host = installed.host
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: "checkmark.circle.fill")
                    .font(.system(size: 30))
                    .foregroundStyle(Palette.signal)
                    .symbolEffect(.bounce, value: appeared)
                VStack(alignment: .leading, spacing: 3) {
                    Text("\(host.name) is added").font(.headline)
                    Text("\(host.label) runs vibed \(host.version ?? "")"
                        + (installed.changed ? ", just installed." : ", as it already did."))
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
            }
            if !host.missing.isEmpty {
                Label {
                    VStack(alignment: .leading, spacing: 3) {
                        Text("Still needed on \(host.name): \(host.missing.joined(separator: ", "))")
                        Text("Install them on the server, then log in there: gh auth login, and claude with /login.")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                    }
                } icon: {
                    Image(systemName: "wrench.and.screwdriver").foregroundStyle(Palette.attention)
                }
                .fixedSize(horizontal: false, vertical: true)
            }
            ForEach(installed.notes, id: \.self) { note in
                Label(note, systemImage: "info.circle")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
        .onAppear { appeared = true }
    }
}
