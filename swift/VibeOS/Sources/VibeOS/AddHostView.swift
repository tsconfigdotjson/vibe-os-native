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

    private var trimmedName: String { name.trimmingCharacters(in: .whitespaces) }
    private var trimmedAddress: String { address.trimmingCharacters(in: .whitespaces) }

    private var canAdd: Bool {
        !installing && !trimmedName.isEmpty && !trimmedAddress.isEmpty
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            if let installed {
                AddedHost(installed: installed)
                HStack {
                    Spacer()
                    Button("Done") { close() }
                        .keyboardShortcut(.defaultAction)
                }
                .padding([.horizontal, .bottom])
            } else {
                form
            }
        }
        .frame(width: 460)
    }

    private var form: some View {
        VStack(alignment: .leading, spacing: 12) {
            Form {
                Section {
                    TextField("SSH destination", text: $address, prompt: Text("me@vps.example.com"))
                        .autocorrectionDisabled()
                        .onChange(of: address) { suggestName() }
                    TextField("Name", text: $name, prompt: Text("vps"))
                        .autocorrectionDisabled()
                } footer: {
                    Text("VibeOS signs in with your SSH keys, installs vibed there as a service of your own, "
                        + "and links it to this Mac. Its agents then show here, named like \(shownName):quiet-amber-otter.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .formStyle(.grouped)
            .disabled(installing)
            if let error {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .symbolRenderingMode(.multicolor)
                    .fixedSize(horizontal: false, vertical: true)
                    .textSelection(.enabled)
                    .padding(.horizontal)
            }
            HStack {
                if installing {
                    ProgressView().controlSize(.small)
                    Text("Installing vibed on \(trimmedAddress)…")
                        .font(.callout).foregroundStyle(.secondary)
                }
                Spacer()
                Button("Cancel") { close() }
                    .keyboardShortcut(.cancelAction)
                Button("Add Host") { Task { await add() } }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!canAdd)
            }
            .padding([.horizontal, .bottom])
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

    @State private var lastSuggestion = ""

    private func add() async {
        installing = true
        defer { installing = false }
        do {
            installed = try await model.addHost(name: trimmedName, address: trimmedAddress)
            error = nil
        } catch {
            self.error = describe(error)
        }
    }

    private func close() {
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

    var body: some View {
        let host = installed.host
        VStack(alignment: .leading, spacing: 10) {
            Label {
                Text("\(host.name) is added").font(.headline)
            } icon: {
                Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
            }
            Text("\(host.label) runs vibed \(host.version ?? "")"
                + (installed.changed ? ", just installed." : ", as it already did."))
                .foregroundStyle(.secondary)
            if !host.missing.isEmpty {
                Label {
                    VStack(alignment: .leading, spacing: 4) {
                        Text("Still needed on \(host.name): \(host.missing.joined(separator: ", "))")
                        Text("Install them on the server, then log in there: gh auth login, and claude with /login.")
                            .font(.callout).foregroundStyle(.secondary)
                    }
                } icon: {
                    Image(systemName: "wrench.and.screwdriver").foregroundStyle(.orange)
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
        .padding()
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
    }
}
