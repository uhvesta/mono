import XCTest
@testable import Boss

/// Covers the green `▶ N` / red `⏸ N` task-count chips on navigator
/// project rows. The view itself is thin — it delegates all logic to
/// `ChatViewModel.unblockedTaskCount(forProjectID:)` and
/// `blockedTaskCount(forProjectID:)` — so these tests cover the model
/// layer, following the same pattern as `WorkDependencyKanbanTests`.
///
/// Acceptance:
///   - ready-only: green chip, no red chip
///   - blocked-only: red chip, no green chip
///   - mixed: both chips (matching the P491 example)
///   - all-done: both counts zero (chips suppressed by the call site)
@MainActor
final class ProjectNavigatorCountBadgeTests: XCTestCase {

    // MARK: - ready-only

    /// A project where every non-done task is `todo` (no dependency
    /// gating) must produce a positive unblocked count and zero blocked.
    func testReadyOnlyProjectYieldsGreenCountOnly() {
        let model = makeModel(todoCount: 3, depBlockedCount: 0, doneCount: 1)
        XCTAssertEqual(model.unblockedTaskCount(forProjectID: projectID), 3)
        XCTAssertEqual(model.blockedTaskCount(forProjectID: projectID), 0)
    }

    /// A single todo task with no other tasks — the minimal "unblocked"
    /// project that matches the P491 example's unblocked side.
    func testSingleReadyTaskCountsAsOneUnblocked() {
        let model = makeModel(todoCount: 1, depBlockedCount: 0, doneCount: 0)
        XCTAssertEqual(model.unblockedTaskCount(forProjectID: projectID), 1)
        XCTAssertEqual(model.blockedTaskCount(forProjectID: projectID), 0)
    }

    // MARK: - blocked-only

    /// All non-done tasks are dependency-blocked — only the red chip
    /// should appear; green count is zero.
    func testBlockedOnlyProjectYieldsRedCountOnly() {
        let model = makeModel(todoCount: 0, depBlockedCount: 5, doneCount: 2)
        XCTAssertEqual(model.unblockedTaskCount(forProjectID: projectID), 0)
        XCTAssertEqual(model.blockedTaskCount(forProjectID: projectID), 5)
    }

    // MARK: - mixed

    /// Mixed state — matches the P491 acceptance criterion (T494 is
    /// the only unblocked task; 12 others are gated).
    func testMixedProjectYieldsBothCounts() {
        let model = makeModel(todoCount: 1, depBlockedCount: 12, doneCount: 0)
        XCTAssertEqual(model.unblockedTaskCount(forProjectID: projectID), 1)
        XCTAssertEqual(model.blockedTaskCount(forProjectID: projectID), 12)
    }

    // MARK: - all-done

    /// All tasks done → both counts are zero; the call site suppresses
    /// both chips and renders just the project name.
    func testAllDoneProjectYieldsZeroCounts() {
        let model = makeModel(todoCount: 0, depBlockedCount: 0, doneCount: 7)
        XCTAssertEqual(model.unblockedTaskCount(forProjectID: projectID), 0)
        XCTAssertEqual(model.blockedTaskCount(forProjectID: projectID), 0)
    }

    /// An empty project (no tasks at all) must also return zero for
    /// both counts so the chips are suppressed cleanly.
    func testEmptyProjectYieldsZeroCounts() {
        let model = makeModel(todoCount: 0, depBlockedCount: 0, doneCount: 0)
        XCTAssertEqual(model.unblockedTaskCount(forProjectID: projectID), 0)
        XCTAssertEqual(model.blockedTaskCount(forProjectID: projectID), 0)
    }

    // MARK: - blocked reason discrimination

    /// Only `blocked_reason == "dependency"` tasks count toward the
    /// red chip. Other block reasons (merge conflict, CI failure, review
    /// feedback) are not dependency-gated and must be excluded.
    func testNonDependencyBlocksDoNotCountAsBlocked() {
        let model = makeModel(todoCount: 2, depBlockedCount: 0, doneCount: 0)
        let otherBlockReasons = ["merge_conflict", "ci_failure", "review_feedback", "ci_failure_exhausted"]
        for reason in otherBlockReasons {
            model.tasksByProjectID[projectID, default: []].append(
                makeTask(status: "blocked", blockedReason: reason)
            )
        }
        XCTAssertEqual(model.unblockedTaskCount(forProjectID: projectID), 2)
        XCTAssertEqual(model.blockedTaskCount(forProjectID: projectID), 0)
    }

    /// A task with `blocked_reason == nil` (manual / legacy block with
    /// no tracked reason) must not count as dependency-blocked.
    func testNilBlockedReasonDoesNotCountAsBlocked() {
        let model = makeModel(todoCount: 1, depBlockedCount: 0, doneCount: 0)
        model.tasksByProjectID[projectID, default: []].append(
            makeTask(status: "blocked", blockedReason: nil)
        )
        XCTAssertEqual(model.unblockedTaskCount(forProjectID: projectID), 1)
        XCTAssertEqual(model.blockedTaskCount(forProjectID: projectID), 0)
    }

    // MARK: - active / in-review exclusion

    /// Active and in-review tasks are in-flight and must not inflate
    /// either count — they belong in neither the "ready" nor the
    /// "gated" bucket.
    func testActiveAndInReviewTasksAreExcludedFromBothCounts() {
        let model = makeModel(todoCount: 1, depBlockedCount: 1, doneCount: 0)
        model.tasksByProjectID[projectID, default: []].append(
            makeTask(status: "active", blockedReason: nil)
        )
        model.tasksByProjectID[projectID, default: []].append(
            makeTask(status: "in_review", blockedReason: nil)
        )
        XCTAssertEqual(model.unblockedTaskCount(forProjectID: projectID), 1)
        XCTAssertEqual(model.blockedTaskCount(forProjectID: projectID), 1)
    }

    // MARK: - Helpers

    private let productID = "prod_test"
    private let projectID = "proj_test"

    private func makeModel(
        todoCount: Int,
        depBlockedCount: Int,
        doneCount: Int
    ) -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-nav-badge-\(UUID().uuidString).sock")
        model.products = [
            WorkProduct(
                id: productID,
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: nil,
                status: "active",
                createdAt: "2026-05-15T00:00:00Z",
                updatedAt: "2026-05-15T00:00:00Z"
            )
        ]
        model.selectedWorkProductID = productID
        model.projectsByProductID = [
            productID: [
                WorkProject(
                    id: projectID,
                    productID: productID,
                    name: "Test Project",
                    slug: "test",
                    description: "",
                    goal: "",
                    status: "active",
                    priority: "medium",
                    createdAt: "2026-05-15T00:00:00Z",
                    updatedAt: "2026-05-15T00:00:00Z"
                )
            ]
        ]
        var tasks: [WorkTask] = []
        for i in 0..<todoCount {
            tasks.append(makeTask(id: "task_todo_\(i)", status: "todo", blockedReason: nil))
        }
        for i in 0..<depBlockedCount {
            tasks.append(makeTask(id: "task_dep_\(i)", status: "blocked", blockedReason: "dependency"))
        }
        for i in 0..<doneCount {
            tasks.append(makeTask(id: "task_done_\(i)", status: "done", blockedReason: nil))
        }
        model.tasksByProjectID = [projectID: tasks]
        return model
    }

    private var taskCounter = 0

    private func makeTask(
        id: String? = nil,
        status: String,
        blockedReason: String?
    ) -> WorkTask {
        taskCounter += 1
        return WorkTask(
            id: id ?? "task_\(taskCounter)",
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: "Task \(taskCounter)",
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-15T00:00:00Z",
            updatedAt: "2026-05-15T00:00:00Z",
            blockedReason: blockedReason
        )
    }
}
