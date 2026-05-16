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
        let a = Comment(id: id, quotedText: "hello", body: "world", createdAt: date)
        let b = Comment(id: id, quotedText: "hello", body: "world", createdAt: date)
        XCTAssertEqual(a, b)
    }

    func testCommentModelIdentityDiffersForDifferentIDs() {
        let date = Date()
        let a = Comment(id: UUID(), quotedText: "x", body: "y", createdAt: date)
        let b = Comment(id: UUID(), quotedText: "x", body: "y", createdAt: date)
        XCTAssertNotEqual(a, b)
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
