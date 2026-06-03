import XCTest
@testable import Boss

/// Regression coverage for the bug where chores with `created_via = engine_auto`
/// (produced by automation triage) were silently dropped from the kanban board.
///
/// Root cause: `computeVisibleWorkItems` filtered out ALL items with
/// `sourceAutomationId != nil` under the assumption they were "managed by
/// the Automations tab." In practice, automation-created chores progress to
/// `in_review` status where the operator must see and review the PR. The
/// filter made them invisible everywhere.
///
/// Fix: remove the `sourceAutomationId == nil` filter. Automation-sourced
/// chores appear on the kanban; the card detail shows a purple wand icon to
/// mark their provenance.
///
/// Coverage also ensures that an arbitrary unknown `created_via` string does
/// not cause a chore to vanish — the field is a plain String with no
/// exhaustive decode, so new provenance values are forward-compatible.
@MainActor
final class EngineAutoChoreKanbanTests: XCTestCase {

    // MARK: - engine_auto provenance

    func testEngineAutoChoreInReviewAppearsOnBoard() {
        let model = makeModel()
        let chore = makeChore(
            id: "task_1194",
            status: "in_review",
            createdVia: "engine_auto",
            sourceAutomationId: "auto_abc"
        )
        model.applyEventForTest(makeWorkTreeEvent(chores: [chore]))

        XCTAssertTrue(
            model.visibleWorkItems.contains { $0.id == "task_1194" },
            "engine_auto chore in in_review must appear in visible board items"
        )
    }

    func testEngineAutoChoreInReviewAppearsInReviewColumn() {
        let model = makeModel()
        let chore = makeChore(
            id: "task_1195",
            status: "in_review",
            createdVia: "engine_auto",
            sourceAutomationId: "auto_abc"
        )
        model.applyEventForTest(makeWorkTreeEvent(chores: [chore]))

        XCTAssertTrue(
            model.workItems(in: .review).contains { $0.id == "task_1195" },
            "engine_auto chore in in_review must route to the Review lane"
        )
    }

    func testEngineAutoChoreActiveAppearsOnBoard() {
        let model = makeModel()
        let chore = makeChore(
            id: "task_1196",
            status: "active",
            createdVia: "engine_auto",
            sourceAutomationId: "auto_abc"
        )
        model.applyEventForTest(makeWorkTreeEvent(chores: [chore]))

        XCTAssertTrue(
            model.visibleWorkItems.contains { $0.id == "task_1196" },
            "engine_auto chore that is active must appear on the kanban so operators can track progress"
        )
    }

    // MARK: - Forward-compatibility: unknown future provenance

    /// A chore with an unrecognised `created_via` value must not disappear.
    /// `createdVia` is a plain String — no exhaustive enum — so any future
    /// value decodes and renders without a code change.
    func testUnknownProvenanceChoreAppearsOnBoard() {
        let model = makeModel()
        let chore = makeChore(
            id: "task_future",
            status: "in_review",
            createdVia: "some_new_surface_not_yet_known",
            sourceAutomationId: nil
        )
        model.applyEventForTest(makeWorkTreeEvent(chores: [chore]))

        XCTAssertTrue(
            model.visibleWorkItems.contains { $0.id == "task_future" },
            "a chore with an unknown created_via string must still render — no exhaustive decode"
        )
    }

    /// A chore with `sourceAutomationId` set AND an unknown provenance string
    /// must also appear (forward-compat for future automation provenance values).
    func testUnknownProvenanceChoreWithSourceAutomationIdAppearsOnBoard() {
        let model = makeModel()
        let chore = makeChore(
            id: "task_future2",
            status: "in_review",
            createdVia: "hypothetical_engine_v2",
            sourceAutomationId: "auto_xyz"
        )
        model.applyEventForTest(makeWorkTreeEvent(chores: [chore]))

        XCTAssertTrue(
            model.visibleWorkItems.contains { $0.id == "task_future2" },
            "a chore with sourceAutomationId and unknown created_via must render on the kanban"
        )
    }

    // MARK: - Helpers

    private func makeChore(
        id: String,
        status: String,
        createdVia: String,
        sourceAutomationId: String?
    ) -> WorkTask {
        var chore = WorkTask(
            id: id,
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "Automation chore \(id)",
            description: "Auto-generated maintenance chore",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: status == "in_review" ? "https://github.com/org/repo/pull/99" : nil,
            deletedAt: nil,
            createdAt: "2026-06-01T00:00:00Z",
            updatedAt: "2026-06-01T00:00:00Z"
        )
        chore.createdVia = createdVia
        chore.sourceAutomationId = sourceAutomationId
        return chore
    }

    private func makeWorkTreeEvent(chores: [WorkTask] = []) -> EngineEvent {
        .workTree(
            product: WorkProduct(
                id: "prod_test",
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: "https://github.com/org/repo.git",
                status: "active",
                createdAt: "2026-06-01T00:00:00Z",
                updatedAt: "2026-06-01T00:00:00Z"
            ),
            projects: [],
            tasks: [],
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
                createdAt: "2026-06-01T00:00:00Z",
                updatedAt: "2026-06-01T00:00:00Z"
            )
        ]
        model.includeChores = true
        model.selectWorkProduct("prod_test")
        return model
    }
}
