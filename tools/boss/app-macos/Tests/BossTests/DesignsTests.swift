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

/// `MarkdownViewerView` is the SwiftUI root of the "Read full description"
/// window, a thin wrapper around Textual's `StructuredText`. This test is
/// the canary that the view still builds and lays out against a
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
