import AppKit
import Foundation
import SwiftUI

#if canImport(Sparkle)
    import Sparkle
#endif

/// Updates through Sparkle (PRD: Auth, security and trust, Distribution):
/// a release build checks the appcast its Info.plist names (SUFeedURL),
/// takes only updates signed with the key it names (SUPublicEDKey), and
/// asks before installing. scripts/release-macos.sh builds with Sparkle
/// when it has the key to sign updates with; any other build has no
/// updater, and Settings shows none.
@MainActor
final class Updater: ObservableObject {
    static let shared = Updater()

    /// Whether this build can update itself at all.
    @Published private(set) var available = false
    /// Whether a check can start now: not while one runs.
    @Published private(set) var canCheck = false

    #if canImport(Sparkle)
        private var controller: SPUStandardUpdaterController?
        private var watching: NSKeyValueObservation?
    #endif

    private init() {}

    /// Starts the updater, once: from here it checks on Sparkle's schedule.
    func start() {
        #if canImport(Sparkle)
            guard controller == nil,
                Bundle.main.object(forInfoDictionaryKey: "SUFeedURL") != nil,
                Bundle.main.object(forInfoDictionaryKey: "SUPublicEDKey") != nil
            else { return }
            let controller = SPUStandardUpdaterController(
                startingUpdater: true, updaterDelegate: nil, userDriverDelegate: nil)
            self.controller = controller
            available = true
            canCheck = controller.updater.canCheckForUpdates
            watching = controller.updater.observe(\.canCheckForUpdates, options: [.new]) { [weak self] updater, _ in
                let can = updater.canCheckForUpdates
                DispatchQueue.main.async { MainActor.assumeIsolated { self?.canCheck = can } }
            }
        #endif
    }

    /// Check for Updates…: Sparkle's own window says how it goes.
    func check() {
        #if canImport(Sparkle)
            NSApp.activate()
            controller?.checkForUpdates(nil)
        #endif
    }
}
