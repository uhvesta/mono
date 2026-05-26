import Foundation
import SwiftUI
import Textual

/// A MarkupParser that wraps Textual's built-in markdown parser and applies a yellow
/// background to the single quoted-text occurrence each comment is anchored to, and an
/// orange background to the actively flashing anchor when a comment is clicked.
///
/// Phase 1: each anchor carries a (quotedText, occurrenceIndex) pair so that two comments
/// on two different occurrences of the same word each highlight only their own instance.
/// Phase 2 will switch to TextQuoteSelector-anchored ranges so highlights survive doc
/// edits and live at glyph precision rather than character-search approximation.
@MainActor
struct HighlightingMarkdownParser: MarkupParser {
    var highlightedAnchors: [CommentAnchor]
    var flashingAnchor: CommentAnchor?
    var baseURL: URL?

    private static let yellowColor = Color(nsColor: NSColor.systemYellow).opacity(0.45)
    private static let orangeColor = Color(nsColor: NSColor.systemOrange).opacity(0.55)

    init(highlightedAnchors: [CommentAnchor], flashingAnchor: CommentAnchor? = nil, baseURL: URL? = nil) {
        self.highlightedAnchors = highlightedAnchors
        self.flashingAnchor = flashingAnchor
        self.baseURL = baseURL
    }

    func attributedString(for input: String) throws -> AttributedString {
        var result = try AttributedStringMarkdownParser.markdown(baseURL: baseURL).attributedString(for: input)
        let plain = String(result.characters)

        for anchor in highlightedAnchors where !anchor.quotedText.isEmpty {
            highlight(anchor: anchor, color: Self.yellowColor, in: &result, plain: plain)
        }
        if let flashing = flashingAnchor, !flashing.quotedText.isEmpty {
            highlight(anchor: flashing, color: Self.orangeColor, in: &result, plain: plain)
        }

        return result
    }

    /// Highlights only the occurrence at `anchor.occurrenceIndex` in the plain text.
    /// If `occurrenceIndex` exceeds the number of matches (e.g. document changed since
    /// the comment was created), no highlight is applied — a silent no-op is safer than
    /// highlighting the wrong span.
    private func highlight(
        anchor: CommentAnchor,
        color: Color,
        in result: inout AttributedString,
        plain: String
    ) {
        var container = AttributeContainer()
        container.backgroundColor = color

        var searchStart = plain.startIndex
        var currentOccurrence = 0
        while let matchRange = plain.range(of: anchor.quotedText, range: searchStart..<plain.endIndex) {
            if currentOccurrence == anchor.occurrenceIndex {
                // Use character-level distances (not UTF-16 offsets) so the index
                // into result.characters is correct for all Unicode text including
                // non-BMP code points such as emoji that span two UTF-16 units.
                let startOffset = plain.distance(from: plain.startIndex, to: matchRange.lowerBound)
                let matchLength = plain.distance(from: matchRange.lowerBound, to: matchRange.upperBound)
                let startIdx = result.characters.index(result.characters.startIndex, offsetBy: startOffset)
                let endIdx = result.characters.index(startIdx, offsetBy: matchLength)
                result[startIdx..<endIdx].mergeAttributes(container)
                return
            }
            currentOccurrence += 1
            searchStart = matchRange.upperBound
        }
    }
}
