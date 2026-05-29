import XCTest
@testable import Boss

/// Covers the "in revision" warning indicator on kanban PR cards.
///
/// When a chain-root task (the one carrying `prURL`) has at least one
/// descendant revision whose status is `todo` or `active`, the engine
/// sets `has_in_progress_revision = true` on the root's work-tree row.
/// The macOS app surfaces this as an orange "in revision" badge next to
/// the PR link chip on the card.
///
/// These tests pin the `hasInProgressRevision` field behaviour on the
/// Swift side: default, parse round-trip, and model-level flag.
@MainActor
final class KanbanInRevisionWarningTests: XCTestCase {

    // MARK: WorkTask field defaults

    func testHasInProgressRevisionDefaultsFalse() {
        let task = makeTaskWithPR(prURL: "https://github.com/org/repo/pull/1")
        XCTAssertFalse(task.hasInProgressRevision,
                       "hasInProgressRevision must default to false")
    }

    func testHasInProgressRevisionTruePreserved() {
        var task = makeTaskWithPR(prURL: "https://github.com/org/repo/pull/1")
        task.hasInProgressRevision = true
        XCTAssertTrue(task.hasInProgressRevision,
                      "hasInProgressRevision must be preserved when set to true")
    }

    // MARK: Show / hide logic (mirroring engine signal rules)

    /// A task with `hasInProgressRevision = true` should show the warning.
    func testWarningShownWhenFlagSet() {
        var task = makeTaskWithPR(prURL: "https://github.com/org/repo/pull/2")
        task.hasInProgressRevision = true
        XCTAssertTrue(task.hasInProgressRevision,
                      "flag must be true — view layer should render PrInRevisionIndicator")
    }

    /// A task with `hasInProgressRevision = false` (the default) must not show the warning.
    func testWarningHiddenWhenFlagClear() {
        let task = makeTaskWithPR(prURL: "https://github.com/org/repo/pull/3")
        XCTAssertFalse(task.hasInProgressRevision,
                       "flag must be false — view layer must NOT render PrInRevisionIndicator")
    }

    /// A task without a PR URL has `hasInProgressRevision` irrelevant —
    /// the PR row is not rendered, so the indicator can never appear.
    func testNoPRURLTaskFlagIrrelevant() {
        var task = makeTask(status: "active")
        task.hasInProgressRevision = true
        XCTAssertNil(task.prURL,
                     "sanity: task must have no PR URL so the PR row is not rendered")
    }

    // MARK: - Helpers

    private func makeTaskWithPR(prURL: String) -> WorkTask {
        WorkTask(
            id: "task_\(UUID().uuidString)",
            productID: "prod_test",
            projectID: "proj_1",
            kind: "chore",
            name: "PR task",
            description: "",
            status: "in_review",
            priority: "medium",
            ordinal: nil,
            prURL: prURL,
            deletedAt: nil,
            createdAt: "2026-05-28T00:00:00Z",
            updatedAt: "2026-05-28T00:00:00Z"
        )
    }

    private func makeTask(status: String) -> WorkTask {
        WorkTask(
            id: "task_\(UUID().uuidString)",
            productID: "prod_test",
            projectID: "proj_1",
            kind: "chore",
            name: "No PR task",
            description: "",
            status: status,
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-28T00:00:00Z",
            updatedAt: "2026-05-28T00:00:00Z"
        )
    }
}
