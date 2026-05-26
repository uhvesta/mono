import XCTest
@testable import Boss

/// Pins the elapsed-time formatting that feeds the
/// `WorkerWaitingIndicator` tooltip ("No response for 9m"). The
/// indicator replaces the old accent-blue-only treatment of a worker
/// parked in `WaitingForInput`; the duration is derived in the UI
/// from the hook-driven `last_event_at` timestamp, so the parse +
/// format contract is the part worth nailing down without hosting a
/// SwiftUI view.
final class WorkerStalenessTests: XCTestCase {

    func testFormatSecondsUnderAMinute() {
        XCTAssertEqual(WorkerStaleness.format(seconds: 0), "0s")
        XCTAssertEqual(WorkerStaleness.format(seconds: 45), "45s")
        XCTAssertEqual(WorkerStaleness.format(seconds: 59), "59s")
    }

    func testFormatWholeMinutes() {
        XCTAssertEqual(WorkerStaleness.format(seconds: 60), "1m")
        XCTAssertEqual(WorkerStaleness.format(seconds: 9 * 60), "9m")
        XCTAssertEqual(WorkerStaleness.format(seconds: 59 * 60 + 59), "59m")
    }

    func testFormatHours() {
        XCTAssertEqual(WorkerStaleness.format(seconds: 60 * 60), "1h")
        XCTAssertEqual(WorkerStaleness.format(seconds: 2 * 60 * 60 + 5 * 60), "2h 5m")
        // Trailing minutes are dropped only when exactly zero.
        XCTAssertEqual(WorkerStaleness.format(seconds: 3 * 60 * 60), "3h")
    }

    func testElapsedShortFromEngineTimestamp() {
        // Engine stamps plain `YYYY-MM-DDTHH:MM:SSZ` (no fractional s).
        let base = WorkerStaleness.parse("2026-05-26T12:00:00Z")
        XCTAssertNotNil(base)
        let now = base!.addingTimeInterval(9 * 60)
        XCTAssertEqual(
            WorkerStaleness.elapsedShort(since: "2026-05-26T12:00:00Z", now: now),
            "9m"
        )
    }

    func testElapsedShortClampsFutureTimestampToZero() {
        let now = WorkerStaleness.parse("2026-05-26T12:00:00Z")!
        // last_event_at slightly in the future (clock skew) must not
        // render a negative/garbage duration.
        XCTAssertEqual(
            WorkerStaleness.elapsedShort(since: "2026-05-26T12:00:30Z", now: now),
            "0s"
        )
    }

    func testElapsedShortReturnsNilForMissingOrUnparseable() {
        let now = Date()
        XCTAssertNil(WorkerStaleness.elapsedShort(since: nil, now: now))
        XCTAssertNil(WorkerStaleness.elapsedShort(since: "not-a-timestamp", now: now))
        XCTAssertNil(WorkerStaleness.elapsedShort(since: "", now: now))
    }

    func testParseAcceptsFractionalSeconds() {
        XCTAssertNotNil(WorkerStaleness.parse("2026-05-26T12:00:00.123Z"))
    }
}
