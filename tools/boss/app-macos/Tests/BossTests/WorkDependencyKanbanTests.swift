import XCTest
@testable import Boss

/// Drives the kanban-side dependency surfaces (chain badge, drag
/// refusal, popover Dependencies subsection) by populating
/// `ChatViewModel`'s published state with a synthetic product and
/// asserting the helpers that the views read. The view code itself
/// is a thin reflection of these helpers — covering them is what the
/// design's "snapshot tests for the badge state and the empty /
/// populated dependency lists" boils down to without a snapshot
/// library wired into the package.
@MainActor
final class WorkDependencyKanbanTests: XCTestCase {
    /// An auto-blocked task — engine-set status, unsatisfied prereq —
    /// must trip the chain-badge predicate. Manual-block parity is
    /// covered separately so a regression that conflates the two
    /// shows up as a clear failing case rather than a silent miss.
    func testAutoBlockedTaskTripsBadgePredicate() {
        let model = makeFixture()
        guard let dependent = model.taskByName("Phase 4") else {
            XCTFail("expected fixture to include the gated task"); return
        }
        XCTAssertTrue(model.isAutoBlocked(dependent))
        XCTAssertEqual(model.gatingPrereqs(for: dependent.id).map(\.title), ["Phase 2"])
    }

    /// A human-set blocked row keeps the lane but loses the chain
    /// badge — design Q7 explicitly carves manual blocks out so the
    /// icon doesn't double up with the lane label.
    func testManualBlockHidesChainBadge() {
        let model = makeFixture()
        model.upsertTaskForTest(
            id: "task_manual",
            name: "Manual block",
            status: "blocked",
            lastStatusActor: "human"
        )
        guard let manual = model.taskByName("Manual block") else {
            XCTFail("expected manual-block fixture"); return
        }
        XCTAssertFalse(model.isAutoBlocked(manual))
    }

    /// `dependencyPrereqs` and `dependencyDependents` underpin the
    /// popover Dependencies subsection. An ungated chore should
    /// return empty lists so the subsection collapses cleanly per the
    /// design.
    func testDependencyListsAreEmptyForUngatedItem() {
        let model = makeFixture()
        guard let lone = model.taskByName("Phase 1") else {
            XCTFail("expected fixture"); return
        }
        XCTAssertEqual(model.dependencyPrereqs(for: lone.id), [])
        XCTAssertEqual(model.dependencyDependents(for: lone.id), [])
    }

    /// Populated lists must surface both incoming and outgoing edges
    /// joined against the work tree's titles and statuses so the
    /// popover can render hyperlinks instead of bare ids.
    func testDependencyListsExposeIncomingAndOutgoingEdges() {
        let model = makeFixture()
        guard let prereq = model.taskByName("Phase 2"),
              let dependent = model.taskByName("Phase 4")
        else {
            XCTFail("expected fixture"); return
        }
        XCTAssertEqual(model.dependencyDependents(for: prereq.id).map(\.title), ["Phase 4"])
        XCTAssertEqual(model.dependencyPrereqs(for: dependent.id).map(\.title), ["Phase 2"])
        XCTAssertEqual(model.dependencyPrereqs(for: dependent.id).first?.status, "active")
    }

    /// Drag refusal: dropping a gated row out of Blocked must be
    /// rejected and surface an inline notice keyed to the source
    /// card's id. The lane never sees the move; the warning replaces
    /// it.
    func testAttemptMoveTaskRefusesGatedDrag() {
        let model = makeFixture()
        guard let dependent = model.taskByName("Phase 4") else {
            XCTFail("expected fixture"); return
        }
        let accepted = model.attemptMoveTask(dependent.id, to: .doing)
        XCTAssertFalse(accepted)
        XCTAssertEqual(model.dragRefusalNotice?.taskID, dependent.id)
        XCTAssertTrue(
            (model.dragRefusalNotice?.message ?? "").contains("gated by 1 incomplete prerequisite")
        )
    }

    /// A manual-block row with no gating edges should still be
    /// movable — the engine accepts a manual unblock once the prereq
    /// set is empty, so the kanban must not pre-empt that.
    func testAttemptMoveTaskAcceptsUngatedDragOutOfBlocked() {
        let model = makeFixture()
        model.upsertTaskForTest(
            id: "task_movable",
            name: "Movable",
            status: "blocked",
            lastStatusActor: "human"
        )
        let accepted = model.attemptMoveTask("task_movable", to: .doing)
        XCTAssertTrue(accepted)
        XCTAssertNil(model.dragRefusalNotice)
    }

    // MARK: - Fixture

    /// One product, one project, three tasks (Phase 1 done, Phase 2
    /// active, Phase 4 blocked-by-engine on Phase 2). Mirrors the
    /// shape the engine emits in `WorkTree` so the helpers exercise
    /// the same join semantics they will in production.
    private func makeFixture() -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        let productID = "prod_test"
        model.products = [
            WorkProduct(
                id: productID,
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: nil,
                status: "active",
                createdAt: "2026-05-08T00:00:00Z",
                updatedAt: "2026-05-08T00:00:00Z"
            )
        ]
        let projectID = "proj_test"
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
                    createdAt: "2026-05-08T00:00:00Z",
                    updatedAt: "2026-05-08T00:00:00Z",
                    lastStatusActor: "human"
                )
            ]
        ]
        let phase1 = WorkTask(
            id: "task_p1",
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: "Phase 1",
            description: "",
            status: "done",
            priority: "medium",
            ordinal: 1,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "human"
        )
        let phase2 = WorkTask(
            id: "task_p2",
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: "Phase 2",
            description: "",
            status: "active",
            priority: "medium",
            ordinal: 2,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "human"
        )
        let phase4 = WorkTask(
            id: "task_p4",
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: "Phase 4",
            description: "",
            status: "blocked",
            priority: "medium",
            ordinal: 4,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "engine"
        )
        model.tasksByProjectID = [projectID: [phase1, phase2, phase4]]
        model.dependenciesByProductID = [
            productID: [
                WorkItemDependency(
                    dependentID: phase4.id,
                    prerequisiteID: phase2.id,
                    relation: "blocks"
                )
            ]
        ]
        return model
    }
}

// MARK: - Test-only helpers

extension ChatViewModel {
    /// Lookup helper used by the dependency tests so each assertion
    /// can read the fixture by human-readable name without leaking
    /// generated ids into the test bodies.
    fileprivate func taskByName(_ name: String) -> WorkTask? {
        for tasks in tasksByProjectID.values {
            if let match = tasks.first(where: { $0.name == name }) {
                return match
            }
        }
        for chores in choresByProductID.values {
            if let match = chores.first(where: { $0.name == name }) {
                return match
            }
        }
        return nil
    }

    /// Inject (or replace) a task on the fixture's first project.
    /// Lets each test extend the baseline fixture with a single
    /// targeted row (manual block, ungated mover) without rebuilding
    /// the whole tree.
    fileprivate func upsertTaskForTest(
        id: String,
        name: String,
        status: String,
        lastStatusActor: String
    ) {
        guard let projectID = projectsByProductID.values.first?.first?.id,
              let productID = projectsByProductID.first?.key
        else {
            XCTFail("upsertTaskForTest called before fixture had a project")
            return
        }
        let task = WorkTask(
            id: id,
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: name,
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: lastStatusActor
        )
        var tasks = tasksByProjectID[projectID] ?? []
        if let existing = tasks.firstIndex(where: { $0.id == id }) {
            tasks[existing] = task
        } else {
            tasks.append(task)
        }
        tasksByProjectID[projectID] = tasks
    }
}
