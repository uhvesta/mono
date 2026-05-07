import XCTest
@testable import BossMacApp

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
