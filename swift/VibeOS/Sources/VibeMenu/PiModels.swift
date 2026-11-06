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
