import AppKit
import SwiftUI
import XCTest
@testable import Boss

final class DesignRepoPathTests: XCTestCase {
    func testBasenameFromSshURL() {
        XCTAssertEqual(DesignRepoPath.basename(from: "git@github.com:spinyfin/mono.git"), "mono")
    }

    func testBasenameFromHttpsURL() {
        XCTAssertEqual(DesignRepoPath.basename(from: "https://github.com/spinyfin/mono.git"), "mono")
    }

    func testBasenameWithoutSuffix() {
        XCTAssertEqual(DesignRepoPath.basename(from: "https://github.com/spinyfin/mono"), "mono")
    }

    func testBasenameEmpty() {
        XCTAssertNil(DesignRepoPath.basename(from: ""))
        XCTAssertNil(DesignRepoPath.basename(from: "   "))
    }

    func testLocalCloneURLEndsAtBasename() {
        let url = DesignRepoPath.localCloneURL(for: "git@github.com:foo/widgets.git")
        XCTAssertEqual(url?.lastPathComponent, "widgets")
        XCTAssertTrue(url?.path.hasSuffix("Documents/dev/widgets") ?? false)
    }
}

final class DesignFileLoaderTests: XCTestCase {
    func testSkipsNoiseDirectories() {
        XCTAssertTrue(DesignFileLoader.shouldSkip(directoryName: "node_modules"))
        XCTAssertTrue(DesignFileLoader.shouldSkip(directoryName: "target"))
        XCTAssertTrue(DesignFileLoader.shouldSkip(directoryName: ".git"))
        XCTAssertTrue(DesignFileLoader.shouldSkip(directoryName: "build"))
        XCTAssertTrue(DesignFileLoader.shouldSkip(directoryName: "dist"))
        XCTAssertTrue(DesignFileLoader.shouldSkip(directoryName: ".next"))
        XCTAssertTrue(DesignFileLoader.shouldSkip(directoryName: "bazel-bin"))
        XCTAssertTrue(DesignFileLoader.shouldSkip(directoryName: "bazel-out"))
        XCTAssertTrue(DesignFileLoader.shouldSkip(directoryName: "bazel-mono"))

        XCTAssertFalse(DesignFileLoader.shouldSkip(directoryName: "src"))
        XCTAssertFalse(DesignFileLoader.shouldSkip(directoryName: "docs"))
    }

    func testEnumerationFindsMarkdownAndPrunesEmptyDirs() throws {
        let fm = FileManager.default
        let root = fm.temporaryDirectory.appendingPathComponent("designs-tests-\(UUID().uuidString)", isDirectory: true)
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: root) }

        try Data().write(to: root.appendingPathComponent("README.md"))
        let docs = root.appendingPathComponent("docs", isDirectory: true)
        try fm.createDirectory(at: docs, withIntermediateDirectories: true)
        try Data().write(to: docs.appendingPathComponent("design.md"))

        let nodeModules = root.appendingPathComponent("node_modules", isDirectory: true)
        try fm.createDirectory(at: nodeModules, withIntermediateDirectories: true)
        try Data().write(to: nodeModules.appendingPathComponent("ignored.md"))

        let bazelBin = root.appendingPathComponent("bazel-bin", isDirectory: true)
        try fm.createDirectory(at: bazelBin, withIntermediateDirectories: true)
        try Data().write(to: bazelBin.appendingPathComponent("ignored.md"))

        let emptyDocs = root.appendingPathComponent("empty", isDirectory: true)
        try fm.createDirectory(at: emptyDocs, withIntermediateDirectories: true)
        try Data().write(to: emptyDocs.appendingPathComponent("readme.txt"))

        let tree = DesignFileLoader.loadTree(at: root)
        // Directories sort first, then files; both sorted by localized
        // standard compare. The noisy `node_modules`, `bazel-bin`, and
        // empty `empty/` directories are pruned.
        XCTAssertEqual(tree.map(\.name), ["docs", "README.md"])

        guard let docsNode = tree.first(where: { $0.name == "docs" }) else {
            return XCTFail("docs dir missing")
        }
        XCTAssertEqual(docsNode.children?.map(\.name), ["design.md"])
    }
}

final class MarkdownParserTests: XCTestCase {
    func testHeadings() {
        let blocks = MarkdownParser.parse("# Title\n\n## Sub\n\nbody")
        XCTAssertEqual(blocks.count, 3)
        if case .heading(let level, let text) = blocks[0] {
            XCTAssertEqual(level, 1)
            XCTAssertEqual(text, "Title")
        } else { XCTFail("expected heading") }
        if case .heading(let level, _) = blocks[1] {
            XCTAssertEqual(level, 2)
        } else { XCTFail("expected heading") }
        if case .paragraph(let text) = blocks[2] {
            XCTAssertEqual(text, "body")
        } else { XCTFail("expected paragraph") }
    }

    func testFencedCodeBlock() {
        let source = """
        Intro.

        ```swift
        let x = 1
        ```

        Outro.
        """
        let blocks = MarkdownParser.parse(source)
        XCTAssertEqual(blocks.count, 3)
        guard case .codeBlock(let lang, let lines) = blocks[1] else {
            return XCTFail("expected code block at index 1, got \(blocks[1])")
        }
        XCTAssertEqual(lang, "swift")
        XCTAssertEqual(lines, ["let x = 1"])
    }

    func testUnorderedAndOrderedLists() {
        let source = """
        - one
        - two

        1. first
        2. second
        """
        let blocks = MarkdownParser.parse(source)
        XCTAssertEqual(blocks.count, 2)
        if case .unorderedList(let items) = blocks[0] {
            XCTAssertEqual(items, ["one", "two"])
        } else { XCTFail("expected unordered list") }
        if case .orderedList(let items) = blocks[1] {
            XCTAssertEqual(items, ["first", "second"])
        } else { XCTFail("expected ordered list") }
    }

    func testBlockquote() {
        let blocks = MarkdownParser.parse("> quoted\n> still quoted")
        XCTAssertEqual(blocks.count, 1)
        if case .blockquote(let text) = blocks[0] {
            XCTAssertEqual(text, "quoted still quoted")
        } else { XCTFail("expected blockquote") }
    }

    func testHorizontalRule() {
        let blocks = MarkdownParser.parse("before\n\n---\n\nafter")
        XCTAssertEqual(blocks.count, 3)
        if case .rule = blocks[1] {
            // ok
        } else { XCTFail("expected rule") }
    }

    func testTable() {
        let source = """
        | col1 | col2 |
        |------|------|
        | a    | b    |
        | c    | d    |
        """
        let blocks = MarkdownParser.parse(source)
        XCTAssertEqual(blocks.count, 1)
        guard case .table(let header, let rows) = blocks[0] else {
            return XCTFail("expected table")
        }
        XCTAssertEqual(header, ["col1", "col2"])
        XCTAssertEqual(rows, [["a", "b"], ["c", "d"]])
    }

    func testParagraphsWithPipesAreNotTables() {
        let source = "this | has | pipes but is not a table"
        let blocks = MarkdownParser.parse(source)
        XCTAssertEqual(blocks.count, 1)
        if case .paragraph(let text) = blocks[0] {
            XCTAssertEqual(text, source)
        } else { XCTFail("expected paragraph, got \(blocks[0])") }
    }
}

/// `MarkdownViewerView` is the SwiftUI root of the "Read full description"
/// window. Step 2 of the markdown-renderer migration swapped it from the
/// hand-rolled `MarkdownBodyView` to Textual's `StructuredText`; this test
/// is the canary that the view still builds and lays out against a
/// representative description (paragraphs, fenced code, a table, a nested
/// list) so a Textual upgrade that breaks the style protocol fails here
/// rather than silently at runtime when a user clicks the affordance.
@MainActor
final class MarkdownViewerViewTests: XCTestCase {
    func testRendersRepresentativeDescription() {
        let source = """
        # Task title

        Some intro paragraph with **bold**, *italic*, `inline code`, and a
        [link](https://example.com).

        ```swift
        struct Greeter {
            let name: String
        }
        ```

        | Column A | Column B |
        | -------- | -------- |
        | one      | two      |

        - top level
          - nested one
          - nested two
        - another top
        """

        let view = MarkdownViewerView(title: "Read full description", source: source)
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 760, height: 640)
        hosting.layoutSubtreeIfNeeded()

        XCTAssertGreaterThan(hosting.fittingSize.height, 0)
        XCTAssertGreaterThan(hosting.fittingSize.width, 0)
    }

    func testRendersEmptySourceWithoutCrashing() {
        let view = MarkdownViewerView(title: "Empty", source: "")
        let hosting = NSHostingView(rootView: view)
        hosting.frame = NSRect(x: 0, y: 0, width: 760, height: 640)
        hosting.layoutSubtreeIfNeeded()
        XCTAssertGreaterThanOrEqual(hosting.fittingSize.height, 0)
    }
}
