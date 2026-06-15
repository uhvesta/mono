import XCTest
@testable import Boss

/// Covers the pure, deterministic pieces of the terminal event-loop
/// diagnostics added to verify/refute the high-CPU busy-spin hypothesis:
/// the counter→rate arithmetic, the verify/refute warning logic, the
/// record JSON contracts (snake_case + roundtrip), the bounded ring +
/// since-filter, and the text dump. The live 1 Hz sampler, the libghostty
/// surface probes, and the tab-switch timing run in the app, not here —
/// these tests pin the logic that decides *what* is recorded and *how* it
/// reads.
final class TerminalLoopDiagnosticsTests: XCTestCase {

    // MARK: - Helpers

    private func pane(
        id: String = "run-abc123",
        role: String = "worker",
        slotId: Int? = 2,
        runId: String? = "abc123",
        title: String = "Riker",
        pid: Int32 = 555,
        pidAlive: Bool = true,
        processExited: Bool = false,
        contentChanged: Bool? = nil
    ) -> PaneLoopSample {
        PaneLoopSample(
            paneId: id,
            role: role,
            slotId: slotId,
            runId: runId,
            title: title,
            foregroundPid: pid,
            pidAlive: pidAlive,
            processExited: processExited,
            contentChanged: contentChanged
        )
    }

    private func loop(
        ts: Int64 = 1_700_000_000_000,
        wakeups: Double = 0,
        ticks: Double = 0,
        panes: [PaneLoopSample] = []
    ) -> LoopSample {
        LoopSample(
            tsEpochMs: ts,
            wakeupsPerSec: wakeups,
            ticksPerSec: ticks,
            intervalMs: 1000,
            panes: panes
        )
    }

    // MARK: - TerminalLoopRate

    func testRatePerSecondOverOneSecond() {
        XCTAssertEqual(
            TerminalLoopRate.perSecond(delta: 2_000, elapsedNanos: 1_000_000_000),
            2_000, accuracy: 0.0001
        )
    }

    func testRatePerSecondOverTwoSeconds() {
        XCTAssertEqual(
            TerminalLoopRate.perSecond(delta: 100, elapsedNanos: 2_000_000_000),
            50, accuracy: 0.0001
        )
    }

    func testRatePerSecondZeroIntervalIsZeroNotInfinity() {
        XCTAssertEqual(TerminalLoopRate.perSecond(delta: 999, elapsedNanos: 0), 0)
    }

    // MARK: - Verify/refute warning logic

    func testHotLoopWithDeadPaneFlagsSpin() {
        let s = loop(wakeups: 5_000, panes: [pane(processExited: true)])
        let warnings = TerminalLoopThresholds.warnings(for: s)
        XCTAssertEqual(warnings.count, 1)
        XCTAssertTrue(warnings[0].message.contains("dead-fd busy-spin SUSPECTED"))
        // Names the pane/slot/worker so the operator can correlate.
        XCTAssertTrue(warnings[0].message.contains("Riker"))
        XCTAssertTrue(warnings[0].message.contains("slot 2"))
        XCTAssertTrue(warnings[0].message.contains("run abc123"))
    }

    func testHotLoopWithDeadPidFlagsSpin() {
        // Non-zero pid that no longer exists is also a spin suspect.
        let s = loop(wakeups: 5_000, panes: [pane(pidAlive: false)])
        let warnings = TerminalLoopThresholds.warnings(for: s)
        XCTAssertEqual(warnings.count, 1)
        XCTAssertTrue(warnings[0].message.contains("dead-fd busy-spin SUSPECTED"))
        XCTAssertTrue(warnings[0].message.contains("dead-pid"))
    }

    func testHotLoopWithAllPanesLiveIsFloodNotSpin() {
        let s = loop(wakeups: 5_000, panes: [pane(), pane(id: "run-def", title: "Worf")])
        let warnings = TerminalLoopThresholds.warnings(for: s)
        XCTAssertEqual(warnings.count, 1)
        XCTAssertTrue(warnings[0].message.contains("genuine output flood"))
        XCTAssertFalse(warnings[0].message.contains("SUSPECTED"))
    }

    func testQuietLoopWithDeadPaneStillWarns() {
        let s = loop(wakeups: 100, panes: [pane(processExited: true)])
        let warnings = TerminalLoopThresholds.warnings(for: s)
        XCTAssertEqual(warnings.count, 1)
        XCTAssertTrue(warnings[0].message.contains("watch for spin"))
    }

    func testQuietLoopAllLiveProducesNoWarnings() {
        let s = loop(wakeups: 100, panes: [pane()])
        XCTAssertTrue(TerminalLoopThresholds.warnings(for: s).isEmpty)
    }

    func testCustomThresholdRespected() {
        let s = loop(wakeups: 600, panes: [pane()])
        // Below the custom 500/s threshold? No — 600 >= 500, all live → flood.
        let warnings = TerminalLoopThresholds.warnings(for: s, wakeupWarnPerSec: 500)
        XCTAssertEqual(warnings.count, 1)
        XCTAssertTrue(warnings[0].message.contains("genuine output flood"))
    }

    // MARK: - JSON contracts

    func testPaneLoopSampleJSONUsesSnakeCaseKeys() throws {
        let data = try JSONEncoder().encode(pane(processExited: true, contentChanged: false))
        let json = try XCTUnwrap(String(data: data, encoding: .utf8))
        XCTAssertTrue(json.contains("\"pane_id\""))
        XCTAssertTrue(json.contains("\"slot_id\""))
        XCTAssertTrue(json.contains("\"run_id\""))
        XCTAssertTrue(json.contains("\"foreground_pid\""))
        XCTAssertTrue(json.contains("\"pid_alive\""))
        XCTAssertTrue(json.contains("\"process_exited\""))
        XCTAssertTrue(json.contains("\"content_changed\""))

        let decoded = try JSONDecoder().decode(PaneLoopSample.self, from: data)
        XCTAssertEqual(decoded, pane(processExited: true, contentChanged: false))
    }

    func testLoopSampleJSONHasKindAndSnakeCase() throws {
        let data = try JSONEncoder().encode(loop(wakeups: 12, ticks: 3, panes: [pane()]))
        let json = try XCTUnwrap(String(data: data, encoding: .utf8))
        XCTAssertTrue(json.contains("\"kind\":\"loop\""))
        XCTAssertTrue(json.contains("\"wakeups_per_sec\""))
        XCTAssertTrue(json.contains("\"ticks_per_sec\""))
        XCTAssertTrue(json.contains("\"ts_epoch_ms\""))

        let decoded = try JSONDecoder().decode(LoopSample.self, from: data)
        XCTAssertEqual(decoded.wakeupsPerSec, 12, accuracy: 0.0001)
        XCTAssertEqual(decoded.kind, "loop")
    }

    func testTabSwitchSampleJSONHasKindAndSnakeCase() throws {
        let t = TabSwitchSample(
            tsEpochMs: 1_700_000_000_000,
            from: "Work",
            to: "Agents",
            panesRelaidOut: 3,
            panesTornDown: 0,
            settleMs: 42
        )
        let data = try JSONEncoder().encode(t)
        let json = try XCTUnwrap(String(data: data, encoding: .utf8))
        XCTAssertTrue(json.contains("\"kind\":\"tab_switch\""))
        XCTAssertTrue(json.contains("\"panes_relaid_out\""))
        XCTAssertTrue(json.contains("\"panes_torn_down\""))
        XCTAssertTrue(json.contains("\"settle_ms\""))

        let decoded = try JSONDecoder().decode(TabSwitchSample.self, from: data)
        XCTAssertEqual(decoded, t)
    }

    // MARK: - Ring buffer + since-filter (in-memory, no disk)

    func testLoopRingEvictsOldestBeyondCapacity() {
        let log = TerminalLoopLog(directory: nil, capacity: 3)
        for i in 0..<5 {
            log.record(loop(ts: Int64(1_000 + i), wakeups: Double(i)))
        }
        let snap = log.loopSnapshot()
        XCTAssertEqual(snap.count, 3)
        // Newest retained, oldest evicted.
        XCTAssertEqual(snap.first?.wakeupsPerSec, 2)
        XCTAssertEqual(snap.last?.wakeupsPerSec, 4)
    }

    func testRecentLoopsFiltersBySince() {
        let log = TerminalLoopLog(directory: nil)
        let now = Date()
        let oldMs = Int64((now.addingTimeInterval(-600).timeIntervalSince1970) * 1000)
        let newMs = Int64((now.addingTimeInterval(-10).timeIntervalSince1970) * 1000)
        log.record(loop(ts: oldMs, wakeups: 1))
        log.record(loop(ts: newMs, wakeups: 2))

        let recent = log.recentLoops(since: now.addingTimeInterval(-60))
        XCTAssertEqual(recent.count, 1)
        XCTAssertEqual(recent.first?.wakeupsPerSec, 2)
    }

    func testTabSwitchRingAndSnapshotIndependentOfLoops() {
        let log = TerminalLoopLog(directory: nil)
        log.record(loop(wakeups: 7))
        log.record(TabSwitchSample(
            tsEpochMs: 1, from: "Agents", to: "Work",
            panesRelaidOut: 0, panesTornDown: 0, settleMs: 0
        ))
        XCTAssertEqual(log.loopSnapshot().count, 1)
        XCTAssertEqual(log.tabSnapshot().count, 1)
        XCTAssertEqual(log.tabSnapshot().first?.from, "Agents")
    }

    // MARK: - Text dump

    func testFormattedDumpIncludesSectionsAndPaneNames() {
        let loops = [loop(wakeups: 5_000, panes: [pane(processExited: true)])]
        let tabs = [TabSwitchSample(
            tsEpochMs: 1_700_000_000_000, from: "Work", to: "Agents",
            panesRelaidOut: 3, panesTornDown: 0, settleMs: 42
        )]
        let dump = TerminalLoopLog.formattedDump(loops: loops, tabSwitches: tabs)
        XCTAssertTrue(dump.contains("Tab switches"))
        XCTAssertTrue(dump.contains("Loop samples"))
        XCTAssertTrue(dump.contains("Work → Agents"))
        XCTAssertTrue(dump.contains("Riker"))
        XCTAssertTrue(dump.contains("EOF"))
    }

    func testFormattedDumpEmptyIsGraceful() {
        let dump = TerminalLoopLog.formattedDump(loops: [], tabSwitches: [])
        XCTAssertTrue(dump.contains("loop samples: 0"))
        XCTAssertTrue(dump.contains("(none recorded)"))
    }

    // MARK: - pidIsAlive

    func testPidIsAliveForCurrentProcess() {
        XCTAssertTrue(pidIsAlive(getpid()))
    }

    func testPidIsAliveFalseForZeroAndNegative() {
        XCTAssertFalse(pidIsAlive(0))
        XCTAssertFalse(pidIsAlive(-1))
    }
}
