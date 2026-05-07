import AppKit
import Foundation

/// Asset keys for the TNG portraits vendored under
/// `Sources/Resources/TrekIcons/<size>/<key>.png`. The key matches the
/// filename stem so the loader can build a path mechanically. Eight
/// of these (Riker..O'Brien) form the worker roster; `picard` is used
/// for the Boss pane and is intentionally not part of `forSlot`.
enum TrekCharacter: String, CaseIterable {
    case riker
    case data
    case worf
    case laforge
    case troi
    case crusher
    case yar
    case obrien
    case picard

    /// Display name as rendered by `WorkerNames.roster` for the same
    /// slot. Kept in lock-step with that roster — slot 1 = Riker,
    /// slot 2 = Data, …, slot 8 = O'Brien.
    var displayName: String {
        switch self {
        case .riker: "Riker"
        case .data: "Data"
        case .worf: "Worf"
        case .laforge: "La Forge"
        case .troi: "Troi"
        case .crusher: "Crusher"
        case .yar: "Yar"
        case .obrien: "O'Brien"
        case .picard: "Picard"
        }
    }

    /// Slot 1..8 → character. Slots beyond the roster (the engine caps
    /// at 8 today, but `WorkerNames` wraps modulo a longer roster) get
    /// `nil` and the UI falls back to text-only.
    static func forSlot(_ slotId: Int) -> TrekCharacter? {
        let roster: [TrekCharacter] = [
            .riker, .data, .worf, .laforge,
            .troi, .crusher, .yar, .obrien,
        ]
        guard slotId >= 1, slotId <= roster.count else { return nil }
        return roster[slotId - 1]
    }
}

enum TrekIconSize: String {
    case xsmall
    case small
    case medium
    case large
}

/// Loads bundled TNG portraits. Resources live under
/// `Sources/Resources/TrekIcons/...` and are processed by SwiftPM
/// (`Bundle.module`). The Bazel build path doesn't bundle them today
/// — callers must tolerate `nil` (the UI keeps text-only fallback).
enum TrekIconAssets {
    private struct CacheKey: Hashable {
        let character: TrekCharacter
        let size: TrekIconSize
    }

    // Decoded NSImages are immutable for our consumers (they only
    // render via SwiftUI `Image(nsImage:)`), so handing the same
    // instance to many views is safe. The roster has <20 distinct
    // (character, size) pairs over the app lifetime — no eviction
    // needed.
    private static let lock = NSLock()
    nonisolated(unsafe) private static var cache: [CacheKey: NSImage] = [:]
    nonisolated(unsafe) private static var negativeCache: Set<CacheKey> = []

    static func image(_ character: TrekCharacter, size: TrekIconSize) -> NSImage? {
        let key = CacheKey(character: character, size: size)
        lock.lock()
        if let cached = cache[key] {
            lock.unlock()
            return cached
        }
        if negativeCache.contains(key) {
            lock.unlock()
            return nil
        }
        lock.unlock()

        let loaded = loadFromBundle(character: character, size: size)

        lock.lock()
        if let loaded {
            cache[key] = loaded
        } else {
            negativeCache.insert(key)
        }
        lock.unlock()
        return loaded
    }

    private static func loadFromBundle(character: TrekCharacter, size: TrekIconSize) -> NSImage? {
        let resource = "TrekIcons/\(size.rawValue)/\(character.rawValue)"
        #if SWIFT_PACKAGE
        if let url = Bundle.module.url(forResource: resource, withExtension: "png"),
           let image = NSImage(contentsOf: url) {
            return image
        }
        #endif
        return nil
    }
}
