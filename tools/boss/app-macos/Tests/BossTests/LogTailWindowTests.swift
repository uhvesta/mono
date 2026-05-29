import XCTest
@testable import Boss

/// Pure-logic guard for the initial-read tail window that keeps opening
/// the Activity window's "Logs" tab constant-time regardless of how large
/// the engine-trace / dispatch JSONL has grown (the hard-beachball fix).
final class LogTailWindowTests: XCTestCase {

    func testInitialReadOffsetReadsWholeFileWhenItFits() {
        XCTAssertEqual(LogTailWindow.initialReadOffset(fileSize: 100, cap: 512), 0)
        // Exactly at the cap still reads the whole file (offset 0).
        XCTAssertEqual(LogTailWindow.initialReadOffset(fileSize: 512, cap: 512), 0)
    }

    func testInitialReadOffsetTailsLargeFile() {
        // 2000-byte file, 512-byte window → start 1488 bytes in.
        XCTAssertEqual(LogTailWindow.initialReadOffset(fileSize: 2000, cap: 512), 1488)
    }

    func testDropPartialFirstLineRemovesLeadingFragment() {
        // The window opened mid-line; the leading fragment must be dropped.
        let chunk = "rtial-line}\n{\"a\":1}\n{\"b\":2}"
        XCTAssertEqual(LogTailWindow.dropPartialFirstLine(chunk), "{\"a\":1}\n{\"b\":2}")
    }

    func testDropPartialFirstLineEmptyWhenNoNewline() {
        // A single line larger than the window: nothing parseable, drop all.
        XCTAssertEqual(LogTailWindow.dropPartialFirstLine("no-newline-fragment"), "")
    }
}
