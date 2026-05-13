import XCTest
@testable import Boss

/// Covers the "dispatch-pending" kanban lane rule:
/// a row that is `status=todo AND autostart=true` routes to the Doing
/// column with a `.dispatchPending` activity state rather than staying
/// in Backlog. A row that is `status=todo AND autostart=false` stays
/// in Backlog (unchanged from the legacy mapping).
@MainActor
final class DispatchPendingKanbanTests: XCTestCase {

    // MARK: boardColumn routing

    func testTodoAutostartRoutesToDoing() {
        let task = makeTask(status: "todo", autostart: true)
        XCTAssertEqual(task.boardColumn, .doing)
    }

    func testTodoNoAutostartRoutesToBacklog() {
        let task = makeTask(status: "todo", autostart: false)
        XCTAssertEqual(task.boardColumn, .backlog)
    }

    func testActiveRoutesToDoing() {
        let task = makeTask(status: "active", autostart: true)
        XCTAssertEqual(task.boardColumn, .doing)
    }

    func testActiveNoAutostartStillRoutesToDoing() {
        let task = makeTask(status: "active", autostart: false)
        XCTAssertEqual(task.boardColumn, .doing)
    }

    func testBlockedRoutesToBacklog() {
        let task = makeTask(status: "blocked", autostart: true)
        XCTAssertEqual(task.boardColumn, .backlog)
    }

    func testDoneRoutesToDone() {
        let task = makeTask(status: "done", autostart: true)
        XCTAssertEqual(task.boardColumn, .done)
    }

    func testInReviewRoutesToReview() {
        let task = makeTask(status: "in_review", autostart: true)
        XCTAssertEqual(task.boardColumn, .review)
    }

    // MARK: AgentActivityState tooltip

    func testDispatchPendingTooltip() {
        let state = AgentActivityState.dispatchPending
        XCTAssertFalse(state.tooltip.isEmpty)
        XCTAssertTrue(
            state.tooltip.lowercased().contains("slot") ||
            state.tooltip.lowercased().contains("queue"),
            "tooltip should mention 'slot' or 'queue'; got: \(state.tooltip)"
        )
    }

    // MARK: workItems(in:) integration

    func testDispatchPendingChoreAppearsInDoingColumn() {
        let model = makeModel()
        let chore = makeTask(status: "todo", autostart: true, kind: "chore")
        model.choresByProductID = ["prod_test": [chore]]
        let doingItems = model.workItems(in: .doing)
        XCTAssertTrue(doingItems.contains(where: { $0.id == chore.id }),
                      "dispatch-pending chore should appear in Doing")
        let backlogItems = model.workItems(in: .backlog)
        XCTAssertFalse(backlogItems.contains(where: { $0.id == chore.id }),
                       "dispatch-pending chore must NOT appear in Backlog")
    }

    func testNonAutostartChoreStaysInBacklog() {
        let model = makeModel()
        let chore = makeTask(status: "todo", autostart: false, kind: "chore")
        model.choresByProductID = ["prod_test": [chore]]
        let backlogItems = model.workItems(in: .backlog)
        XCTAssertTrue(backlogItems.contains(where: { $0.id == chore.id }),
                      "non-autostart todo chore should remain in Backlog")
        let doingItems = model.workItems(in: .doing)
        XCTAssertFalse(doingItems.contains(where: { $0.id == chore.id }),
                       "non-autostart todo chore must NOT appear in Doing")
    }

    // MARK: - Helpers

    private func makeTask(
        status: String,
        autostart: Bool,
        kind: String = "chore"
    ) -> WorkTask {
        WorkTask(
            id: "task_\(UUID().uuidString)",
            productID: "prod_test",
            projectID: nil,
            kind: kind,
            name: "Test item",
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-13T00:00:00Z",
            updatedAt: "2026-05-13T00:00:00Z",
            autostart: autostart
        )
    }

    private func makeModel() -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.products = [
            WorkProduct(
                id: "prod_test",
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: nil,
                status: "active",
                createdAt: "2026-05-13T00:00:00Z",
                updatedAt: "2026-05-13T00:00:00Z"
            )
        ]
        model.selectedWorkProductID = "prod_test"
        return model
    }
}
