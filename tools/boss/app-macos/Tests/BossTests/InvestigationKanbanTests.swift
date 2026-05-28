import XCTest
@testable import Boss

/// Regression coverage for issue #886: a product-level
/// (`project_id IS NULL`) `kind == "investigation"` work item used to be
/// dropped on the floor by the work-tree reception path and was invisible
/// on the kanban even while a live worker produced against it.
///
/// The engine delivers product-level investigations in the work tree's
/// `tasks` array (see `get_work_tree`'s `kind IN (..., 'investigation')`
/// query). The old handler bucketed only project-scoped tasks and chore-
/// parented revisions, silently discarding everything else with no
/// project. These tests drive the real reception path end-to-end via
/// `applyEventForTest` and assert the investigation survives and renders.
@MainActor
final class InvestigationKanbanTests: XCTestCase {

    // MARK: - Reception path

    func testProductLevelInvestigationIsBucketedNotDropped() {
        let model = makeModel()
        let investigation = makeInvestigation(id: "task_inv", status: "active")
        model.applyEventForTest(makeWorkTreeEvent(tasks: [investigation]))

        XCTAssertEqual(
            model.productLevelTasksByProductID["prod_test"]?.map(\.id),
            ["task_inv"],
            "a product-level investigation must be retained in a bucket, not dropped"
        )
        // It must NOT be misfiled under a project or the chores/revision buckets.
        let projectTasks = model.tasksByProjectID.values.flatMap { $0 }
        XCTAssertFalse(projectTasks.contains { $0.id == "task_inv" })
        XCTAssertNil(model.choresByProductID["prod_test"]?.first { $0.id == "task_inv" })
    }

    func testProductLevelInvestigationIsVisibleOnBoard() {
        let model = makeModel()
        let investigation = makeInvestigation(id: "task_inv", status: "active")
        model.applyEventForTest(makeWorkTreeEvent(tasks: [investigation]))

        XCTAssertTrue(
            model.visibleWorkItems.contains { $0.id == "task_inv" },
            "product-level investigation must appear in the visible board items"
        )
    }

    func testActiveInvestigationRoutesToDoingColumn() {
        let model = makeModel()
        let investigation = makeInvestigation(id: "task_inv", status: "active")
        model.applyEventForTest(makeWorkTreeEvent(tasks: [investigation]))

        XCTAssertTrue(
            model.workItems(in: .doing).contains { $0.id == "task_inv" },
            "an active investigation (worker producing against it) must land in Doing"
        )
    }

    func testInvestigationVisibleEvenWhenChoresHidden() {
        // Investigations are first-class work, not chores — toggling the
        // chores filter off must not hide them.
        let model = makeModel()
        model.includeChores = false
        let investigation = makeInvestigation(id: "task_inv", status: "active")
        model.applyEventForTest(makeWorkTreeEvent(tasks: [investigation]))

        XCTAssertTrue(
            model.visibleWorkItems.contains { $0.id == "task_inv" },
            "product-level investigation must stay visible with chores hidden"
        )
    }

    func testProductLevelInvestigationIsResolvableByID() {
        let model = makeModel()
        let investigation = makeInvestigation(id: "task_inv", status: "active")
        model.applyEventForTest(makeWorkTreeEvent(tasks: [investigation]))

        XCTAssertEqual(
            model.workTask(withID: "task_inv")?.id,
            "task_inv",
            "card selection / detail lookup must resolve a product-level investigation"
        )
    }

    // MARK: - kindLabel

    func testKindLabelNamesInvestigation() {
        let investigation = makeInvestigation(id: "task_inv", status: "active")
        XCTAssertEqual(investigation.kindLabel, "Investigation")
    }

    func testKindLabelTitleCasesUnknownKind() {
        // A kind the app doesn't enumerate still reads sensibly rather than
        // being mislabeled "Task".
        let novel = makeInvestigation(id: "task_x", status: "todo", kind: "spike_analysis")
        XCTAssertEqual(novel.kindLabel, "Spike Analysis")
    }

    // MARK: - Helpers

    private func makeInvestigation(
        id: String,
        status: String,
        kind: String = "investigation"
    ) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: kind,
            name: "Investigation \(id)",
            description: "Bisect candidates",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-26T00:00:00Z",
            updatedAt: "2026-05-26T00:00:00Z"
        )
    }

    private func makeWorkTreeEvent(tasks: [WorkTask] = [], chores: [WorkTask] = []) -> EngineEvent {
        .workTree(
            product: WorkProduct(
                id: "prod_test",
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: "https://github.com/org/repo.git",
                status: "active",
                createdAt: "2026-05-26T00:00:00Z",
                updatedAt: "2026-05-26T00:00:00Z"
            ),
            projects: [],
            tasks: tasks,
            chores: chores,
            taskRuntimes: [],
            dependencies: []
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
                repoRemoteURL: "https://github.com/org/repo.git",
                status: "active",
                createdAt: "2026-05-26T00:00:00Z",
                updatedAt: "2026-05-26T00:00:00Z"
            )
        ]
        model.selectWorkProduct("prod_test")
        return model
    }
}
