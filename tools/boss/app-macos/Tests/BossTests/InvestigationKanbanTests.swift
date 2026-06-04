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

    // MARK: - Doc affordance derives from pr_url (T928 / merged T927)

    /// Regression anchor for T928 (reproducing the T903 vs T784 evidence):
    /// an investigation task in Review whose only doc signal is `prURL`
    /// (the bespoke `investigation_doc_*` triple is gone) must produce the
    /// exact same PR-link label as a design task carrying the same `prURL`.
    /// The card derives the doc affordance from `pr_url` for both kinds via
    /// the shared `pullRequestLinkLabel` path — there is no kind-specific
    /// branch any more.
    func testInvestigationPRLinkMatchesDesignForSamePrURL() {
        let prURL = "https://github.com/org/repo/pull/1026"
        let investigation = makeInvestigation(id: "T903", status: "in_review", prURL: prURL)
        let design = WorkTask(
            id: "T784",
            productID: "prod_test",
            projectID: "proj_test",
            kind: "design",
            name: "Design",
            description: "",
            status: "in_review",
            priority: "medium",
            ordinal: 1,
            prURL: prURL,
            deletedAt: nil,
            createdAt: "2026-05-30T00:00:00Z",
            updatedAt: "2026-05-30T00:00:00Z"
        )

        // The card's PR-link branch (`if let prURL = task.prURL`) fires for
        // both, and the derived label is identical — the link is live and
        // derived, never a stored/stale pointer.
        XCTAssertEqual(investigation.prURL, design.prURL)
        XCTAssertEqual(
            pullRequestLinkLabel(for: investigation.prURL ?? "", ambiguousRepoNames: []),
            pullRequestLinkLabel(for: design.prURL ?? "", ambiguousRepoNames: [])
        )
        XCTAssertEqual(
            pullRequestLinkLabel(for: investigation.prURL ?? "", ambiguousRepoNames: []),
            "repo#1026"
        )
    }

    /// T903 shape: an investigation with `pr_url` set and no doc-pointer
    /// fields whatsoever (the columns are dropped engine-side). The PR link
    /// must still resolve — never a blank/dead link when a doc PR exists.
    func testInvestigationWithPrURLResolvesLinkWithoutDocPointer() {
        let investigation = makeInvestigation(
            id: "T903",
            status: "in_review",
            prURL: "https://github.com/org/repo/pull/1026"
        )
        let parsed = parseGitHubPRURL(investigation.prURL ?? "")
        XCTAssertEqual(parsed?.repo, "repo")
        XCTAssertEqual(parsed?.number, "1026")
    }

    /// T927 live-refresh requirement: when the engine auto-detects the doc
    /// PR and emits an updated work tree, the app (a thin client) must pick
    /// up the new `prURL` on re-render with no reload. Driving two reception
    /// events proves the model reconverges on the engine's update.
    func testInvestigationPrURLAppearsLiveOnEngineUpdate() {
        let model = makeModel()

        // First the engine knows the investigation but the PR is not open yet.
        model.applyEventForTest(makeWorkTreeEvent(tasks: [
            makeInvestigation(id: "T903", status: "active", prURL: nil)
        ]))
        XCTAssertNil(
            model.workTask(withID: "T903")?.prURL,
            "no PR yet → no link source"
        )

        // The worker opens the doc PR; the engine auto-detects `pr_url` and
        // re-broadcasts. The app must now carry the link source — no restart.
        model.applyEventForTest(makeWorkTreeEvent(tasks: [
            makeInvestigation(
                id: "T903",
                status: "in_review",
                prURL: "https://github.com/org/repo/pull/1026"
            )
        ]))
        XCTAssertEqual(
            model.workTask(withID: "T903")?.prURL,
            "https://github.com/org/repo/pull/1026",
            "engine-detected pr_url must appear live without an app reload"
        )
    }

    /// RECURRENCE LOCK for the render surface (T1310). An investigation
    /// whose doc PR is open arrives over the work tree as `status ==
    /// "in_review"` carrying `prURL`. Two things must hold for the Review-lane
    /// doc link to appear, and both have silently broken before:
    ///   1. the card routes to the Review column (status → board column), and
    ///   2. the card's PR-link branch (`if let prURL = task.prURL`) has a
    ///      live, non-empty source — there is no kind gate, so it derives
    ///      identically to a design task with the same PR.
    /// This drives the real reception path and pins both, so a future change
    /// that mis-routes in_review investigations OR kind-gates the PR link
    /// fails here instead of shipping a sixth silent regression.
    func testInReviewInvestigationRoutesToReviewAndExposesDocLink() {
        let model = makeModel()
        let prURL = "https://github.com/spinyfin/mono/pull/1324"
        model.applyEventForTest(makeWorkTreeEvent(tasks: [
            makeInvestigation(id: "T1310", status: "in_review", prURL: prURL)
        ]))

        // (1) Routes to Review — not stranded in Doing/Backlog.
        XCTAssertTrue(
            model.workItems(in: .review).contains { $0.id == "T1310" },
            "an investigation with an open doc PR must land in the Review column"
        )

        // (2) The doc-link source is present and derivable. The card's
        // PR-link branch fires on a non-empty prURL and renders this label.
        let task = model.workTask(withID: "T1310")
        XCTAssertEqual(task?.prURL, prURL, "the live pr_url is the sole doc-link source")
        XCTAssertEqual(
            pullRequestLinkLabel(for: task?.prURL ?? "", ambiguousRepoNames: []),
            "mono#1324",
            "the Review-lane card must expose a derived doc link, never a blank affordance"
        )
        XCTAssertNotNil(parseGitHubPRURL(task?.prURL ?? ""))
    }

    /// The doc link is strictly pr_url-derived: before the worker opens the
    /// PR there is simply no source, so no link is exposed. This pins the
    /// "live derived, never a stale stored pointer" contract (T928) — the
    /// affordance must appear exactly when `pr_url` does, no sooner.
    func testInvestigationWithoutPrURLExposesNoDocLink() {
        let model = makeModel()
        model.applyEventForTest(makeWorkTreeEvent(tasks: [
            makeInvestigation(id: "T1310", status: "active", prURL: nil)
        ]))
        let task = model.workTask(withID: "T1310")
        XCTAssertNil(task?.prURL, "no PR yet → no doc-link source → no link")
    }

    // MARK: - Helpers

    private func makeInvestigation(
        id: String,
        status: String,
        kind: String = "investigation",
        prURL: String? = nil
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
            prURL: prURL,
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
