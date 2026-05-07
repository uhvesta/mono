import Foundation

/// In-character recreational flavor lines, one set per crew member.
/// Picked deterministically per (slot, idle-cycle) so an idle pane
/// keeps the same line until it transitions out of idle — the cycle
/// id is whatever the caller bumps on enter-idle.
enum TrekIdleFlavor {
    static func lines(for character: TrekCharacter) -> [String] {
        switch character {
        case .riker:
            return [
                "Riker is vacationing on Risa.",
                "Riker is playing trombone in Ten Forward.",
                "Riker is beating Worf at poker.",
                "Riker is on the bridge, leaning.",
                "Riker is cooking eggs for the senior staff.",
                "Riker is reviewing Klingon opera scores.",
                "Riker is challenging anyone to a game of poker.",
                "Riker is teaching the holodeck how to mix a Saurian brandy.",
            ]
        case .data:
            return [
                "Data is practicing his violin.",
                "Data is painting Spot.",
                "Data is composing a sonnet about the warp core.",
                "Data is studying human humor (still inconclusive).",
                "Data is rehearsing Shakespeare on the holodeck.",
                "Data is teaching Spot a new trick. Spot is unimpressed.",
                "Data is calculating pi to a few extra decimal places.",
                "Data is contemplating the works of Sherlock Holmes.",
            ]
        case .worf:
            return [
                "Worf is in the holodeck on calisthenics program four.",
                "Worf is brewing prune juice for Guinan.",
                "Worf is studying Klingon battle poetry.",
                "Worf is meditating on the path of Kahless.",
                "Worf is sharpening his bat'leth. Again.",
                "Worf is grumbling about the lack of glory in administrative work.",
                "Worf is recounting the Battle of HarOs to anyone who will listen.",
            ]
        case .laforge:
            return [
                "La Forge is recalibrating the dilithium matrix for fun.",
                "La Forge is in Engineering, talking to the warp core.",
                "La Forge is rebuilding a 1957 Corvette on the holodeck.",
                "La Forge is bouncing efficiency ideas off Data.",
                "La Forge is reading the latest Daystrom Institute journal.",
                "La Forge is tuning his VISOR.",
                "La Forge is running a tabletop holo-program with Reg Barclay.",
            ]
        case .troi:
            return [
                "Troi is having a chocolate sundae in Ten Forward.",
                "Troi is leading a counseling session on Deck 7.",
                "Troi is reading a Betazoid romance novel.",
                "Troi is sensing… great hunger. From herself. For chocolate.",
                "Troi is teaching beginning meditation in the arboretum.",
                "Troi is sparring with Worf on the holodeck.",
            ]
        case .crusher:
            return [
                "Crusher is rehearsing for the next ship's theatrical.",
                "Crusher is dancing in Sickbay between shifts.",
                "Crusher is gardening in the arboretum.",
                "Crusher is reviewing autopsy notes she'd rather not.",
                "Crusher is teaching Wesley not to touch that.",
                "Crusher is reading up on Trill symbiont biology.",
            ]
        case .yar:
            return [
                "Yar is running a security drill in the cargo bay.",
                "Yar is sparring with the Aikido program on the holodeck.",
                "Yar is cleaning her phaser. Slowly. Deliberately.",
                "Yar is reviewing tactical scans from the last shift.",
                "Yar is teaching hand-to-hand to the new ensigns.",
            ]
        case .obrien:
            return [
                "O'Brien is reassembling the transporter buffer on a hunch.",
                "O'Brien is at Quark's, nursing a synthale.",
                "O'Brien is playing darts with Bashir.",
                "O'Brien is fixing whatever Sisko broke on the Defiant.",
                "O'Brien is muttering at a Cardassian conduit.",
                "O'Brien is calling Keiko back, eventually.",
            ]
        }
    }

    /// Pick a stable line for `(character, cycle)`. Same cycle id →
    /// same line, so the idle pane doesn't flicker on every render.
    /// Different cycles (e.g. after a run completes and the slot goes
    /// idle again) pick a fresh line from the same pool.
    static func line(for character: TrekCharacter, cycle: Int) -> String {
        let pool = lines(for: character)
        guard !pool.isEmpty else { return "" }
        let index = abs(cycle) % pool.count
        return pool[index]
    }
}
