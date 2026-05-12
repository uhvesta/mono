import XCTest

/// Regression guard for the
/// "tests pop the user's browser during `swift test`" class of bug.
///
/// The original incident: `ProjectDesignDocAffordanceTests` called
/// `model.openProjectDesignDoc(project)` while `ChatViewModel`'s only
/// open path was a direct `NSWorkspace.shared.open(url)`. Every test
/// run handed `https://github.com/foo/bar/blob/main/.../x.md` to the
/// OS, which the browser dutifully opened. The fix is the injected
/// `ChatViewModel.urlOpener` — but the fix only sticks if future tests
/// don't reach around it.
///
/// This scan reads every `*.swift` under the `BossTests` directory and
/// asserts the file is not driving a real OS opener directly. The list
/// of banned symbols is reconstructed at runtime so the guard file
/// itself can describe what it bans without tripping its own check.
final class TestSourcesDoNotCallRealOpenerTests: XCTestCase {
    func testNoTestFileInvokesARealOpener() throws {
        let selfPath = URL(fileURLWithPath: #filePath)
        let testsDir = selfPath.deletingLastPathComponent()

        // Reassembled at runtime so this file's source can mention the
        // symbols without the scanner flagging itself.
        let banned: [String] = [
            ["NS", "Workspace.shared.open"].joined(),
            // SwiftUI `\.openURL` environment + `OpenURLAction` direct
            // invocation. Allowed inside the production `Sources/` tree
            // where the real opener has to live; banned in tests.
            ["Open", "URLAction"].joined(),
        ]

        let fileManager = FileManager.default
        guard let enumerator = fileManager.enumerator(
            at: testsDir,
            includingPropertiesForKeys: [.isRegularFileKey],
            options: [.skipsHiddenFiles]
        ) else {
            XCTFail("could not enumerate \(testsDir.path)")
            return
        }

        var offenders: [String] = []
        for case let fileURL as URL in enumerator {
            guard fileURL.pathExtension == "swift" else { continue }
            // Skip this guard file — it names the banned symbols on
            // purpose, in `banned` above and in this doc comment.
            if fileURL.lastPathComponent == selfPath.lastPathComponent {
                continue
            }
            let contents = try String(contentsOf: fileURL, encoding: .utf8)
            for (lineIndex, rawLine) in contents.split(
                separator: "\n",
                omittingEmptySubsequences: false
            ).enumerated() {
                let line = String(rawLine)
                let trimmed = line.trimmingCharacters(in: .whitespaces)
                // Comments and doc-comments are fine — naming the
                // symbol in prose doesn't pop the browser.
                if trimmed.hasPrefix("//") || trimmed.hasPrefix("///") || trimmed.hasPrefix("*") {
                    continue
                }
                for symbol in banned where line.contains(symbol) {
                    offenders.append(
                        "\(fileURL.lastPathComponent):\(lineIndex + 1) uses banned symbol `\(symbol)` — route through `ChatViewModel.urlOpener` (production) and a recording stub (tests)."
                    )
                }
            }
        }

        XCTAssertEqual(
            offenders, [],
            "Tests must not invoke the real OS URL opener:\n\(offenders.joined(separator: "\n"))"
        )
    }
}
