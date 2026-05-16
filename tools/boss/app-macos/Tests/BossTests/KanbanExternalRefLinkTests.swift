import XCTest
@testable import Boss

/// Covers the kanban card upstream-link affordance (chore 15 of
/// `external-issue-tracker-sync-github-projects.md`). Three states:
///
/// - **Bound** — `externalRef` present, `unboundAt == nil`. Shows `↗ #N`.
/// - **Unbound** — no `externalRef`. No affordance rendered (`nil`).
/// - **Unbound-with-stale-ref** — `externalRef` present, `unboundAt != nil`.
///   Shows `↗ #N` with stale treatment.
///
/// The tests validate `ExternalRefLinkPresentation.forTask` — the pure-data
/// layer that the thin SwiftUI view reflects — so no host app is required.
@MainActor
final class KanbanExternalRefLinkTests: XCTestCase {

    // MARK: - issueLabel

    func testIssueLabelExtractsHashFragment() {
        XCTAssertEqual(
            ExternalRefLinkPresentation.issueLabel(from: "spinyfin/mono#560"),
            "↗ #560"
        )
    }

    func testIssueLabelHandlesNumericOnlyFragment() {
        XCTAssertEqual(
            ExternalRefLinkPresentation.issueLabel(from: "org/repo#1"),
            "↗ #1"
        )
    }

    func testIssueLabelFallsBackWhenNoHash() {
        XCTAssertEqual(
            ExternalRefLinkPresentation.issueLabel(from: "jira-BOSS-42"),
            "↗ jira-BOSS-42"
        )
    }

    // MARK: - bound state

    func testBoundTaskReturnsPresentationWithLabel() {
        let task = makeTask(externalRef: makeRef())
        let p = ExternalRefLinkPresentation.forTask(task)
        XCTAssertNotNil(p)
        XCTAssertEqual(p?.label, "↗ #560")
    }

    func testBoundTaskCarriesWebURL() {
        let task = makeTask(externalRef: makeRef())
        let p = ExternalRefLinkPresentation.forTask(task)
        XCTAssertEqual(p?.url, "https://github.com/spinyfin/mono/issues/560")
    }

    func testBoundTaskIsNotStale() {
        let task = makeTask(externalRef: makeRef())
        let p = ExternalRefLinkPresentation.forTask(task)
        XCTAssertEqual(p?.isStale, false)
    }

    func testBoundTaskTooltipContainsCanonicalID() {
        let task = makeTask(externalRef: makeRef())
        let p = ExternalRefLinkPresentation.forTask(task)
        XCTAssertTrue(
            p?.tooltip.contains("spinyfin/mono#560") ?? false,
            "tooltip should include canonical ID; got: \(p?.tooltip ?? "(nil)")"
        )
    }

    func testBoundTaskTooltipIncludesSyncTime() {
        let ref = makeRef(syncedAt: "1747000100", unboundAt: nil)
        let task = makeTask(externalRef: ref)
        let p = ExternalRefLinkPresentation.forTask(task)
        XCTAssertTrue(
            p?.tooltip.contains("1747000100") ?? false,
            "tooltip should include syncedAt; got: \(p?.tooltip ?? "(nil)")"
        )
    }

    func testBoundTaskTooltipOmitsSyncTimeWhenNil() {
        let ref = makeRef(syncedAt: nil, unboundAt: nil)
        let task = makeTask(externalRef: ref)
        let p = ExternalRefLinkPresentation.forTask(task)
        XCTAssertFalse(
            p?.tooltip.contains("Last synced") ?? true,
            "tooltip should not mention sync time when syncedAt is nil"
        )
    }

    // MARK: - unbound state (no externalRef)

    func testUnboundTaskReturnsNil() {
        let task = makeTask(externalRef: nil)
        XCTAssertNil(ExternalRefLinkPresentation.forTask(task))
    }

    // MARK: - unbound-with-stale-ref state

    func testStaleRefTaskReturnsPresentationWithStaleFlag() {
        let ref = makeRef(syncedAt: "1747000100", unboundAt: "1747001000")
        let task = makeTask(externalRef: ref)
        let p = ExternalRefLinkPresentation.forTask(task)
        XCTAssertNotNil(p)
        XCTAssertEqual(p?.isStale, true)
    }

    func testStaleRefTaskLabelMatchesBoundLabel() {
        let ref = makeRef(syncedAt: "1747000100", unboundAt: "1747001000")
        let task = makeTask(externalRef: ref)
        let p = ExternalRefLinkPresentation.forTask(task)
        XCTAssertEqual(p?.label, "↗ #560",
            "label must remain navigable even when stale")
    }

    func testStaleRefTaskStillCarriesWebURL() {
        let ref = makeRef(syncedAt: "1747000100", unboundAt: "1747001000")
        let task = makeTask(externalRef: ref)
        let p = ExternalRefLinkPresentation.forTask(task)
        XCTAssertEqual(p?.url, "https://github.com/spinyfin/mono/issues/560",
            "stale ref must still expose the web URL so the user can navigate to the (now-removed) upstream")
    }

    func testStaleRefTaskTooltipMentionsBindingCleared() {
        let ref = makeRef(syncedAt: "1747000100", unboundAt: "1747001000")
        let task = makeTask(externalRef: ref)
        let p = ExternalRefLinkPresentation.forTask(task)
        XCTAssertTrue(
            p?.tooltip.lowercased().contains("cleared") ?? false,
            "stale tooltip must mention the binding was cleared; got: \(p?.tooltip ?? "(nil)")"
        )
    }

    // MARK: - Fixtures

    private func makeRef(
        syncedAt: String? = "1747000100",
        unboundAt: String? = nil
    ) -> WorkItemExternalRef {
        WorkItemExternalRef(
            kind: "github",
            canonicalID: "spinyfin/mono#560",
            raw: "{}",
            webURL: "https://github.com/spinyfin/mono/issues/560",
            syncedAt: syncedAt,
            unboundAt: unboundAt
        )
    }

    private func makeTask(externalRef: WorkItemExternalRef?) -> WorkTask {
        WorkTask(
            id: "task_extref_test",
            productID: "prod_test",
            projectID: nil,
            kind: "chore",
            name: "External ref test",
            description: "",
            status: "todo",
            priority: "medium",
            ordinal: nil,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-16T00:00:00Z",
            updatedAt: "2026-05-16T00:00:00Z",
            externalRef: externalRef
        )
    }
}
