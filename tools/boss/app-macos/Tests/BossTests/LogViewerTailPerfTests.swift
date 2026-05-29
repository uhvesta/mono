import XCTest
import os
@testable import Boss

/// Regression / perf guard for the Activity window "Logs" tab
/// hard-beachball: opening the tab once the engine-trace JSONL had grown
/// over a long session used to read and parse the **entire** file on the
/// main thread, so cost was O(N) in total log length and got worse the
/// longer Boss ran. The tailer now bounds its initial read to a trailing
/// byte window, so the open cost is constant regardless of file size.
///
/// These tests point a `JsonlLineTailer` at a large synthetic file and
/// assert the initial delivery is window-bounded (not the whole file) and
/// arrives promptly — i.e. switching to the Logs tab stays within a small
/// budget no matter how much log has accumulated.
final class LogViewerTailPerfTests: XCTestCase {

    private func makeTempDir() throws -> URL {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("boss-tail-test-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    func testInitialReadOfHugeFileIsWindowBounded() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let file = dir.appendingPathComponent("engine-trace.jsonl")

        // ~20k lines × ~90 bytes ≈ 1.8 MB, far over the 64 KiB test window.
        let totalLines = 20_000
        var blob = ""
        blob.reserveCapacity(totalLines * 90)
        for i in 0..<totalLines {
            blob += "{\"timestamp\":\"2026-05-29T00:00:00.000Z\",\"level\":\"INFO\",\"fields\":{\"message\":\"line \(i)\"}}\n"
        }
        try blob.write(to: file, atomically: true, encoding: .utf8)

        let window: UInt64 = 64 * 1024
        let received = OSAllocatedUnfairLock(initialState: [String]())
        let exp = expectation(description: "initial tail delivered")
        exp.assertForOverFulfill = false

        let tailer = JsonlLineTailer(
            url: file,
            label: "test.engine-trace.huge",
            initialTailBytes: window,
            onLines: { lines in
                received.withLock { $0.append(contentsOf: lines) }
                exp.fulfill()
            },
            onEmpty: {}
        )
        tailer.start()
        wait(for: [exp], timeout: 5)
        tailer.stop()

        let count = received.withLock { $0.count }
        XCTAssertGreaterThan(count, 0, "should deliver the trailing window")
        // 64 KiB / ~90 B ≈ 730 lines — must be nowhere near the full 20k.
        XCTAssertLessThan(
            count, 4_000,
            "initial read should be window-bounded, got \(count) of \(totalLines)"
        )
    }

    func testInitialReadOfSmallFileDeliversEverything() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let file = dir.appendingPathComponent("engine-trace.jsonl")
        try "a\nb\nc\n".write(to: file, atomically: true, encoding: .utf8)

        let received = OSAllocatedUnfairLock(initialState: [String]())
        let exp = expectation(description: "all lines delivered")
        exp.assertForOverFulfill = false

        let tailer = JsonlLineTailer(
            url: file,
            label: "test.engine-trace.small",
            initialTailBytes: 64 * 1024,
            onLines: { lines in
                received.withLock { $0.append(contentsOf: lines) }
                exp.fulfill()
            },
            onEmpty: {}
        )
        tailer.start()
        wait(for: [exp], timeout: 5)
        tailer.stop()

        // A file under the window is read whole, with no partial-line drop.
        XCTAssertEqual(received.withLock { $0 }, ["a", "b", "c"])
    }
}
