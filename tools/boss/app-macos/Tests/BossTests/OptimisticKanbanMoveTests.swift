import XCTest
@testable import Boss

/// Tests for optimistic kanban drag-drop: the card moves immediately on drop
/// and either reconciles when the engine confirms or bounces back on failure.
@MainActor
final class OptimisticKanbanMoveTests: XCTestCase {

    // MARK: - Optimistic column override on drop

    func testDropMovesCardImmediately() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        // Card starts in Backlog.
        XCTAssertEqual(model.workItems(in: .backlog).map(\.id), [task.id])
        XCTAssertTrue(model.workItems(in: .doing).isEmpty)

        // Drop into Doing — attemptMoveTask should return true and the card
        // should appear in Doing before any engine response.
        let accepted = model.attemptMoveTask(task.id, to: .doing)
        XCTAssertTrue(accepted, "drop should be accepted")
        XCTAssertTrue(model.workItems(in: .doing).map(\.id).contains(task.id),
                      "card must appear in Doing immediately after drop")
        XCTAssertFalse(model.workItems(in: .backlog).map(\.id).contains(task.id),
                       "card must leave Backlog immediately after drop")
    }

    func testOptimisticOverrideRespectedWhileInFlight() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        _ = model.attemptMoveTask(task.id, to: .doing)

        // Engine has not yet responded; tasksByProjectID still has old status.
        // The card must stay in Doing via the optimistic override.
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)
    }

    // MARK: - Reconciliation after engine confirmation

    func testWorkItemUpdatedAppliesIncrementalUpdateForChore() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        _ = model.attemptMoveTask(task.id, to: .doing)

        // Engine confirms the move via work_item_updated alone — no full tree fetch.
        let confirmedTask = makeTask(status: "active", id: task.id)
        model.applyEventForTest(.workItemUpdated(item: .chore(confirmedTask)))

        // Override is cleared and the in-memory store is updated, without a work tree.
        XCTAssertEqual(model.effectiveBoardColumn(for: confirmedTask), .doing,
                       "card must be in Doing after work_item_updated without a tree refresh")
        XCTAssertTrue(model.workItems(in: .doing).map(\.id).contains(task.id),
                      "card must appear in Doing after incremental update")
        XCTAssertFalse(model.workItems(in: .backlog).map(\.id).contains(task.id),
                       "card must not remain in Backlog after incremental update")
    }

    func testWorkItemUpdatedAppliesIncrementalUpdateForProjectTask() {
        let model = makeModel()
        let project = makeProject()
        model.projectsByProductID = ["prod_test": [project]]
        let task = makeProjectTask(status: "todo", projectID: project.id)
        model.tasksByProjectID = [project.id: [task]]

        _ = model.attemptMoveTask(task.id, to: .doing)

        let confirmedTask = makeProjectTask(status: "active", id: task.id, projectID: project.id)
        model.applyEventForTest(.workItemUpdated(item: .task(confirmedTask)))

        XCTAssertEqual(model.effectiveBoardColumn(for: confirmedTask), .doing,
                       "project task must be in Doing after work_item_updated")
        XCTAssertTrue(model.workItems(in: .doing).map(\.id).contains(task.id),
                      "project task must appear in Doing after incremental update")
    }

    func testReconcileDropsOverrideWhenTreeConfirms() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        _ = model.attemptMoveTask(task.id, to: .doing)

        // Simulate engine sending back a work_item_updated with the new status,
        // then a work tree that reflects the updated status.
        let confirmedTask = makeTask(status: "active", id: task.id)
        model.applyEventForTest(.workItemUpdated(item: .chore(confirmedTask)))
        model.applyEventForTest(
            .workTree(
                product: makeProduct(),
                projects: [],
                tasks: [],
                chores: [confirmedTask],
                taskRuntimes: [],
                dependencies: []
            )
        )

        // Override is cleared; real boardColumn (.doing for "active") takes over.
        XCTAssertEqual(model.effectiveBoardColumn(for: confirmedTask), .doing)
        XCTAssertTrue(model.workItems(in: .doing).map(\.id).contains(task.id),
                      "card must still be in Doing after tree reconcile")
    }

    // MARK: - Bounce-back on work_error

    func testWorkErrorBouncesCardBack() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        _ = model.attemptMoveTask(task.id, to: .doing)
        // Card is now optimistically in Doing.
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)

        // Simulate engine rejecting the move.
        model.applyEventForTest(.workError(message: "transition not allowed"))

        // Override cleared; card returns to its real boardColumn (.backlog for "todo").
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .backlog)
        XCTAssertTrue(model.workItems(in: .backlog).map(\.id).contains(task.id),
                      "card must bounce back to Backlog after work_error")
        XCTAssertFalse(model.workItems(in: .doing).map(\.id).contains(task.id),
                       "card must not remain in Doing after work_error")
    }

    func testWorkErrorShowsInlineNoticeNotModal() {
        let model = makeModel()
        let task = makeTask(status: "todo")
        model.choresByProductID = ["prod_test": [task]]

        _ = model.attemptMoveTask(task.id, to: .doing)
        model.applyEventForTest(.workError(message: "test error"))

        // Inline drag-refusal notice is set for the affected task.
        XCTAssertNotNil(model.dragRefusalNotice, "inline notice must be shown")
        XCTAssertEqual(model.dragRefusalNotice?.taskID, task.id)
        // Modal (workErrorMessage) must NOT be set for a kanban-move error.
        XCTAssertNil(model.workErrorMessage,
                     "modal must not interrupt when an optimistic move is in-flight")
    }

    func testWorkErrorFallsBackToModalWhenNoMovePending() {
        let model = makeModel()
        // No pending optimistic move.
        model.applyEventForTest(.workError(message: "some other error"))
        XCTAssertNotNil(model.workErrorMessage,
                        "modal must still show when no move is in-flight")
    }

    // MARK: - No flicker: unrelated engine pushes don't clobber optimistic position

    func testUnrelatedWorkTreeDoesNotClobberOptimisticCard() {
        let model = makeModel()
        let task = makeTask(status: "todo", id: "task_a")
        let other = makeTask(status: "active", id: "task_b")
        model.choresByProductID = ["prod_test": [task, other]]

        _ = model.attemptMoveTask(task.id, to: .doing)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)

        // Engine sends a tree update (unrelated, task_a still has old status).
        model.applyEventForTest(
            .workTree(
                product: makeProduct(),
                projects: [],
                tasks: [],
                chores: [task, other],  // task_a still "todo"
                taskRuntimes: [],
                dependencies: []
            )
        )

        // Optimistic override must still hold — real state is "todo" but move
        // is still pending confirmation.
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing,
                       "optimistic position must survive an unrelated tree push")
    }

    // MARK: - Dispatch-pending autostart case

    func testDispatchPendingDropToBacklogIsOptimistic() {
        let model = makeModel()
        let task = makeTask(status: "todo", autostart: true)
        model.choresByProductID = ["prod_test": [task]]

        // Dispatch-pending task shows in Doing.
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .doing)

        // Drop to Backlog (cancels dispatch).
        let accepted = model.attemptMoveTask(task.id, to: .backlog)
        XCTAssertTrue(accepted)
        XCTAssertEqual(model.effectiveBoardColumn(for: task), .backlog,
                       "card must move to Backlog immediately")
    }

    // MARK: - Helpers

    private func makeTask(
        status: String,
        autostart: Bool = false,
        id: String? = nil
    ) -> WorkTask {
        WorkTask(
            id: id ?? "task_\(UUID().uuidString)",
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "Test",
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-06-01T00:00:00Z",
            updatedAt: "2026-06-01T00:00:00Z",
            autostart: autostart
        )
    }

    private func makeProjectTask(
        status: String,
        id: String? = nil,
        projectID: String
    ) -> WorkTask {
        WorkTask(
            id: id ?? "task_\(UUID().uuidString)",
            productID: "prod_test",
            projectID: projectID,
            kind: "task",
            name: "Test Project Task",
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-06-01T00:00:00Z",
            updatedAt: "2026-06-01T00:00:00Z"
        )
    }

    private func makeProject() -> WorkProject {
        WorkProject(
            id: "proj_test",
            productID: "prod_test",
            name: "Test Project",
            slug: "test",
            description: "",
            goal: "",
            status: "active",
            priority: "medium",
            createdAt: "2026-06-01T00:00:00Z",
            updatedAt: "2026-06-01T00:00:00Z"
        )
    }

    private func makeProduct() -> WorkProduct {
        WorkProduct(
            id: "prod_test",
            name: "Test Product",
            slug: "test",
            description: "",
            repoRemoteURL: nil,
            status: "active",
            createdAt: "2026-06-01T00:00:00Z",
            updatedAt: "2026-06-01T00:00:00Z"
        )
    }

    private func makeModel() -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.products = [makeProduct()]
        model.selectedWorkProductID = "prod_test"
        return model
    }
}
