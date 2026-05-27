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

    /// Default grouping (`.none`) renders the project badge on the
    /// card so the reader can tell which project a task belongs to
    /// without expanding it.
    func testCardProjectBadgeShownWhenUngrouped() {
        let model = makeFixture()
        model.workBoardGrouping = .none
        guard let task = model.taskByName("Phase 2") else {
            XCTFail("expected fixture"); return
        }
        XCTAssertEqual(model.cardProjectBadge(for: task), "Test Project")
    }

    /// Grouping by project promotes the project name to the lane
    /// header, so the per-card badge would just duplicate it. The
    /// helper must suppress it across every card in that mode.
    func testCardProjectBadgeHiddenWhenGroupedByProject() {
        let model = makeFixture()
        model.workBoardGrouping = .project
        guard let task = model.taskByName("Phase 2") else {
            XCTFail("expected fixture"); return
        }
        XCTAssertNil(model.cardProjectBadge(for: task))
    }

    /// Chores have no project so the badge was already absent; the
    /// helper must hold that line regardless of grouping mode.
    func testCardProjectBadgeAlwaysNilForChores() {
        let model = makeFixture()
        let productID = model.products.first?.id ?? "prod_test"
        let chore = WorkTask(
            id: "chore_test",
            productID: productID,
            projectID: nil,
            kind: "chore",
            name: "Tidy",
            description: "",
            status: "active",
            priority: "medium",
            ordinal: 1,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "human"
        )
        model.choresByProductID = [productID: [chore]]
        model.workBoardGrouping = .none
        XCTAssertNil(model.cardProjectBadge(for: chore))
        model.workBoardGrouping = .project
        XCTAssertNil(model.cardProjectBadge(for: chore))
    }

    /// An active task with `blocked_reason=dependency` must surface its
    /// prereqs via `dependencyPrereqs` so the card can render "Waiting
    /// on: <name>". This mirrors the real repro: `status=active`,
    /// `blocked_reason=dependency` — the engine left the field set after
    /// the last block evaluation.
    func testActiveTaskWithBlockedReasonDependencyExposesPrereqs() {
        let model = makeFixture()
        guard let dependent = model.taskByName("Phase 4") else {
            XCTFail("expected fixture to include the gated task"); return
        }
        // Phase 4 is status=blocked in the fixture; inject an active
        // clone with the same dependency edge to exercise the
        // non-blocked-status path.
        let productID = model.products.first?.id ?? "prod_test"
        let projectID = model.projectsByProductID[productID]?.first?.id ?? "proj_test"
        let activeGated = WorkTask(
            id: "task_active_gated",
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: "Active But Gated",
            description: "",
            status: "active",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "engine",
            blockedReason: "dependency"
        )
        var tasks = model.tasksByProjectID[projectID] ?? []
        tasks.append(activeGated)
        model.tasksByProjectID[projectID] = tasks
        model.dependenciesByProductID[productID, default: []].append(
            WorkItemDependency(
                dependentID: activeGated.id,
                prerequisiteID: dependent.id,
                relation: "blocks"
            )
        )
        // dependencyPrereqs drives the "Waiting on:" subtitle — must
        // include the prereq even when the card status is not "blocked".
        let prereqs = model.dependencyPrereqs(for: activeGated.id)
        XCTAssertEqual(prereqs.map(\.title), ["Phase 4"])
    }

    /// Stale dependency block: `blocked_reason=dependency` is set but
    /// the sole prereq is already done. `dependencyPrereqs` must still
    /// return the prereq row (for the "Waiting on:" label) even though
    /// `gatingPrereqs` returns empty (nothing incomplete). The card
    /// uses the full set so the stale block is still visible.
    func testStaleDependencyBlockStillExposesPrereqsForCard() {
        let model = makeFixture()
        guard let done = model.taskByName("Phase 1") else {
            XCTFail("expected fixture"); return
        }
        let productID = model.products.first?.id ?? "prod_test"
        let projectID = model.projectsByProductID[productID]?.first?.id ?? "proj_test"
        let staleGated = WorkTask(
            id: "task_stale_gated",
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: "Stale Gated",
            description: "",
            status: "active",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "engine",
            blockedReason: "dependency"
        )
        var tasks = model.tasksByProjectID[projectID] ?? []
        tasks.append(staleGated)
        model.tasksByProjectID[projectID] = tasks
        model.dependenciesByProductID[productID, default: []].append(
            WorkItemDependency(
                dependentID: staleGated.id,
                prerequisiteID: done.id,
                relation: "blocks"
            )
        )
        // Phase 1 is done — no gating prereqs, but the edge still exists.
        XCTAssertEqual(model.gatingPrereqs(for: staleGated.id), [])
        // The full prereq list is non-empty — the card must show "Waiting on: Phase 1".
        let allPrereqs = model.dependencyPrereqs(for: staleGated.id)
        XCTAssertEqual(allPrereqs.map(\.title), ["Phase 1"])
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

    // MARK: - Dependency badge frontier

    /// Phase 4 is blocked by Phase 2 (active, no gating prereqs).
    /// The frontier for Phase 4 is exactly {Phase 2} — it is
    /// reachable, unblocked, and open.
    func testFrontierForDirectlyBlockedTask() {
        let model = makeFixture()
        guard let phase2 = model.taskByName("Phase 2"),
              let phase4 = model.taskByName("Phase 4")
        else {
            XCTFail("expected fixture tasks"); return
        }
        let frontier = model.actionablePrereqFrontier(for: phase4.id)
        XCTAssertEqual(frontier, [phase2.id])
    }

    /// Phase 1 is already done, so it is not open. The frontier for
    /// a task blocked only by Phase 1 must be empty (nothing actionable).
    func testFrontierExcludesTerminalPrereqs() {
        let model = makeFixture()
        guard let phase1 = model.taskByName("Phase 1") else {
            XCTFail("expected fixture"); return
        }
        let productID = model.products.first?.id ?? "prod_test"
        let projectID = model.projectsByProductID[productID]?.first?.id ?? "proj_test"
        let staleBlocked = WorkTask(
            id: "task_stale",
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: "Stale Blocked",
            description: "",
            status: "blocked",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "engine"
        )
        var tasks = model.tasksByProjectID[projectID] ?? []
        tasks.append(staleBlocked)
        model.tasksByProjectID[projectID] = tasks
        model.dependenciesByProductID[productID, default: []].append(
            WorkItemDependency(dependentID: staleBlocked.id, prerequisiteID: phase1.id, relation: "blocks")
        )
        let frontier = model.actionablePrereqFrontier(for: staleBlocked.id)
        XCTAssertTrue(frontier.isEmpty, "frontier must be empty when the only prereq is already done")
    }

    /// Three-deep chain: Chore → A (blocked) → B (active, unblocked).
    /// The frontier for Chore is {B} — A is blocked so it is not
    /// actionable yet; B is the reachable leaf that is open and unblocked.
    func testFrontierWalksThroughBlockedIntermediateNodes() {
        let model = makeFixture()
        let productID = model.products.first?.id ?? "prod_test"
        let projectID = model.projectsByProductID[productID]?.first?.id ?? "proj_test"

        let taskB = WorkTask(
            id: "task_b",
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: "Task B",
            description: "",
            status: "active",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "human"
        )
        let taskA = WorkTask(
            id: "task_a",
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: "Task A",
            description: "",
            status: "blocked",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "engine"
        )
        let chore = WorkTask(
            id: "chore_c",
            productID: productID,
            projectID: nil,
            kind: "chore",
            name: "Chore C",
            description: "",
            status: "blocked",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "engine"
        )
        var tasks = model.tasksByProjectID[projectID] ?? []
        tasks.append(contentsOf: [taskB, taskA])
        model.tasksByProjectID[projectID] = tasks
        model.choresByProductID[productID, default: []].append(chore)
        model.dependenciesByProductID[productID, default: []] += [
            WorkItemDependency(dependentID: taskA.id, prerequisiteID: taskB.id, relation: "blocks"),
            WorkItemDependency(dependentID: chore.id, prerequisiteID: taskA.id, relation: "blocks"),
        ]
        let frontier = model.actionablePrereqFrontier(for: chore.id)
        // Task A is blocked (not unblocked), so it is not in the frontier.
        // Task B is active, unblocked — it is the actionable frontier.
        XCTAssertEqual(frontier, [taskB.id])
    }

    /// `setDepBadgeHover` populates `depFrontierHighlightIDs` on enter
    /// and clears it on leave (nil).
    func testSetDepBadgeHoverPopulatesAndClearsHighlightSet() {
        let model = makeFixture()
        guard let phase2 = model.taskByName("Phase 2"),
              let phase4 = model.taskByName("Phase 4")
        else {
            XCTFail("expected fixture tasks"); return
        }
        XCTAssertTrue(model.depFrontierHighlightIDs.isEmpty, "should start empty")
        model.setDepBadgeHover(phase4.id)
        XCTAssertEqual(model.depFrontierHighlightIDs, [phase2.id], "phase2 is the frontier for phase4")
        model.setDepBadgeHover(nil)
        XCTAssertTrue(model.depFrontierHighlightIDs.isEmpty, "should clear on nil")
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
