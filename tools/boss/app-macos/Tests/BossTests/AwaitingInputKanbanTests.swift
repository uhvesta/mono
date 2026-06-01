import XCTest
@testable import Boss

/// Covers the "awaiting input" kanban rule: when the live worker behind
/// a Doing-lane card is paused on a permission prompt
/// (`activity == .waitingForInput`), the card's activity dot must read
/// as *waiting* — visibly distinct from a normally-running (active)
/// worker — rather than the green "active" dot that a bound slot would
/// otherwise force. The state clears on its own when the worker resumes
/// and the activity flips back to `.working`.
@MainActor
final class AwaitingInputKanbanTests: XCTestCase {

    func testWaitingForInputSurfacesAsWaitingNotActive() {
        let live = makeLiveState(activity: .waitingForInput)
        let state = AgentActivityState.forDoingCard(
            runtime: makeRuntime(),
            liveState: live,
            isDispatchPending: false,
            isResolvingConflicts: false,
            isRemediatingCI: false
        )
        guard case let .waiting(reason) = state else {
            return XCTFail("expected .waiting for a waiting-for-input worker; got \(state)")
        }
        XCTAssertTrue(
            reason.lowercased().contains("input"),
            "waiting reason should mention input; got: \(reason)"
        )
    }

    func testWorkingBoundWorkerStaysActive() {
        // A normally-running worker (bound slot, activity=working) must
        // still read as active — the awaiting-input rule must not regress
        // the common case.
        let live = makeLiveState(activity: .working)
        let state = AgentActivityState.forDoingCard(
            runtime: makeRuntime(),
            liveState: live,
            isDispatchPending: false,
            isResolvingConflicts: false,
            isRemediatingCI: false
        )
        guard case .active = state else {
            return XCTFail("expected .active for a working bound worker; got \(state)")
        }
    }

    func testWaitingForInputTakesPrecedenceOverBoundSlot() {
        // The bound-slot "active" shortcut must not pre-empt the
        // waiting-for-input check: a worker can be both bound to a slot
        // and paused on a prompt at the same time.
        let live = makeLiveState(slotId: 3, activity: .waitingForInput)
        let state = AgentActivityState.forDoingCard(
            runtime: makeRuntime(),
            liveState: live,
            isDispatchPending: false,
            isResolvingConflicts: false,
            isRemediatingCI: false
        )
        guard case .waiting = state else {
            return XCTFail("waiting-for-input must win over a bound slot; got \(state)")
        }
    }

    func testDispatchPendingStillWins() {
        // Dispatch-pending is the highest-precedence rule and must hold
        // even if a stale live state reports waiting-for-input.
        let live = makeLiveState(activity: .waitingForInput)
        let state = AgentActivityState.forDoingCard(
            runtime: makeRuntime(),
            liveState: live,
            isDispatchPending: true,
            isResolvingConflicts: false,
            isRemediatingCI: false
        )
        guard case .dispatchPending = state else {
            return XCTFail("dispatch-pending must win; got \(state)")
        }
    }

    func testResumingWorkerClearsWaiting() {
        // The same card, once the worker resumes, is no longer waiting —
        // the indicator clears automatically from the activity flip.
        let resumed = AgentActivityState.forDoingCard(
            runtime: makeRuntime(),
            liveState: makeLiveState(activity: .working),
            isDispatchPending: false,
            isResolvingConflicts: false,
            isRemediatingCI: false
        )
        guard case .active = resumed else {
            return XCTFail("resumed worker should read as active; got \(resumed)")
        }
    }

    // MARK: - Helpers

    private func makeLiveState(
        slotId: Int = 1,
        activity: WorkerActivity
    ) -> WorkerLiveState {
        WorkerLiveState(
            slotId: slotId,
            runId: "exec-1",
            model: "claude-opus-4-7",
            shellPid: 1234,
            lastEventAt: "2026-06-01T00:00:00Z",
            currentTool: nil,
            lastToolEndedAt: nil,
            activity: activity,
            liveStatus: "Waiting for user input",
            liveStatusAt: "2026-06-01T00:00:00Z"
        )
    }

    private func makeRuntime() -> WorkTaskRuntime {
        WorkTaskRuntime(
            workItemID: "task_1",
            executionStatus: "running",
            runStatus: "active",
            executionID: "exec-1"
        )
    }
}
