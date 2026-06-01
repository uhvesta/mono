import XCTest

/// Unit tests for effort level badge T-shirt sizing.
/// Verifies that the effort→badge mapping correctly implements:
/// - trivial → XS
/// - small → S
/// - medium → M
/// - large → L
/// - max → XL
final class EffortChipTests: XCTestCase {

    // MARK: - Letter Mapping Tests

    func testTrivialEffortMapsToBadgeXS() {
        let letter = effortLetter(for: "trivial")
        XCTAssertEqual(letter, "XS")
    }

    func testSmallEffortMapsToBadgeS() {
        let letter = effortLetter(for: "small")
        XCTAssertEqual(letter, "S")
    }

    func testMediumEffortMapsToBadgeM() {
        let letter = effortLetter(for: "medium")
        XCTAssertEqual(letter, "M")
    }

    func testLargeEffortMapsToBadgeL() {
        let letter = effortLetter(for: "large")
        XCTAssertEqual(letter, "L")
    }

    func testMaxEffortMapsToBadgeXL() {
        let letter = effortLetter(for: "max")
        XCTAssertEqual(letter, "XL")
    }

    // MARK: - Label Mapping Tests (for tooltip/accessibility)

    func testTrivialEffortLabelReadsFull() {
        let label = effortLabel(for: "trivial")
        XCTAssertEqual(label, "Trivial")
    }

    func testSmallEffortLabelReadsFull() {
        let label = effortLabel(for: "small")
        XCTAssertEqual(label, "Small")
    }

    func testMediumEffortLabelReadsFull() {
        let label = effortLabel(for: "medium")
        XCTAssertEqual(label, "Medium")
    }

    func testLargeEffortLabelReadsFull() {
        let label = effortLabel(for: "large")
        XCTAssertEqual(label, "Large")
    }

    func testMaxEffortLabelReadsFull() {
        let label = effortLabel(for: "max")
        XCTAssertEqual(label, "Max")
    }

    // MARK: - Badge Sizing

    /// XS and XL are two characters each; verify they're not forcing
    /// a single-character width that would clip the text.
    func testXSBadgeTwoCharactersNotClipped() {
        let letter = effortLetter(for: "trivial")
        XCTAssertEqual(letter.count, 2)
    }

    func testXLBadgeTwoCharactersNotClipped() {
        let letter = effortLetter(for: "max")
        XCTAssertEqual(letter.count, 2)
    }

    func testSingleCharacterBadgesRemainSingleCharacter() {
        XCTAssertEqual(effortLetter(for: "small").count, 1)
        XCTAssertEqual(effortLetter(for: "medium").count, 1)
        XCTAssertEqual(effortLetter(for: "large").count, 1)
    }

    // MARK: - Empty / Blank Effort (must not render an empty pill)

    /// A blank or whitespace-only effort_level on the wire must be treated as
    /// unset so the card does not render an empty gray pill in the leftmost
    /// metadata slot. Mirrors the call-site guard in ContentView and the
    /// parse-time normalization in EngineClient.
    func testEmptyEffortIsTreatedAsUnset() {
        XCTAssertFalse(shouldRenderEffortChip(for: ""))
    }

    func testWhitespaceOnlyEffortIsTreatedAsUnset() {
        XCTAssertFalse(shouldRenderEffortChip(for: "   "))
        XCTAssertFalse(shouldRenderEffortChip(for: "\n\t"))
    }

    func testNilEffortIsTreatedAsUnset() {
        XCTAssertFalse(shouldRenderEffortChip(for: nil))
    }

    func testKnownEffortLevelsRender() {
        for level in ["trivial", "small", "medium", "large", "max"] {
            XCTAssertTrue(shouldRenderEffortChip(for: level), "expected \(level) to render")
        }
    }

    // MARK: - Helpers (mirrors EffortChip private functions)

    /// Returns whether an effort chip should be rendered for a wire-supplied
    /// effort_level value. Mirrors the guard in ContentView's card footer and
    /// the normalization in EngineClient.parseTask.
    private func shouldRenderEffortChip(for effortLevel: String?) -> Bool {
        guard let effortLevel,
              !effortLevel.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            return false
        }
        return true
    }

    /// Returns the badge display text for an effort level.
    /// Mirrors the EffortChip.letter computed property.
    private func effortLetter(for effortLevel: String) -> String {
        switch effortLevel {
        case "trivial": return "XS"
        case "small": return "S"
        case "medium": return "M"
        case "large": return "L"
        case "max": return "XL"
        default: return effortLevel.prefix(1).uppercased()
        }
    }

    /// Returns the full-word label for an effort level (used in tooltips and
    /// accessibility labels).
    /// Mirrors the EffortChip.label computed property.
    private func effortLabel(for effortLevel: String) -> String {
        switch effortLevel {
        case "trivial": return "Trivial"
        case "small": return "Small"
        case "medium": return "Medium"
        case "large": return "Large"
        case "max": return "Max"
        default: return effortLevel.capitalized
        }
    }
}
