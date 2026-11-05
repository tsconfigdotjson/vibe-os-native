import Foundation
import VibeRPC

/// What Pi's list makes of what was typed for a model (PRD: Profiles). None of it stops
/// anything being saved: Pi has the last word, and may know what it doesn't list.
public enum PiModelMatch: Equatable, Sendable {
    /// One model Pi lists, in full.
    case listed(String)
    /// A bare id more than one provider has, which Pi would refuse, in full.
    case several([String])
    case unlisted
}

/// `text` against Pi's list. A bare id with one owner comes back as `provider/id`, as an
/// id can have a `/` in it, for a gateway's: `anthropic/x` may be `openrouter`'s.
public func matchPiModel(_ text: String, in providers: [PiProvider]) -> PiModelMatch {
    let model = text.trimmingCharacters(in: .whitespaces)
    guard !model.isEmpty else { return .unlisted }
    if let slash = model.firstIndex(of: "/") {
        let (name, id) = (model[..<slash], model[model.index(after: slash)...])
        if providers.contains(where: { $0.id == name && $0.listedModels.contains { $0.id == id } }) {
            return .listed(model)
        }
    }
    let owners = providers.filter { $0.listedModels.contains { $0.id == model } }.map { "\($0.id)/\(model)" }
    switch owners.count {
    case 0: return .unlisted
    case 1: return .listed(owners[0])
    default: return .several(owners)
    }
}

/// The provider `model` belongs to, if Pi knows it's not signed in to it and can be.
/// One that signs in from what's around it, as Bedrock can, is left to try.
public func piProviderToSignIn(for model: String, in providers: [PiProvider]) -> PiProvider? {
    let named: String
    switch matchPiModel(model, in: providers) {
    case .listed(let full): named = full
    case .several: return nil
    case .unlisted: named = model
    }
    guard let slash = named.firstIndex(of: "/") else { return nil }
    let name = named[..<slash]
    return providers.first { $0.id == name && !$0.signedIn && !$0.ambient && ($0.oauth || $0.apiKey) }
}

/// Whether Pi has nobody to answer with at all, as far as its list can tell.
public func piHasNoLogin(_ providers: [PiProvider]) -> Bool {
    !providers.isEmpty && !providers.contains { $0.signedIn || $0.ambient }
}

/// Pi's providers and models with `query` in their names, signed-in providers first as
/// Pi's list has them. A provider's own name matching brings all its models.
public func piModels(matching query: String, in providers: [PiProvider]) -> [PiProvider] {
    let words = query.trimmingCharacters(in: .whitespaces).lowercased()
    guard !words.isEmpty else { return providers.filter { !$0.listedModels.isEmpty } }
    return providers.compactMap { provider in
        if provider.id.lowercased().contains(words) || provider.name.lowercased().contains(words) {
            return provider.listedModels.isEmpty ? nil : provider
        }
        let models = provider.listedModels.filter { model in
            "\(provider.id)/\(model.id)".lowercased().contains(words)
                || (model.name?.lowercased().contains(words) ?? false)
        }
        guard !models.isEmpty else { return nil }
        var found = provider
        found.models = models
        return found
    }
}

/// What vibed offers when a Pi agent was refused for not being signed in.
public struct PiSignInOffer: Hashable, Sendable {
    /// Who to sign in to. None for a model that names no one.
    public var provider: String?
    public var target: PiTarget
}

/// That offer, if `error` carries one.
public func piSignInOffer(from error: any Error) -> PiSignInOffer? {
    guard case VibedClientError.rpc(let rpc) = error,
        case .object(let data)? = rpc.data,
        case .object(let offer)? = data["pi_sign_in"],
        case .string(let target)? = offer["target"],
        let target = PiTarget(rawValue: target)
    else { return nil }
    var provider: String?
    if case .string(let name)? = offer["provider"] { provider = name }
    return PiSignInOffer(provider: provider, target: target)
}
