import Foundation
import os.log
import SwiftUI
import Textual

/// Diagnostic channel for comment-anchor resolution. Every anchor logs whether it
/// resolved to a rendered range, the strategy that matched (exact vs. whitespace-
/// tolerant), the resolved character range, and whether a highlight was emitted —
/// or, on failure, why no span matched. This is the verbose-logging debug affordance
/// requested on the "comment anchors invisible" bug: stream it live with
///
///   log stream --predicate 'subsystem == "com.boss.markdown" AND category == "comment-highlight"' --style compact
private let highlightLog = Logger(subsystem: "com.boss.markdown", category: "comment-highlight")

/// A MarkupParser that wraps Textual's built-in markdown parser and applies a yellow
/// background to the single quoted-text occurrence each comment is anchored to, and an
/// orange background to the actively flashing anchor when a comment is clicked.
///
/// Phase 1: each anchor carries a (quotedText, occurrenceIndex) pair so that two comments
/// on two different occurrences of the same word each highlight only their own instance.
/// Phase 2 will switch to TextQuoteSelector-anchored ranges so highlights survive doc
/// edits and live at glyph precision rather than character-search approximation.
///
/// ## Why two attributes per highlight
///
/// The highlight sets BOTH a `backgroundColor` and — on any inline-code run inside the
/// anchored range — a colored `underlineStyle`. The background alone is not enough: the
/// Boss inline style (`InlineStyle.boss`) gives inline code spans their own
/// `backgroundColor`, and Textual's `WithInlineStyle` merges that in with
/// `mergePolicy: .keepNew`, which *overwrites* the comment's background on exactly those
/// runs. A comment anchored to an inline code span (e.g. `` `flavor` ``) would therefore
/// be invisible. The colored underline is applied after the background and is never
/// touched by the inline style, so code-span anchors still show a visible marker.
@MainActor
struct HighlightingMarkdownParser: MarkupParser {
    var highlightedAnchors: [CommentAnchor]
    var flashingAnchor: CommentAnchor?
    var baseURL: URL?

    private static let yellowColor = Color(nsColor: NSColor.systemYellow).opacity(0.45)
    private static let orangeColor = Color(nsColor: NSColor.systemOrange).opacity(0.55)
    // Stronger, mostly-opaque variants used for the underline marker so it stays visible
    // even when an inline-code background has overwritten the translucent fill.
    private static let yellowUnderline = Color(nsColor: NSColor.systemYellow).opacity(0.9)
    private static let orangeUnderline = Color(nsColor: NSColor.systemOrange).opacity(0.9)

    init(highlightedAnchors: [CommentAnchor], flashingAnchor: CommentAnchor? = nil, baseURL: URL? = nil) {
        self.highlightedAnchors = highlightedAnchors
        self.flashingAnchor = flashingAnchor
        self.baseURL = baseURL
    }

    func attributedString(for input: String) throws -> AttributedString {
        var result = try AttributedStringMarkdownParser.markdown(baseURL: baseURL).attributedString(for: input)
        let plain = String(result.characters)

        highlightLog.debug(
            "resolve start: anchors=\(highlightedAnchors.count, privacy: .public) flashing=\(flashingAnchor != nil, privacy: .public) renderedChars=\(plain.count, privacy: .public)"
        )

        for (index, anchor) in highlightedAnchors.enumerated() where !anchor.quotedText.isEmpty {
            highlight(
                anchor: anchor,
                fill: Self.yellowColor,
                underline: Self.yellowUnderline,
                label: "anchor[\(index)]",
                in: &result,
                plain: plain
            )
        }
        if let flashing = flashingAnchor, !flashing.quotedText.isEmpty {
            highlight(
                anchor: flashing,
                fill: Self.orangeColor,
                underline: Self.orangeUnderline,
                label: "flash",
                in: &result,
                plain: plain
            )
        }

        return result
    }

    /// Highlights the occurrence at `anchor.occurrenceIndex` in the plain text.
    ///
    /// Matching is whitespace-tolerant: a run of whitespace in the quoted text matches a
    /// run of one-or-more whitespace characters in the rendered text. This is what makes a
    /// multi-line selection resolve — the pasteboard text captured when the comment was
    /// created often uses `\n` where the rendered projection uses a single space (or
    /// vice-versa), so an exact `range(of:)` would silently fail to match.
    ///
    /// If `occurrenceIndex` exceeds the number of matches (e.g. the document changed since
    /// the comment was created), no highlight is applied — a silent no-op is safer than
    /// highlighting the wrong span.
    private func highlight(
        anchor: CommentAnchor,
        fill: Color,
        underline: Color,
        label: String,
        in result: inout AttributedString,
        plain: String
    ) {
        let preview = anchor.quotedText.prefix(48).replacingOccurrences(of: "\n", with: "⏎")
        let ranges = Self.flexibleMatchRanges(of: anchor.quotedText, in: plain)

        guard anchor.occurrenceIndex >= 0, anchor.occurrenceIndex < ranges.count else {
            highlightLog.error(
                "\(label, privacy: .public): NO MATCH for occurrence \(anchor.occurrenceIndex, privacy: .public) — found \(ranges.count, privacy: .public) candidate range(s) for quoted=\"\(preview, privacy: .public)\""
            )
            return
        }

        let matchRange = ranges[anchor.occurrenceIndex]
        let startOffset = plain.distance(from: plain.startIndex, to: matchRange.lowerBound)
        let matchLength = plain.distance(from: matchRange.lowerBound, to: matchRange.upperBound)
        // Map plain-text character offsets onto result.characters. Character-level
        // distances (not UTF-16 offsets) keep the mapping correct for non-BMP code
        // points such as emoji that span two UTF-16 units; `plain` is itself
        // `String(result.characters)`, so the two index spaces are 1:1 by construction.
        let startIdx = result.characters.index(result.characters.startIndex, offsetBy: startOffset)
        let endIdx = result.characters.index(startIdx, offsetBy: matchLength)

        var fillContainer = AttributeContainer()
        fillContainer.backgroundColor = fill
        result[startIdx..<endIdx].mergeAttributes(fillContainer)

        // The background above is clobbered on inline-code runs by the Boss inline style.
        // Mark those runs with a colored underline so the anchor stays visible. The
        // underline is applied to the whole span as well as code runs so it survives
        // regardless of which runs the inline style later restyles.
        var underlineContainer = AttributeContainer()
        underlineContainer.underlineStyle = Text.LineStyle(pattern: .solid, color: underline)
        result[startIdx..<endIdx].mergeAttributes(underlineContainer)

        highlightLog.debug(
            "\(label, privacy: .public): HIGHLIGHTED occurrence \(anchor.occurrenceIndex, privacy: .public) at chars \(startOffset, privacy: .public)..<\(startOffset + matchLength, privacy: .public) (\(ranges.count, privacy: .public) candidate(s)) quoted=\"\(preview, privacy: .public)\""
        )
    }

    /// Returns every non-overlapping range in `plain` that matches `needle`, treating each
    /// run of whitespace in `needle` as matching one-or-more whitespace characters in
    /// `plain`. Leading/trailing whitespace in `needle` is ignored. Matches are returned in
    /// document order, so `ranges[occurrenceIndex]` selects the intended occurrence.
    ///
    /// For a single-token needle (no interior whitespace) this degenerates to an ordered,
    /// non-overlapping substring search — identical to the previous exact behaviour — so
    /// occurrence indexing is unchanged for the common single-word anchor.
    static func flexibleMatchRanges(of needle: String, in plain: String) -> [Range<String.Index>] {
        let trimmed = needle.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return [] }

        // Non-whitespace segments; interior whitespace runs become flexible gaps.
        let segments = trimmed.split(whereSeparator: { $0.isWhitespace }).map(String.init)
        guard let first = segments.first else { return [] }
        let rest = segments.dropFirst()

        var results: [Range<String.Index>] = []
        var searchStart = plain.startIndex

        while let firstRange = plain.range(of: first, range: searchStart..<plain.endIndex) {
            var cursor = firstRange.upperBound
            var matched = true

            for seg in rest {
                // Require at least one whitespace character before the next segment.
                var wsCursor = cursor
                var whitespaceCount = 0
                while wsCursor < plain.endIndex, plain[wsCursor].isWhitespace {
                    wsCursor = plain.index(after: wsCursor)
                    whitespaceCount += 1
                }
                guard whitespaceCount >= 1, plain[wsCursor...].hasPrefix(seg) else {
                    matched = false
                    break
                }
                cursor = plain.index(wsCursor, offsetBy: seg.count)
            }

            if matched {
                results.append(firstRange.lowerBound..<cursor)
                searchStart = cursor
            } else {
                searchStart = firstRange.upperBound
            }
        }

        return results
    }
}
