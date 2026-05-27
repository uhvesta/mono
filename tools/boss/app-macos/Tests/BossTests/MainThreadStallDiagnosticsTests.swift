import XCTest
@testable import Boss

/// Covers the pure, deterministic pieces of the main-thread stall
/// diagnostics added for the Ghostty pane-sluggishness shake: the
/// detection threshold, the stall record/log contract (ring buffer +
/// since-filter + JSONL shape + text dump), the backtrace frame
/// formatting, and the dropped-frame tally. The live watchdog timers
/// and the Mach frame-pointer walk are exercised in the running app,
/// not here — these tests pin the logic that decides *what* gets
/// recorded and *how* it's rendered.
final class MainThreadStallDiagnosticsTests: XCTestCase {

    // MARK: - StallDetector

    func testStallDetectorReturnsNilBelowThreshold() {
        // 100 ms elapsed, 250 ms threshold → not a stall.
        let last: UInt64 = 1_000_000_000
        let now = last + 100_000_000
        XCTAssertNil(StallDetector.stallDurationMs(
            lastHeartbeatNanos: last, nowNanos: now, thresholdMs: 250
        ))
    }

    func testStallDetectorReportsElapsedAboveThreshold() {
        // 300 ms elapsed, 250 ms threshold → a 300 ms stall.
        let last: UInt64 = 1_000_000_000
        let now = last + 300_000_000
        let dur = StallDetector.stallDurationMs(
            lastHeartbeatNanos: last, nowNanos: now, thresholdMs: 250
        )
        XCTAssertEqual(dur ?? 0, 300, accuracy: 0.001)
    }

    func testStallDetectorBoundaryIsStrictlyGreater() {
        // Exactly at threshold is not yet a stall.
        let last: UInt64 = 0
        let now: UInt64 = 250_000_000
        XCTAssertNil(StallDetector.stallDurationMs(
            lastHeartbeatNanos: last, nowNanos: now, thresholdMs: 250
        ))
    }

    func testStallDetectorHandlesNonMonotonicClock() {
        // now < last (shouldn't happen with a monotonic clock, but must
        // not underflow into a giant positive duration).
        XCTAssertNil(StallDetector.stallDurationMs(
            lastHeartbeatNanos: 500, nowNanos: 100, thresholdMs: 250
        ))
    }

    // MARK: - StallRecord JSON contract

    func testStallRecordJSONUsesSnakeCaseKeys() throws {
        let rec = StallRecord(
            id: UUID(uuidString: "00000000-0000-0000-0000-000000000001")!,
            tsEpochMs: 1_700_000_000_000,
            durationMs: 312.5,
            heartbeatIntervalMs: 100,
            thresholdMs: 250,
            context: "Picard",
            backtrace: ["0   Boss  0x0000 frame + 0"]
        )
        let data = try JSONEncoder().encode(rec)
        let json = try XCTUnwrap(String(data: data, encoding: .utf8))
        XCTAssertTrue(json.contains("\"ts_epoch_ms\""))
        XCTAssertTrue(json.contains("\"duration_ms\""))
        XCTAssertTrue(json.contains("\"heartbeat_interval_ms\""))
        XCTAssertTrue(json.contains("\"threshold_ms\""))

        let decoded = try JSONDecoder().decode(StallRecord.self, from: data)
        XCTAssertEqual(decoded, rec)
    }

    // MARK: - StallLog ring buffer

    func testStallLogRingBufferEvictsOldest() {
        let log = StallLog(directory: nil, capacity: 3)
        for i in 0..<5 {
            log.record(makeRecord(tsEpochMs: Int64(i), context: "c\(i)"))
        }
        let snap = log.snapshot()
        XCTAssertEqual(snap.count, 3)
        // Oldest two (0,1) evicted; newest-last ordering preserved.
        XCTAssertEqual(snap.map(\.context), ["c2", "c3", "c4"])
    }

    func testStallLogRecentFiltersBySince() {
        let log = StallLog(directory: nil, capacity: 10)
        let nowMs = Int64(Date().timeIntervalSince1970 * 1000)
        log.record(makeRecord(tsEpochMs: nowMs - 600_000, context: "old"))   // 10m ago
        log.record(makeRecord(tsEpochMs: nowMs - 60_000, context: "recent")) // 1m ago

        let lastFiveMin = log.recent(since: Date().addingTimeInterval(-300))
        XCTAssertEqual(lastFiveMin.map(\.context), ["recent"])
    }

    func testStallLogSnapshotIsIndependentCopy() {
        let log = StallLog(directory: nil, capacity: 10)
        log.record(makeRecord(tsEpochMs: 1, context: "a"))
        let snap = log.snapshot()
        log.record(makeRecord(tsEpochMs: 2, context: "b"))
        // The earlier snapshot must not see the later append.
        XCTAssertEqual(snap.map(\.context), ["a"])
    }

    // MARK: - StallLog text dump

    func testFormattedDumpEmpty() {
        let dump = StallLog.formattedDump([])
        XCTAssertTrue(dump.contains("stalls: 0"))
        XCTAssertTrue(dump.contains("No stalls recorded"))
    }

    func testFormattedDumpRendersNewestFirstWithFrames() {
        let older = makeRecord(tsEpochMs: 1_700_000_000_000, context: "Worf", duration: 300)
        let newer = makeRecord(
            tsEpochMs: 1_700_000_005_000,
            context: "Picard",
            duration: 1200,
            backtrace: ["0   Boss  0x1 a + 0", "1   Boss  0x2 b + 4"]
        )
        let dump = StallLog.formattedDump([older, newer])
        XCTAssertTrue(dump.contains("stalls: 2"))
        // Newest (Picard) printed before older (Worf).
        let picardIdx = try! XCTUnwrap(dump.range(of: "Picard")).lowerBound
        let worfIdx = try! XCTUnwrap(dump.range(of: "Worf")).lowerBound
        XCTAssertLessThan(picardIdx, worfIdx)
        XCTAssertTrue(dump.contains("≥1200 ms"))
        XCTAssertTrue(dump.contains("a + 0"))
    }

    // MARK: - Backtrace frame formatting

    func testFormatFramePadsColumns() {
        let frame = MainThreadBacktrace.formatFrame(
            index: 7,
            image: "Boss",
            address: 0x10abc,
            symbol: "$s4Boss3fooyyF",
            offset: 24
        )
        XCTAssertTrue(frame.hasPrefix("7  "), "index padded to width 3, got: \(frame)")
        XCTAssertTrue(frame.contains("0x0000000000010abc"), "address zero-padded hex, got: \(frame)")
        XCTAssertTrue(frame.hasSuffix("$s4Boss3fooyyF + 24"))
    }

    func testFormatFrameTruncatesLongImageName() {
        let longName = String(repeating: "X", count: 50)
        let frame = MainThreadBacktrace.formatFrame(
            index: 0, image: longName, address: 0, symbol: "s", offset: 0
        )
        // Image column is fixed width (30) — long names are truncated.
        XCTAssertTrue(frame.contains(String(repeating: "X", count: 30)))
        XCTAssertFalse(frame.contains(String(repeating: "X", count: 31)))
    }

    // MARK: - Frame-drop tally

    func testFrameTallyComputesDrops() {
        // 1 second at 60 Hz = ~61 frames expected; only 30 serviced.
        let result = InteractionFrameCounter.tally(
            elapsed: 1.0, frameInterval: 1.0 / 60.0, actualFrames: 30
        )
        XCTAssertEqual(result?.expected, 61)
        XCTAssertEqual(result?.actual, 30)
        XCTAssertEqual(result?.dropped, 31)
    }

    func testFrameTallyClampsNegativeDrops() {
        // More serviced than expected (rounding / extra ticks) → 0 drops.
        let result = InteractionFrameCounter.tally(
            elapsed: 0.5, frameInterval: 1.0 / 60.0, actualFrames: 100
        )
        XCTAssertEqual(result?.dropped, 0)
    }

    func testFrameTallyRejectsDegenerateInput() {
        XCTAssertNil(InteractionFrameCounter.tally(
            elapsed: 0, frameInterval: 1.0 / 60.0, actualFrames: 0
        ))
        XCTAssertNil(InteractionFrameCounter.tally(
            elapsed: 1.0, frameInterval: 0, actualFrames: 0
        ))
    }

    // MARK: - Helpers

    private func makeRecord(
        tsEpochMs: Int64,
        context: String,
        duration: Double = 300,
        backtrace: [String] = []
    ) -> StallRecord {
        StallRecord(
            tsEpochMs: tsEpochMs,
            durationMs: duration,
            heartbeatIntervalMs: 100,
            thresholdMs: 250,
            context: context,
            backtrace: backtrace
        )
    }
}
