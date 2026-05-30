import AppKit
import SwiftUI
import XCTest
@testable import Boss

/// Tests for the Phase 1 in-memory comment system.
///
/// Four layout-correctness checks mirror the design doc's acceptance
/// criteria: no comments, single comment, multiple comments, and
/// mid-authoring (popover open). Each check instantiates the real view
/// hierarchy via `NSHostingView` — the same approach used in
/// `MarkdownViewerViewTests` — so a SwiftUI regression that breaks
/// layout fails here rather than silently at runtime.
@MainActor
final class CommentLayerTests: XCTestCase {

    // MARK: - Comment model

    func testCommentModelEquality() {
        let id = UUID()
        let date = Date()
        let a = Comment(id: id, quotedText: "hello", occurrenceIndex: 0, body: "world", createdAt: date)
        let b = Comment(id: id, quotedText: "hello", occurrenceIndex: 0, body: "world", createdAt: date)
        XCTAssertEqual(a, b)
    }

    func testCommentModelIdentityDiffersForDifferentIDs() {
        let date = Date()
        let a = Comment(id: UUID(), quotedText: "x", occurrenceIndex: 0, body: "y", createdAt: date)
        let b = Comment(id: UUID(), quotedText: "x", occurrenceIndex: 0, body: "y", createdAt: date)
        XCTAssertNotEqual(a, b)
    }

    func testCommentAnchorEquality() {
        let a = CommentAnchor(quotedText: "foo", occurrenceIndex: 1)
        let b = CommentAnchor(quotedText: "foo", occurrenceIndex: 1)
        let c = CommentAnchor(quotedText: "foo", occurrenceIndex: 2)
        XCTAssertEqual(a, b)
        XCTAssertNotEqual(a, c)
    }

    func testCommentAnchorReflectsOccurrenceIndex() {
        let date = Date()
        let c = Comment(id: UUID(), quotedText: "rename", occurrenceIndex: 1, body: "note", createdAt: date)
        XCTAssertEqual(c.anchor.quotedText, "rename")
        XCTAssertEqual(c.anchor.occurrenceIndex, 1)
    }

    // MARK: - CommentLayer (in-memory state)

    func testAddCommentAppendsToArray() {
        let layer = CommentLayer()
        XCTAssertTrue(layer.comments.isEmpty)
        layer.addComment(quoted: "selected text", body: "my comment")
        XCTAssertEqual(layer.comments.count, 1)
        XCTAssertEqual(layer.comments[0].quotedText, "selected text")
        XCTAssertEqual(layer.comments[0].body, "my comment")
    }

    func testAddCommentIgnoresBlankBody() {
        let layer = CommentLayer()
        layer.addComment(quoted: "some text", body: "   ")
        XCTAssertTrue(layer.comments.isEmpty)
    }

    func testAddCommentTrimsBodyWhitespace() {
        let layer = CommentLayer()
        layer.addComment(quoted: "", body: "  hello  ")
        XCTAssertEqual(layer.comments[0].body, "hello")
    }

    func testDismissRemovesComment() {
        let layer = CommentLayer()
        layer.addComment(quoted: "a", body: "first")
        layer.addComment(quoted: "b", body: "second")
        let toRemove = layer.comments[0]
        layer.dismiss(toRemove)
        XCTAssertEqual(layer.comments.count, 1)
        XCTAssertEqual(layer.comments[0].body, "second")
    }

    func testAddCommentClosesPopoverAndClearsPending() {
        let layer = CommentLayer()
        layer.pendingQuotedText = "selection"
        layer.isShowingPopover = true
        layer.addComment(quoted: "selection", body: "note")
        XCTAssertFalse(layer.isShowingPopover)
        XCTAssertEqual(layer.pendingQuotedText, "")
    }

    // MARK: - View: no comments state

    func testMarkdownViewerWithCommentsRendersWhenEmpty() {
        let view = MarkdownViewerView(title: "Test Doc", source: "# Hello\n\nSome content.")
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 760, height: 640)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
        XCTAssertGreaterThan(hosting.fittingSize.width, 0)
    }

    // MARK: - View: single comment state

    func testCommentSidebarWithSingleComment() {
        let layer = CommentLayer()
        layer.addComment(quoted: "the quick brown fox", body: "This needs clarification.")
        let view = CommentSidebar(layer: layer)
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 280, height: 600)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
    }

    // MARK: - View: multiple comments state

    func testCommentSidebarWithMultipleComments() {
        let layer = CommentLayer()
        layer.addComment(quoted: "first selection", body: "First comment.")
        layer.addComment(quoted: "second selection", body: "Second comment.")
        layer.addComment(quoted: "", body: "Comment without a quote.")
        let view = CommentSidebar(layer: layer)
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 280, height: 600)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
    }

    // MARK: - View: mid-authoring state (popover open)

    func testCommentPopoverRendersWithQuotedText() {
        let layer = CommentLayer()
        layer.pendingQuotedText = "the selected markdown span"
        let view = CommentPopover(layer: layer)
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 400, height: 400)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
        XCTAssertGreaterThan(hosting.fittingSize.width, 0)
    }

    func testCommentPopoverRendersWithoutQuotedText() {
        let layer = CommentLayer()
        layer.pendingQuotedText = ""
        let view = CommentPopover(layer: layer)
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 400, height: 400)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
    }

    // MARK: - HighlightingMarkdownParser: single-occurrence correctness

    /// Returns true if the run containing the character at `charOffset` in `plain`
    /// has a non-nil backgroundColor attribute in the attributed string.
    private func isHighlighted(at charOffset: Int, in result: AttributedString) -> Bool {
        let idx = result.characters.index(result.characters.startIndex, offsetBy: charOffset)
        return result.runs.contains { run in
            run.range.contains(idx) && run.swiftUI.backgroundColor != nil
        }
    }

    /// Verifies that HighlightingMarkdownParser applies a yellow background to
    /// every quoted-text span when two comments reference different words in the
    /// same document.  Regression test for the bug where only the first
    /// comment's text received a highlight while the second was silently skipped.
    func testHighlightingParserHighlightsBothCommentedTexts() throws {
        let source = "The fox jumped over the lazy dog and the cat sat quietly."
        let parser = HighlightingMarkdownParser(highlightedAnchors: [
            CommentAnchor(quotedText: "fox", occurrenceIndex: 0),
            CommentAnchor(quotedText: "cat", occurrenceIndex: 0),
        ])
        let result = try parser.attributedString(for: source)
        let plain = String(result.characters)

        guard let foxRange = plain.range(of: "fox") else {
            return XCTFail("'fox' not found in rendered plain text")
        }
        let foxOffset = plain.distance(from: plain.startIndex, to: foxRange.lowerBound)
        XCTAssertTrue(
            isHighlighted(at: foxOffset, in: result),
            "'fox' span must carry a backgroundColor attribute"
        )

        guard let catRange = plain.range(of: "cat") else {
            return XCTFail("'cat' not found in rendered plain text")
        }
        let catOffset = plain.distance(from: plain.startIndex, to: catRange.lowerBound)
        XCTAssertTrue(
            isHighlighted(at: catOffset, in: result),
            "'cat' span must carry a backgroundColor attribute — second comment must be highlighted"
        )
    }

    /// A comment anchored to occurrenceIndex=0 of a repeated word highlights only the
    /// FIRST occurrence; the second must NOT be highlighted.
    func testHighlightingParserHighlightsOnlyFirstOccurrence() throws {
        let source = "alpha beta alpha gamma"
        let parser = HighlightingMarkdownParser(
            highlightedAnchors: [CommentAnchor(quotedText: "alpha", occurrenceIndex: 0)]
        )
        let result = try parser.attributedString(for: source)
        let plain = String(result.characters)

        // First occurrence: should be highlighted.
        guard let firstRange = plain.range(of: "alpha") else {
            return XCTFail("'alpha' not found")
        }
        let firstOffset = plain.distance(from: plain.startIndex, to: firstRange.lowerBound)
        XCTAssertTrue(
            isHighlighted(at: firstOffset, in: result),
            "First 'alpha' (occurrenceIndex=0) must be highlighted"
        )

        // Second occurrence: must NOT be highlighted.
        guard let secondRange = plain.range(of: "alpha", range: firstRange.upperBound..<plain.endIndex) else {
            return XCTFail("Second 'alpha' not found")
        }
        let secondOffset = plain.distance(from: plain.startIndex, to: secondRange.lowerBound)
        XCTAssertFalse(
            isHighlighted(at: secondOffset, in: result),
            "Second 'alpha' must NOT be highlighted when only occurrenceIndex=0 is anchored"
        )
    }

    /// A comment anchored to occurrenceIndex=1 of a repeated word highlights only the
    /// SECOND occurrence; the first must NOT be highlighted.
    func testHighlightingParserHighlightsOnlySecondOccurrence() throws {
        let source = "alpha beta alpha gamma"
        let parser = HighlightingMarkdownParser(
            highlightedAnchors: [CommentAnchor(quotedText: "alpha", occurrenceIndex: 1)]
        )
        let result = try parser.attributedString(for: source)
        let plain = String(result.characters)

        guard let firstRange = plain.range(of: "alpha") else { return XCTFail("'alpha' not found") }
        let firstOffset = plain.distance(from: plain.startIndex, to: firstRange.lowerBound)
        XCTAssertFalse(
            isHighlighted(at: firstOffset, in: result),
            "First 'alpha' must NOT be highlighted when only occurrenceIndex=1 is anchored"
        )

        guard let secondRange = plain.range(of: "alpha", range: firstRange.upperBound..<plain.endIndex) else {
            return XCTFail("Second 'alpha' not found")
        }
        let secondOffset = plain.distance(from: plain.startIndex, to: secondRange.lowerBound)
        XCTAssertTrue(
            isHighlighted(at: secondOffset, in: result),
            "Second 'alpha' (occurrenceIndex=1) must be highlighted"
        )
    }

    /// Two anchors on two different occurrences of the same word each highlight their
    /// respective instance (both are highlighted, but through independent anchors).
    func testHighlightingParserTwoAnchorsHighlightBothOccurrences() throws {
        let source = "alpha beta alpha gamma"
        let parser = HighlightingMarkdownParser(highlightedAnchors: [
            CommentAnchor(quotedText: "alpha", occurrenceIndex: 0),
            CommentAnchor(quotedText: "alpha", occurrenceIndex: 1),
        ])
        let result = try parser.attributedString(for: source)
        let plain = String(result.characters)

        guard let firstRange = plain.range(of: "alpha") else { return XCTFail("'alpha' not found") }
        let firstOffset = plain.distance(from: plain.startIndex, to: firstRange.lowerBound)
        XCTAssertTrue(isHighlighted(at: firstOffset, in: result), "First 'alpha' must be highlighted")

        guard let secondRange = plain.range(of: "alpha", range: firstRange.upperBound..<plain.endIndex) else {
            return XCTFail("Second 'alpha' not found")
        }
        let secondOffset = plain.distance(from: plain.startIndex, to: secondRange.lowerBound)
        XCTAssertTrue(isHighlighted(at: secondOffset, in: result), "Second 'alpha' must be highlighted")
    }

    /// Returns true if the run containing the character at `charOffset` in `plain`
    /// carries an underline attribute (the clobber-proof marker for inline-code spans).
    private func isUnderlined(at charOffset: Int, in result: AttributedString) -> Bool {
        let idx = result.characters.index(result.characters.startIndex, offsetBy: charOffset)
        return result.runs.contains { run in
            run.range.contains(idx) && run.swiftUI.underlineStyle != nil
        }
    }

    /// A multi-line selection: the quoted text the user copied uses a space where the
    /// rendered projection has a paragraph-internal soft break (or vice-versa). Exact
    /// `range(of:)` would fail; whitespace-tolerant matching must still resolve it.
    /// Regression for "comment anchors invisible" — the long quoted sentence never
    /// highlighted because its whitespace didn't match the rendered text byte-for-byte.
    func testHighlightingParserMatchesAcrossWhitespaceDifferences() throws {
        // Source wraps the sentence across two lines (soft break -> rendered space).
        let source = "Moving a design between projects\nhas flavor-specific invariants and is scoped out."
        // Quoted text uses single spaces throughout, as a pasteboard copy typically does.
        let quoted = "Moving a design between projects has flavor-specific invariants and is scoped out."
        let parser = HighlightingMarkdownParser(
            highlightedAnchors: [CommentAnchor(quotedText: quoted, occurrenceIndex: 0)]
        )
        let result = try parser.attributedString(for: source)
        let plain = String(result.characters)

        guard let movingRange = plain.range(of: "Moving") else {
            return XCTFail("'Moving' not found in rendered plain text")
        }
        let offset = plain.distance(from: plain.startIndex, to: movingRange.lowerBound)
        XCTAssertTrue(
            isHighlighted(at: offset, in: result),
            "Whitespace-tolerant matching must resolve a multi-line quoted selection"
        )
    }

    /// `flexibleMatchRanges` finds each occurrence in document order and tolerates
    /// collapsed/expanded interior whitespace.
    func testFlexibleMatchRangesToleratesWhitespaceRuns() {
        let plain = "the   quick\nbrown fox and the quick brown cat"
        let ranges = HighlightingMarkdownParser.flexibleMatchRanges(of: "quick brown", in: plain)
        XCTAssertEqual(ranges.count, 2, "Both 'quick brown' occurrences must match across varied whitespace")
    }

    /// A single-token needle keeps exact, ordered, non-overlapping occurrence semantics.
    func testFlexibleMatchRangesSingleTokenMatchesEachOccurrence() {
        let plain = "alpha beta alpha gamma alpha"
        let ranges = HighlightingMarkdownParser.flexibleMatchRanges(of: "alpha", in: plain)
        XCTAssertEqual(ranges.count, 3)
    }

    /// A comment anchored to an inline-code span must emit the clobber-proof underline
    /// marker so it stays visible after the Boss inline style overwrites the code span's
    /// background. Regression for the invisible `` `flavor` `` anchor.
    func testHighlightingParserUnderlinesInlineCodeAnchor() throws {
        let source = "The new `flavor` column replaces the old type."
        let parser = HighlightingMarkdownParser(
            highlightedAnchors: [CommentAnchor(quotedText: "flavor", occurrenceIndex: 0)]
        )
        let result = try parser.attributedString(for: source)
        let plain = String(result.characters)

        guard let range = plain.range(of: "flavor") else {
            return XCTFail("'flavor' not found in rendered plain text")
        }
        let offset = plain.distance(from: plain.startIndex, to: range.lowerBound)
        XCTAssertTrue(
            isHighlighted(at: offset, in: result),
            "Inline-code anchor must carry a backgroundColor at parse time"
        )
        XCTAssertTrue(
            isUnderlined(at: offset, in: result),
            "Inline-code anchor must also carry the clobber-proof underline marker"
        )
    }

    /// An anchor whose occurrenceIndex exceeds the number of matches applies no highlight
    /// (silent no-op; safer than highlighting the wrong span after a doc edit).
    func testHighlightingParserOutOfRangeOccurrenceIndexIsNoOp() throws {
        let source = "alpha beta gamma"
        let parser = HighlightingMarkdownParser(
            highlightedAnchors: [CommentAnchor(quotedText: "alpha", occurrenceIndex: 5)]
        )
        let result = try parser.attributedString(for: source)
        let plain = String(result.characters)

        guard let alphaRange = plain.range(of: "alpha") else { return XCTFail("'alpha' not found") }
        let alphaOffset = plain.distance(from: plain.startIndex, to: alphaRange.lowerBound)
        XCTAssertFalse(
            isHighlighted(at: alphaOffset, in: result),
            "Out-of-range occurrenceIndex must not highlight anything"
        )
    }

    // MARK: - View: highlight overlay placeholder compiles and renders

    func testCommentHighlightOverlayRendersWithComments() {
        let layer = CommentLayer()
        layer.addComment(quoted: "some text", body: "a note")
        let view = CommentHighlightOverlay(comments: layer.comments)
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 400, height: 400)
        hosting.layoutSubtreeIfNeeded()
        // Phase 1 stub renders EmptyView — size may be zero; just assert it
        // doesn't crash.
        XCTAssertGreaterThanOrEqual(hosting.fittingSize.height, 0)
    }
}
