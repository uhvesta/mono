import XCTest
@testable import Boss

/// Covers revision-kind kanban routing rules:
///
/// - An `in_review` revision must NOT appear as a standalone card in the
///   Review column (it rolls up under the parent's card instead).
/// - An `in_review` revision DOES NOT appear in Backlog or Doing either
///   (it has no standalone card anywhere in that status).
/// - A revision that is `todo` or `active` appears in Backlog/Doing as a
///   distinct card — NOT hidden.
/// - `inReviewRevisions(forParentTaskID:)` returns the right revisions,
///   ordered by `revisionSeq`.
/// - `workTask(withID:)` resolves tasks from both project-task and chore
///   buckets.
@MainActor
final class RevisionKanbanTests: XCTestCase {

    // MARK: boardColumn routing

    func testInReviewRevisionRoutesMappedToReview() {
        let revision = makeRevision(status: "in_review", seq: 1)
        XCTAssertEqual(revision.boardColumn, .review,
                       "in_review revision's boardColumn should be .review")
    }

    func testTodoRevisionRoutesToBacklog() {
        let revision = makeRevision(status: "todo", seq: 1)
        XCTAssertEqual(revision.boardColumn, .backlog)
    }

    func testActiveRevisionRoutesToDoing() {
        let revision = makeRevision(status: "active", seq: 2)
        XCTAssertEqual(revision.boardColumn, .doing)
    }

    // MARK: workItems(in:) suppression

    func testInReviewRevisionSuppressedFromReviewColumn() {
        let model = makeModel()
        let parent = makeParent(status: "in_review")
        let revision = makeRevision(status: "in_review", seq: 1, parentID: parent.id)
        model.tasksByProjectID = ["proj_1": [parent, revision]]

        let reviewItems = model.workItems(in: .review)
        XCTAssertTrue(
            reviewItems.contains(where: { $0.id == parent.id }),
            "parent must appear in Review"
        )
        XCTAssertFalse(
            reviewItems.contains(where: { $0.id == revision.id }),
            "in_review revision must NOT appear as a standalone card in Review"
        )
    }

    func testTodoRevisionNotSuppressedFromBacklog() {
        let model = makeModel()
        let revision = makeRevision(status: "todo", seq: 1)
        model.tasksByProjectID = ["proj_1": [revision]]

        let backlogItems = model.workItems(in: .backlog)
        XCTAssertTrue(
            backlogItems.contains(where: { $0.id == revision.id }),
            "todo revision must appear in Backlog"
        )
    }

    func testActiveRevisionNotSuppressedFromDoing() {
        let model = makeModel()
        let revision = makeRevision(status: "active", seq: 2)
        model.tasksByProjectID = ["proj_1": [revision]]

        let doingItems = model.workItems(in: .doing)
        XCTAssertTrue(
            doingItems.contains(where: { $0.id == revision.id }),
            "active revision must appear in Doing"
        )
    }

    // MARK: inReviewRevisions(forParentTaskID:)

    func testInReviewRevisionsReturnedForParent() {
        let model = makeModel()
        let parent = makeParent(status: "in_review")
        let r1 = makeRevision(status: "in_review", seq: 1, parentID: parent.id)
        let r2 = makeRevision(status: "in_review", seq: 2, parentID: parent.id)
        model.tasksByProjectID = ["proj_1": [parent, r1, r2]]

        let revisions = model.inReviewRevisions(forParentTaskID: parent.id)
        XCTAssertEqual(revisions.count, 2)
        XCTAssertEqual(revisions[0].id, r1.id, "R1 should come first (ordered by seq)")
        XCTAssertEqual(revisions[1].id, r2.id, "R2 should come second")
    }

    func testInReviewRevisionsExcludesNonInReview() {
        let model = makeModel()
        let parent = makeParent(status: "in_review")
        let r1 = makeRevision(status: "in_review", seq: 1, parentID: parent.id)
        let r2 = makeRevision(status: "todo", seq: 2, parentID: parent.id)
        model.tasksByProjectID = ["proj_1": [parent, r1, r2]]

        let revisions = model.inReviewRevisions(forParentTaskID: parent.id)
        XCTAssertEqual(revisions.count, 1)
        XCTAssertEqual(revisions[0].id, r1.id, "only in_review revisions should appear")
    }

    func testInReviewRevisionsExcludesOtherParents() {
        let model = makeModel()
        let parent1 = makeParent(status: "in_review", suffix: "1")
        let parent2 = makeParent(status: "in_review", suffix: "2")
        let r1 = makeRevision(status: "in_review", seq: 1, parentID: parent1.id)
        let r2 = makeRevision(status: "in_review", seq: 1, parentID: parent2.id)
        model.tasksByProjectID = ["proj_1": [parent1, parent2, r1, r2]]

        let revisionsForP1 = model.inReviewRevisions(forParentTaskID: parent1.id)
        XCTAssertEqual(revisionsForP1.count, 1)
        XCTAssertEqual(revisionsForP1[0].id, r1.id)

        let revisionsForP2 = model.inReviewRevisions(forParentTaskID: parent2.id)
        XCTAssertEqual(revisionsForP2.count, 1)
        XCTAssertEqual(revisionsForP2[0].id, r2.id)
    }

    func testInReviewRevisionsEmptyForNonParent() {
        let model = makeModel()
        let task = makeParent(status: "in_review")
        model.tasksByProjectID = ["proj_1": [task]]

        let revisions = model.inReviewRevisions(forParentTaskID: task.id)
        XCTAssertTrue(revisions.isEmpty, "task with no revisions should return empty list")
    }

    // MARK: workTask(withID:)

    func testWorkTaskWithIDFindsProjectTask() {
        let model = makeModel()
        let task = makeParent(status: "active")
        model.tasksByProjectID = ["proj_1": [task]]
        XCTAssertEqual(model.workTask(withID: task.id)?.id, task.id)
    }

    func testWorkTaskWithIDFindsChore() {
        let model = makeModel()
        let chore = WorkTask(
            id: "chore_abc",
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "A chore",
            description: "",
            status: "todo",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-26T00:00:00Z",
            updatedAt: "2026-05-26T00:00:00Z"
        )
        model.choresByProductID = ["prod_test": [chore]]
        XCTAssertEqual(model.workTask(withID: chore.id)?.id, chore.id)
    }

    func testWorkTaskWithIDReturnsNilForMissing() {
        let model = makeModel()
        XCTAssertNil(model.workTask(withID: "nonexistent_id"))
    }

    // MARK: chore-parented revisions (issue #789)
    //
    // A revision inherits its project from the chain root, so a chore-parented
    // revision has no project_id. The work-tree reception used to drop every
    // project-less task, making such revisions invisible everywhere in the
    // kanban. These tests drive the real reception path (`applyEventForTest`)
    // to prove they now survive and render.

    func testChoreParentedRevisionSurvivesReceptionAsDoingCard() {
        let model = makeModel()
        let chore = makeChore(id: "chore_c19", status: "active")
        // Mirrors the repro: a todo, autostart revision (→ Doing).
        let revision = makeChoreParentedRevision(
            id: "task_t29", status: "todo", seq: 1, parentID: chore.id, autostart: true
        )
        model.applyEventForTest(makeWorkTreeEvent(tasks: [revision], chores: [chore]))

        XCTAssertEqual(
            model.productLevelRevisionsByProductID["prod_test"]?.map(\.id),
            [revision.id],
            "chore-parented revision must be retained in the product-level bucket"
        )
        XCTAssertTrue(
            model.workItems(in: .doing).contains { $0.id == revision.id },
            "chore-parented revision must render as a standalone Doing card"
        )
    }

    func testChoreParentedInReviewRevisionRollsUpUnderChore() {
        let model = makeModel()
        let chore = makeChore(id: "chore_c19", status: "in_review")
        let revision = makeChoreParentedRevision(
            id: "task_t29", status: "in_review", seq: 1, parentID: chore.id
        )
        model.applyEventForTest(makeWorkTreeEvent(tasks: [revision], chores: [chore]))

        let rollup = model.inReviewRevisions(forParentTaskID: chore.id)
        XCTAssertEqual(rollup.map(\.id), [revision.id],
                       "in-review chore-parented revision must roll up under its parent chore")

        let reviewItems = model.workItems(in: .review)
        XCTAssertTrue(reviewItems.contains { $0.id == chore.id },
                      "parent chore must appear in Review")
        XCTAssertFalse(reviewItems.contains { $0.id == revision.id },
                       "in-review revision must not also appear as a standalone Review card")
    }

    func testWorkTaskWithIDFindsChoreParentedRevision() {
        let model = makeModel()
        let chore = makeChore(id: "chore_c19", status: "in_review")
        let revision = makeChoreParentedRevision(
            id: "task_t29", status: "todo", seq: 1, parentID: chore.id
        )
        model.applyEventForTest(makeWorkTreeEvent(tasks: [revision], chores: [chore]))

        XCTAssertEqual(model.workTask(withID: revision.id)?.id, revision.id,
                       "workTask(withID:) must resolve a chore-parented revision")
    }

    // MARK: - WorkTask fields

    func testRevisionFieldsDefaultToNil() {
        let task = makeParent(status: "todo")
        XCTAssertNil(task.parentTaskId)
        XCTAssertNil(task.revisionSeq)
        XCTAssertNil(task.revisionParentPrUrl)
    }

    func testRevisionFieldsPreserved() {
        let revision = makeRevision(
            status: "in_review",
            seq: 3,
            parentID: "parent_123",
            parentPrUrl: "https://github.com/org/repo/pull/42"
        )
        XCTAssertEqual(revision.parentTaskId, "parent_123")
        XCTAssertEqual(revision.revisionSeq, 3)
        XCTAssertEqual(revision.revisionParentPrUrl, "https://github.com/org/repo/pull/42")
    }

    // MARK: - Helpers

    private func makeParent(status: String, suffix: String = "") -> WorkTask {
        WorkTask(
            id: "task_parent\(suffix)_\(UUID().uuidString)",
            productID: "prod_test",
            projectID: "proj_1",
            kind: "chore",
            name: "Parent task\(suffix)",
            description: "Parent description",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: status == "in_review" ? "https://github.com/org/repo/pull/1" : nil,
            deletedAt: nil,
            createdAt: "2026-05-26T00:00:00Z",
            updatedAt: "2026-05-26T00:00:00Z"
        )
    }

    private func makeRevision(
        status: String,
        seq: Int,
        parentID: String = "task_parent",
        parentPrUrl: String = "https://github.com/org/repo/pull/1"
    ) -> WorkTask {
        WorkTask(
            id: "task_revision_\(seq)_\(UUID().uuidString)",
            productID: "prod_test",
            projectID: "proj_1",
            kind: "revision",
            name: "Revision \(seq) description",
            description: "Detailed revision intent",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-26T00:00:00Z",
            updatedAt: "2026-05-26T00:00:00Z",
            parentTaskId: parentID,
            revisionSeq: seq,
            revisionParentPrUrl: parentPrUrl
        )
    }

    /// A chore (kind = "chore"), which has no project — the chain root in the
    /// issue #789 repro.
    private func makeChore(id: String, status: String) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "Chore \(id)",
            description: "Chore description",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: status == "in_review" ? "https://github.com/org/repo/pull/250" : nil,
            deletedAt: nil,
            createdAt: "2026-05-26T00:00:00Z",
            updatedAt: "2026-05-26T00:00:00Z"
        )
    }

    /// A revision whose parent is a chore: it inherits the chore's (absent)
    /// project, so `projectID` is nil — the shape the reception path used to
    /// drop.
    private func makeChoreParentedRevision(
        id: String,
        status: String,
        seq: Int,
        parentID: String,
        autostart: Bool = false
    ) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "revision",
            name: "Revision \(seq) description",
            description: "Detailed revision intent",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-26T00:00:00Z",
            updatedAt: "2026-05-26T00:00:00Z",
            autostart: autostart,
            parentTaskId: parentID,
            revisionSeq: seq,
            revisionParentPrUrl: "https://github.com/org/repo/pull/250"
        )
    }

    private func makeWorkTreeEvent(
        tasks: [WorkTask] = [],
        chores: [WorkTask] = []
    ) -> EngineEvent {
        .workTree(
            product: WorkProduct(
                id: "prod_test",
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: nil,
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
                repoRemoteURL: nil,
                status: "active",
                createdAt: "2026-05-26T00:00:00Z",
                updatedAt: "2026-05-26T00:00:00Z"
            )
        ]
        model.projectsByProductID = [
            "prod_test": [
                WorkProject(
                    id: "proj_1",
                    productID: "prod_test",
                    name: "Test Project",
                    slug: "test-project",
                    description: "",
                    goal: "",
                    status: "active",
                    priority: "medium",
                    createdAt: "2026-05-26T00:00:00Z",
                    updatedAt: "2026-05-26T00:00:00Z"
                )
            ]
        ]
        model.selectedWorkProductID = "prod_test"
        // Chores (and chore-parented revisions) are gated on this; pin it on so
        // tests don't depend on the machine's persisted UserDefaults value.
        model.includeChores = true
        return model
    }
}
