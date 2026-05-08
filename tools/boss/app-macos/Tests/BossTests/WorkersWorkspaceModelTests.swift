import XCTest
@testable import Boss

@MainActor
final class WorkersWorkspaceModelFocusTests: XCTestCase {
    func testFocusUnknownSlotReturnsUnknownSlot() {
        let model = WorkersWorkspaceModel()
        // Workers grid is 1...8; 99 has no slot at all.
        let result = model.focusWorkerPane(slotId: 99)
        guard case .failure(.unknownSlot) = result else {
            XCTFail("expected .unknownSlot for nonexistent slot, got \(result)")
            return
        }
    }

    func testFocusIdleSlotReturnsUnknownSlot() {
        let model = WorkersWorkspaceModel()
        // All slots start without a session attached. Focusing an
        // idle slot should fail the same way as an unknown one — the
        // app has nothing to raise. Mirrors the
        // `release_worker_pane` semantics for idle slots so the engine
        // can treat both cases the same way.
        let result = model.focusWorkerPane(slotId: 1)
        guard case .failure(.unknownSlot) = result else {
            XCTFail("expected .unknownSlot for idle slot, got \(result)")
            return
        }
    }
}

@MainActor
final class WorkersWorkspaceModelSpawnTests: XCTestCase {
    private func makeRequest(slot: Int, runId: String = "run-test") -> EngineSpawnRequest {
        EngineSpawnRequest(
            runId: runId,
            workspacePath: "/tmp/ws",
            slotId: slot,
            initialInput: "claude\n",
            env: [],
            summary: nil
        )
    }

    func testSpawnHonorsEngineClaimedSlot() {
        // Engine asked for slot 5. The app must host the pane in
        // slot 5 — not the lowest free slot, not a random one. This
        // is the contract that replaces the old firstIndex(where:)
        // heuristic.
        let model = WorkersWorkspaceModel()
        let result = model.spawnWorkerPane(makeRequest(slot: 5))
        guard case .success(let slotId, _) = result else {
            XCTFail("expected .success, got \(result)")
            return
        }
        XCTAssertEqual(slotId, 5, "app must honor the engine-supplied slot")
        XCTAssertNotNil(
            model.slots.first(where: { $0.slotId == 5 })?.session,
            "slot 5 should now host a session"
        )
        XCTAssertNil(
            model.slots.first(where: { $0.slotId == 1 })?.session,
            "no other slot should be touched when the engine asked for slot 5"
        )
    }

    func testSpawnIntoOccupiedSlotReturnsSlotBusy() {
        // Engine and app disagree about whether slot 3 is free. The
        // app must surface .slotBusy rather than silently picking a
        // different slot — that would re-introduce the dual
        // allocator the engine-owns-slots refactor exists to remove.
        let model = WorkersWorkspaceModel()
        _ = model.spawnWorkerPane(makeRequest(slot: 3, runId: "run-first"))
        let result = model.spawnWorkerPane(makeRequest(slot: 3, runId: "run-second"))
        guard case .failure(.slotBusy) = result else {
            XCTFail("expected .slotBusy when engine asks for an occupied slot, got \(result)")
            return
        }
    }

    func testSpawnRejectsOutOfRangeSlot() {
        let model = WorkersWorkspaceModel()
        let zeroResult = model.spawnWorkerPane(makeRequest(slot: 0))
        guard case .failure(.internalFailure) = zeroResult else {
            XCTFail("expected .internalFailure for slot 0, got \(zeroResult)")
            return
        }
        let highResult = model.spawnWorkerPane(makeRequest(slot: 99))
        guard case .failure(.internalFailure) = highResult else {
            XCTFail("expected .internalFailure for slot 99, got \(highResult)")
            return
        }
    }
}
