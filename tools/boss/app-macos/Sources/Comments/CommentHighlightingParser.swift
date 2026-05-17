import Foundation
import SwiftUI
import Textual

/// A MarkupParser that wraps Textual's built-in markdown parser and applies a yellow
/// background to quoted-text spans from attached comments, and an orange background
/// to the actively flashing span when a comment is clicked.
///
/// Phase 1: span matching is a case-sensitive substring search on the parser's plain-text
/// output. Phase 2 will switch to TextQuoteSelector-anchored ranges so highlights survive
/// doc edits and live at glyph precision rather than character-search approximation.
@MainActor
struct HighlightingMarkdownParser: MarkupParser {
    var highlightedTexts: [String]
    var flashingText: String?
    var baseURL: URL?

    private static let yellowColor = Color(nsColor: NSColor.systemYellow).opacity(0.45)
    private static let orangeColor = Color(nsColor: NSColor.systemOrange).opacity(0.55)

    init(highlightedTexts: [String], flashingText: String? = nil, baseURL: URL? = nil) {
        self.highlightedTexts = highlightedTexts
        self.flashingText = flashingText
        self.baseURL = baseURL
    }

    func attributedString(for input: String) throws -> AttributedString {
        var result = try AttributedStringMarkdownParser.markdown(baseURL: baseURL).attributedString(for: input)
        let plain = String(result.characters)

        for text in highlightedTexts where !text.isEmpty {
            highlight(text: text, color: Self.yellowColor, in: &result, plain: plain)
        }
        if let flashing = flashingText, !flashing.isEmpty {
            highlight(text: flashing, color: Self.orangeColor, in: &result, plain: plain)
        }

        return result
    }

    private func highlight(
        text: String,
        color: Color,
        in result: inout AttributedString,
        plain: String
    ) {
        var container = AttributeContainer()
        container.backgroundColor = color

        var searchStart = plain.startIndex
        while let matchRange = plain.range(of: text, range: searchStart..<plain.endIndex) {
            // Use character-level distances (not UTF-16 offsets) so the index
            // into result.characters is correct for all Unicode text including
            // non-BMP code points such as emoji that span two UTF-16 units.
            let startOffset = plain.distance(from: plain.startIndex, to: matchRange.lowerBound)
            let matchLength = plain.distance(from: matchRange.lowerBound, to: matchRange.upperBound)
            let startIdx = result.characters.index(result.characters.startIndex, offsetBy: startOffset)
            let endIdx = result.characters.index(startIdx, offsetBy: matchLength)
            result[startIdx..<endIdx].mergeAttributes(container)
            searchStart = matchRange.upperBound
        }
    }
}
