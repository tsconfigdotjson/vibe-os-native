import Foundation
import Testing

@testable import VibeMenu
import VibeRPC

private let piList = [
    PiProvider(
        id: "openai", name: "OpenAI", signedIn: true, oauth: true,
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

@Test func onlyAModelsOwnProviderCanNeedASignIn() {
    #expect(piProviderToSignIn(for: "opencode/gpt-5", in: piList)?.id == "opencode")
    #expect(piProviderToSignIn(for: "deepseek-v4.1-flash", in: piList)?.id == "opencode")
    // One Pi knows, with a model it doesn't list.
    #expect(piProviderToSignIn(for: "opencode/new-one", in: piList)?.id == "opencode")
    #expect(piProviderToSignIn(for: "openai/gpt-5", in: piList) == nil)
    #expect(piProviderToSignIn(for: "amazon-bedrock/nova", in: piList) == nil, "it may have what's around it")
    #expect(piProviderToSignIn(for: "gpt-5", in: piList) == nil, "it names nobody yet")
    #expect(piProviderToSignIn(for: "mine/own", in: piList) == nil)
    #expect(!piHasNoLogin(piList))
    #expect(piHasNoLogin([PiProvider(id: "a", name: "A", signedIn: false)]))
    #expect(
        piHasNoLogin([
            PiProvider(id: "a", name: "A", signedIn: false),
            PiProvider(id: "amazon-bedrock", name: "Amazon Bedrock", signedIn: false, ambient: true),
        ]), "every real list has one of those")
    #expect(!piHasNoLogin([]), "no list says nothing")
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

@Test func aRefusalOffersItsSignIn() {
    let refusal = RPCError(
        code: RPCErrorCode.conflict.rawValue, message: "Pi isn't signed in",
        data: .object(["pi_sign_in": .object(["provider": .string("opencode"), "target": .string("vm")])]))
    #expect(piSignInOffer(from: VibedClientError.rpc(refusal)) == PiSignInOffer(provider: "opencode", target: .vm))
    let anyone = RPCError(
        code: RPCErrorCode.conflict.rawValue, message: "Pi isn't signed in",
        data: .object(["pi_sign_in": .object(["provider": .null, "target": .string("native")])]))
    #expect(piSignInOffer(from: VibedClientError.rpc(anyone)) == PiSignInOffer(provider: nil, target: .native))
    let other = RPCError(code: RPCErrorCode.conflict.rawValue, message: "That name is taken")
    #expect(piSignInOffer(from: VibedClientError.rpc(other)) == nil)
}
