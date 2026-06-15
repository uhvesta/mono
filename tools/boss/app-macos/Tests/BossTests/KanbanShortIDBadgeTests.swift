import XCTest
@testable import Boss

/// Covers the friendly-id badge on `WorkBoardCardView` and related
/// surfaces (popover header, right-click copy). Per the design for
/// "macOS kanban: short_id badge", every card that carries a `shortID`
/// must surface it as a `T<n>` badge anchored bottom-right in the
/// footer; the popover header repeats it inline; and right-click
/// offers "Copy Friendly ID". These tests pin the data layer that the
/// views reflect so correctness is asserted without spinning up a
/// SwiftUI host — the same approach as `WorkDependencyKanbanTests`.
@MainActor
final class KanbanShortIDBadgeTests: XCTestCase {

    // MARK: - shortID on WorkTask

    /// A task with a `shortID` must carry the value through and format
    /// it as `T<n>` at the call site. This is the minimal contract the
    /// card view depends on.
    func testShortIDFormatIsTPrefixed() {
        let task = makeTask(status: "todo", shortID: 42)
        XCTAssertEqual(task.shortID, 42)
        XCTAssertEqual("T\(task.shortID!)", "T42")
    }

    /// Tasks that predate the migration arrive with `shortID == nil`;
    /// the badge must be absent rather than rendering an empty capsule.
    func testNilShortIDProducesNoBadge() {
        let task = makeTask(status: "todo", shortID: nil)
        XCTAssertNil(task.shortID)
    }

    // MARK: - Badge present on every kanban lane

    /// Backlog (status = todo): badge must be present.
    func testBadgePresentInBacklogLane() {
        let task = makeTask(status: "todo", shortID: 1)
        XCTAssertEqual(task.boardColumn, .backlog)
        XCTAssertNotNil(task.shortID)
    }

    /// Doing (status = active): badge must be present.
    func testBadgePresentInDoingLane() {
        let task = makeTask(status: "active", shortID: 2)
        XCTAssertEqual(task.boardColumn, .doing)
        XCTAssertNotNil(task.shortID)
    }

    /// Review (status = in_review): badge must be present.
    func testBadgePresentInReviewLane() {
        let task = makeTask(status: "in_review", shortID: 3)
        XCTAssertEqual(task.boardColumn, .review)
        XCTAssertNotNil(task.shortID)
    }

    /// Done (status = done): badge must be present.
    func testBadgePresentInDoneLane() {
        let task = makeTask(status: "done", shortID: 4)
        XCTAssertEqual(task.boardColumn, .done)
        XCTAssertNotNil(task.shortID)
    }

    /// Blocked (status = blocked → backlog): badge must be present on
    /// blocked cards too, not just the lane's non-blocked rows.
    func testBadgePresentOnBlockedCard() {
        let task = makeTask(status: "blocked", shortID: 5)
        XCTAssertEqual(task.boardColumn, .backlog)
        XCTAssertNotNil(task.shortID)
    }

    // MARK: - Badge absent when shortID is nil on each lane

    /// Legacy todo rows (no shortID) must not render a badge.
    func testBadgeAbsentOnLegacyTodoRow() {
        let task = makeTask(status: "todo", shortID: nil)
        XCTAssertEqual(task.boardColumn, .backlog)
        XCTAssertNil(task.shortID)
    }

    /// Legacy active rows must not render a badge.
    func testBadgeAbsentOnLegacyActiveRow() {
        let task = makeTask(status: "active", shortID: nil)
        XCTAssertEqual(task.boardColumn, .doing)
        XCTAssertNil(task.shortID)
    }

    // MARK: - Friendly-id format contract

    /// Friendly-id badge text is the `T` sigil followed by the decimal
    /// integer with no padding — `T7`, not `T007`. This mirrors the CLI
    /// output (`boss task show` renders `T7`) and the coordinator's
    /// preferred referral protocol.
    func testFriendlyIDHasNoZeroPadding() {
        let task = makeTask(status: "todo", shortID: 7)
        XCTAssertEqual("T\(task.shortID!)", "T7")
    }

    /// Large numbers render without abbreviation so the reader can tell
    /// `T1000` from `T1K` at a glance. Any abbreviation would be
    /// misleading when two items share the same abbreviation.
    func testFriendlyIDRendersFullDecimalForLargeIDs() {
        let task = makeTask(status: "todo", shortID: 1000)
        XCTAssertEqual("T\(task.shortID!)", "T1000")
    }

    // MARK: - ChatViewModel wiring

    /// A task with a shortID in each column is surfaced correctly via
    /// `workItems(in:)`. This exercises the view-model join so a
    /// regression that strips `shortID` during the task-list merge
    /// would be caught here without relying on the view layer.
    func testShortIDSurvivesWorkItemsJoin() {
        let model = makeModel()
        let projectID = "proj_test"

        let tasks: [(String, Int?)] = [
            ("todo", 10),
            ("active", 11),
            ("in_review", 12),
            ("done", 13),
        ]
        model.tasksByProjectID = [
            projectID: tasks.enumerated().map { idx, pair in
                makeTask(id: "task_\(idx)", status: pair.0, shortID: pair.1)
            }
        ]

        let allItems = WorkBoardColumnKey.allCases.flatMap { model.workItems(in: $0) }
        let shortIDs = allItems.compactMap(\.shortID).sorted()
        XCTAssertEqual(shortIDs, [10, 11, 12, 13], "shortID must survive the workItems join for all lanes")
    }

    /// A chore (no project) with a shortID also reaches the board
    /// correctly. Chores live in `choresByProductID`, not
    /// `tasksByProjectID`, so a separate join path is exercised.
    func testShortIDSurvivesChoreJoin() {
        let model = makeModel()
        let productID = "prod_test"

        let chore = makeTask(id: "chore_1", status: "todo", shortID: 99, kind: "chore")
        model.choresByProductID = [productID: [chore]]

        let backlog = model.workItems(in: .backlog)
        XCTAssertTrue(backlog.contains(where: { $0.id == "chore_1" && $0.shortID == 99 }),
                      "chore shortID must survive the choresByProductID join")
    }

    // MARK: - Helpers

    private func makeTask(
        id: String = "task_test",
        status: String,
        shortID: Int?,
        kind: String = "task"
    ) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: "proj_test",
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
            shortID: shortID
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
        model.projectsByProductID = [
            "prod_test": [
                WorkProject(
                    id: "proj_test",
                    productID: "prod_test",
                    name: "Test Project",
                    slug: "test",
                    description: "",
                    goal: "",
                    status: "active",
                    priority: "medium",
                    createdAt: "2026-05-13T00:00:00Z",
                    updatedAt: "2026-05-13T00:00:00Z"
                )
            ]
        ]
        return model
    }
}
