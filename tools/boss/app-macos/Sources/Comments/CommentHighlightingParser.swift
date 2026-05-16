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

    private static let yellowColor = Color(nsColor: NSColor.systemYellow).opacity(0.45)
    private static let orangeColor = Color(nsColor: NSColor.systemOrange).opacity(0.55)

    init(highlightedTexts: [String], flashingText: String? = nil) {
        self.highlightedTexts = highlightedTexts
        self.flashingText = flashingText
    }

    func attributedString(for input: String) throws -> AttributedString {
        var result = try AttributedStringMarkdownParser.markdown().attributedString(for: input)
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
            let nsRange = NSRange(matchRange, in: plain)
            let startIdx = result.characters.index(
                result.characters.startIndex, offsetBy: nsRange.location)
            let endIdx = result.characters.index(startIdx, offsetBy: nsRange.length)
            // Subscript to get the AttributedSubstring and merge attributes on that range.
            result[startIdx..<endIdx].mergeAttributes(container)
            searchStart = matchRange.upperBound
        }
    }
}
