import SwiftUI
import VibeMenu
import VibeRPC

/// Settings → Agents (PRD: Who gets which credentials; What's inside
/// `linux`): the coding agents besides Claude, on each host. For Pi, the
/// `pi` a host's native agents run, the layer `linux` agents boot on this Mac,
/// and who's signed in. Signing in is Pi's own, guided here with no terminal:
/// VibeOS picks no provider, supplies no key, and shows only providers' names.
struct AgentsPane: View {
    @ObservedObject var model: MenuModel
    @State private var host = VibeRPC.localHost
    @State private var status: PiStatus?
    @State private var installing: PiTarget?
    @State private var error: String?
    @State private var signIn: PiSignInRequest?

    private var local: Bool { host == VibeRPC.localHost }

    var body: some View {
        Form {
            if model.hosts.count > 1 {
                HostPicker(model: model, host: $host)
            }
            if let status {
                nativeSection(status)
                if let vms = status.vms { vmSection(vms) }
                signInNote
            } else if error == nil {
                ProgressView().frame(maxWidth: .infinity)
            }
            ErrorText(error: error)
        }
        .formStyle(.grouped)
        .navigationTitle("Agents")
        .task(id: "\(host)#\(model.settingsRevision)") { await load() }
        .sheet(item: $signIn) { request in
            PiSignInSheet(model: model, request: request) { Task { await load() } }
        }
    }

    /// The `pi` that runs natively: on the Mac for `macos` agents, on a server
    /// for all of them.
    private func nativeSection(_ status: PiStatus) -> some View {
        Section {
            LabeledContent("Pi") {
                if let native = status.native {
                    Text(native.version)
                        .textSelection(.enabled)
                        .help(native.path)
                } else {
                    Text("Not installed").foregroundStyle(.secondary)
                }
            }
            if let why = status.error {
                Label(why, systemImage: "exclamationmark.triangle")
                    .font(.caption)
                    .foregroundStyle(Palette.attention)
                    .textSelection(.enabled)
            }
            loginsRow(status.logins ?? [])
            HStack {
                if installing == .native {
                    Working(text: "Downloading Pi…")
                }
                Spacer()
                Button("Sign In…") { signIn = PiSignInRequest(host: host, target: .native) }
                    .help("Sign in to a provider, for these agents, with an account or an API key. No terminal.")
                // A `pi` of the owner's own is theirs to update.
                if status.native == nil || status.native?.managed == true {
                    Button(status.error != nil ? "Try Again" : status.native == nil ? "Install Pi" : "Update Pi") {
                        Task { await install(.native) }
                    }
                    .disabled(installing != nil)
                    .help("Downloads the version of Pi VibeOS is tested with, checks it against its release's checksums, and links it at ~/.local/bin/pi. No Node, no Homebrew.")
                }
            }
        } header: {
            Text(local ? "macOS agents" : "Agents on \(host)")
        } footer: {
            Text(
                local
                    ? "macOS agents use your own Pi and your own agent directory, as they do your own Claude."
                    : "Agents on this server use its own Pi and its own ~/.pi/agent."
            )
            .foregroundStyle(.secondary)
        }
    }

    /// What the VMs boot, and the one login they all share.
    private func vmSection(_ vms: PiVms) -> some View {
        Section {
            LabeledContent("Pi") {
                if let version = vms.version {
                    Text(version)
                } else {
                    Text("Not downloaded").foregroundStyle(.secondary)
                }
            }
            loginsRow(vms.logins ?? [])
            if let error = vms.error, vms.version == nil {
                Text(error).font(.callout).foregroundStyle(.secondary)
            }
            ForEach(vms.alerts ?? [], id: \.self) { alert in
                Label(cleanAlert(alert), systemImage: "exclamationmark.triangle.fill")
                    .font(.callout)
                    .foregroundStyle(.orange)
                    .textSelection(.enabled)
            }
            if let offered = vms.offered, !offered.isEmpty {
                ForEach(offered, id: \.provider) { login in
                    LabeledContent {
                        Button("Forget") { Task { await forget([login.provider]) } }
                            .help("Takes this login from every Linux Pi agent. If that agent offers another, stop it too.")
                    } label: {
                        Text(cleanAlert(login.provider))
                        Text("From \(cleanAlert(login.agent)), not you")
                    }
                }
                if offered.count > 1 {
                    Button("Forget All from Agents") {
                        Task { await forget(offered.map(\.provider)) }
                    }
                }
            }
            HStack {
                if installing == .vm {
                    Working(text: "Downloading Pi…")
                }
                Spacer()
                // For signing in, or in again over what an agent's left: an alert
                // above may be sending the owner here.
                Button("Sign In…") { signIn = PiSignInRequest(host: host, target: .vm) }
                    .help("Sign in to a provider, for every Linux agent, with an account or an API key. No terminal.")
                // The version is this VibeOS's own: a new one comes with an update.
                if vms.version == nil {
                    Button("Download Pi") {
                        Task { await install(.vm) }
                    }
                    .disabled(installing != nil)
                    .help("Downloads the version of Pi VibeOS is tested with, checksummed, as a layer for Linux Pi agents.")
                }
            }
        } header: {
            Text("Linux agents")
        } footer: {
            Text(
                "Sign in once, here. Within seconds every Linux Pi agent has that login, each VM holding its own "
                    + "copy. Settings and extensions for them live in ~/VibeOS/linux-pi on this Mac."
            )
            .foregroundStyle(.secondary)
        }
    }

    private func loginsRow(_ logins: [String]) -> some View {
        LabeledContent("Signed in to") {
            if logins.isEmpty {
                Text("No one yet").foregroundStyle(.secondary)
            } else {
                Text(logins.joined(separator: ", ")).textSelection(.enabled)
            }
        }
    }

    private var signInNote: some View {
        Section {
            Text(
                "Pi's login is your own: a subscription or an API key, with whichever provider you choose. Pi "
                    + "itself signs in, and its choices are the ones it offers in /login. Your Claude sign-in never reaches Pi, nor does a key in VibeOS's environment. A "
                    + "key you do want Pi to use goes in ~/.config/vibeos/pi.env. A login to the same provider "
                    + "takes the place of that key."
            )
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
        }
    }

    private func load() async {
        do {
            status = try await model.call(PiStatusGet.self, HostParams(host: host))
            error = nil
        } catch {
            status = nil
            self.error = describe(error)
        }
    }

    private func forget(_ providers: [String]) async {
        do {
            status = try await model.call(PiForget.self, PiForgetParams(host: host, providers: providers))
            error = nil
        } catch {
            self.error = describe(error)
        }
    }

    private func install(_ target: PiTarget) async {
        installing = target
        defer { installing = nil }
        do {
            status = try await model.call(
                PiInstall.self, PiInstallParams(host: host, target: target), timeout: 60 * 60)
            error = nil
        } catch {
            self.error = describe(error)
        }
    }
}
