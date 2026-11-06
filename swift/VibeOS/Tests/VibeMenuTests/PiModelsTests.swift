import Foundation
import Testing

@testable import VibeMenu
import VibeRPC

private let piList = [
    PiProvider(
        id: "openai", name: "OpenAI", signedIn: true,
        models: [PiModel(id: "gpt-5", name: "GPT-5"), PiModel(id: "gpt-5-mini")]),
    PiProvider(
        id: "opencode", name: "OpenCode Zen", signedIn: false,
        models: [PiModel(id: "deepseek-v4.1-flash"), PiModel(id: "gpt-5")]),
    PiProvider(id: "openrouter", name: "OpenRouter", signedIn: true, models: [PiModel(id: "anthropic/claude-x")]),
    PiProvider(id: "amazon-bedrock", name: "Amazon Bedrock", signedIn: false, ambient: true, models: [PiModel(id: "nova")]),
]

@Test func aTypedModelFindsWhatPiLists() {
    #expect(matchPiModel("openai/gpt-5", in: piList) == .listed("openai/gpt-5"))
    #expect(matchPiModel(" deepseek-v4.1-flash ", in: piList) == .listed("opencode/deepseek-v4.1-flash"))
    #expect(matchPiModel("gpt-5", in: piList) == .several(["openai/gpt-5", "opencode/gpt-5"]))
    // An id can have a slash of its own.
    #expect(matchPiModel("anthropic/claude-x", in: piList) == .listed("openrouter/anthropic/claude-x"))
    #expect(matchPiModel("mine/own", in: piList) == .unlisted)
    #expect(matchPiModel("", in: piList) == .unlisted)
    #expect(matchPiModel("gpt-5", in: []) == .unlisted)
}

@Test func searchingFindsModelsAndProviders() {
    #expect(piModels(matching: "", in: piList).map(\.id) == ["openai", "opencode", "openrouter", "amazon-bedrock"])
    let flash = piModels(matching: "FLASH", in: piList)
    #expect(flash.map(\.id) == ["opencode"])
    #expect(flash.first?.listedModels.map(\.id) == ["deepseek-v4.1-flash"])
    #expect(piModels(matching: "GPT-5 ", in: piList).first?.listedModels.map(\.id) == ["gpt-5", "gpt-5-mini"])
    #expect(piModels(matching: "zen", in: piList).first?.listedModels.count == 2, "a provider brings all its own")
    #expect(piModels(matching: "nothing like it", in: piList).isEmpty)
}

/// VibeOS signs nobody in to Pi (#158): no call does, and what's kept is all still there.
@Test func nothingInTheContractSignsAnyoneInToPi() {
    #expect(!VibeRPC.methods.contains { $0.hasPrefix("pi.login") })
    for kept in [PiStatusGet.name, PiInstall.name, PiForget.name, PiModelsGet.name] {
        #expect(VibeRPC.methods.contains(kept), "\(kept)")
    }
}

/// A host on an older VibeOS says how each provider signs in. That's no matter here.
@Test func aListFromAnOlderHostStillDecodes() throws {
    let older = #"""
        {"id": "openai", "name": "OpenAI", "signed_in": true, "ambient": false, "oauth": true,
         "oauth_label": "Sign in with ChatGPT", "api_key": true, "models": [{"id": "gpt-5"}]}
        """#
    let provider = try JSONDecoder().decode(PiProvider.self, from: Data(older.utf8))
    #expect(provider.id == "openai" && provider.signedIn)
    #expect(provider.listedModels.map(\.id) == ["gpt-5"])
}
