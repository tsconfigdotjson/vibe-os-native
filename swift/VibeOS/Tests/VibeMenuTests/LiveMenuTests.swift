import Foundation
import Testing

@testable import VibeMenu
import VibeRPC

/// The menu against a real vibed running agents on vibe-env's test backend.
/// scripts/check-full.sh starts that vibed and sets VIBE_TEST_SOCKET and
/// VIBE_TEST_REPO (a git repo to add as a project); the test writes
/// VIBE_TEST_MARKER so the script knows it ran rather than being skipped.
@Suite(
    .enabled(
        if: ProcessInfo.processInfo.environment["VIBE_TEST_SOCKET"] != nil
            && ProcessInfo.processInfo.environment["VIBE_TEST_REPO"] != nil))
@MainActor
struct LiveMenuTests {
    let environment = ProcessInfo.processInfo.environment

    @Test(.timeLimit(.minutes(2))) func theMenuTracksALiveAgent() async throws {
        let client = VibedClient(socketPath: environment["VIBE_TEST_SOCKET"] ?? "")
        let repo = try #require(environment["VIBE_TEST_REPO"])
        let project = try client.call(ProjectsAdd.self, ProjectAddParams(path: repo))
        // 1G agents and nothing kept back, so one fits on any Mac.
        _ = try client.call(
            ProfilesUpdate.self, ProfileUpdateParams(project: project.name, profile: "software-eng-mac", memory: 1 << 30))
        var limits = LimitsSetParams()
        limits.reserveMemory = 0
        _ = try client.call(LimitsSet.self, limits)

        // No poll: only vibed's events may move the menu.
        let terminal = FakeTerminal()
        let model = MenuModel(
            daemon: client, terminal: terminal, vibePath: "/usr/local/bin/vibe", socket: nil,
            openPoll: .seconds(3600), closedPoll: .seconds(3600), endOfTurn: { false })
        model.start()
        defer { model.stop() }
        await eventually("connected", within: 20) { model.live && model.connection == .connected }
        #expect(model.agents.isEmpty)

        // The stand-in for Claude asks a question as Claude does: the row
        // appears with the start's event, and turns `question` with the
        // status engine's.
        let started = try await model.call(
            AgentsStart.self,
            AgentStartParams(project: project.name, profile: "software-eng-mac", prompt: "live question"))
        await eventually("the new row", within: 20) { model.agent(host: "local", ref: started.ref) != nil }
        await eventually("the question", within: 30) {
            model.agent(host: "local", ref: started.ref)?.state == .question
        }
        #expect(model.icon.question)
        #expect(model.icon.awake == 1)

        // Interrupt reaches it, and a click opens one tab for it.
        let interrupted = try await model.call(AgentsInterrupt.self, AgentParams(ref: started.ref))
        #expect(interrupted.ref == started.ref)
        #expect(interrupted.memory == 1 << 30)
        let row = try #require(model.agent(host: "local", ref: started.ref))
        await model.attach(row)
        await model.attach(row)
        #expect(model.message == nil)
        #expect(terminal.ran == ["exec '/usr/local/bin/vibe' attach '\(started.ref)'"])

        // Deleted (it has its setup's changes, so forced), its row goes.
        #expect(await model.delete(row, force: true) == .deleted)
        await model.refresh()
        #expect(model.agent(host: "local", ref: started.ref) == nil)
        _ = try client.call(ProjectsRemove.self, ProjectParams(project: project.name))

        if let marker = environment["VIBE_TEST_MARKER"] {
            try Data("ok\n".utf8).write(to: URL(fileURLWithPath: marker))
        }
    }
}
