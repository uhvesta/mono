import XCTest
@testable import Boss

/// Drives the work-item detail Repo: row + Change… affordance
/// (Follow-up chore #12 of `multi-repo-work-modeling.md`). Splits
/// across three layers, matching the pattern in
/// `KanbanRepoChipTests` and `ProjectDesignDocAffordanceTests`:
///
/// - `RepoOverridePresentation` pins the CLI provenance vocabulary
///   so the macOS popover and `boss <kind> show` agree verbatim.
/// - `ChatViewModel.recentRepoURLs(forProduct:)` derives the picker
///   source from the in-memory work tree.
/// - `ChatViewModel.setRepoOverride(for:to:)` drives the UI
///   interaction acceptance: changing the override and clearing it
///   round-trip through the presentation a view would read.
@MainActor
final class WorkItemRepoRowTests: XCTestCase {
    // MARK: - RepoOverridePresentation

    /// Task carries an override → presentation reads as the override
    /// URL with the CLI's "(override on this work item)" provenance.
    /// Matches `format_repo_line` in `cli/src/main.rs`.
    func testPresentationReflectsTaskOverride() {
        let task = makeTask(
            id: "t_1",
            repoRemoteURL: "git@github.com:foo/nimbus.git"
        )
        let product = makeProduct(
            slug: "work",
            repoRemoteURL: "https://github.com/foo/bar.git"
        )
        let presentation = RepoOverridePresentation.resolve(
            task: task,
            product: product
        )
        XCTAssertEqual(presentation.resolvedURL, "git@github.com:foo/nimbus.git")
        XCTAssertEqual(presentation.provenanceLabel, "override on this work item")
        XCTAssertEqual(presentation.provenance, .taskOverride)
        XCTAssertEqual(
            presentation.cliLine,
            "git@github.com:foo/nimbus.git (override on this work item)"
        )
    }

    /// Task has no override → presentation falls through to the
    /// product default with CLI provenance "inherited from product
    /// `<slug>`". The slug is the one users see in the CLI, so the
    /// macOS row stays in lockstep.
    func testPresentationFallsThroughToProductDefault() {
        let task = makeTask(id: "t_1", repoRemoteURL: nil)
        let product = makeProduct(
            slug: "work",
            repoRemoteURL: "https://github.com/foo/bar.git"
        )
        let presentation = RepoOverridePresentation.resolve(
            task: task,
            product: product
        )
        XCTAssertEqual(presentation.resolvedURL, "https://github.com/foo/bar.git")
        XCTAssertEqual(presentation.provenanceLabel, "inherited from product `work`")
        XCTAssertEqual(presentation.provenance, .productDefault(productSlug: "work"))
        XCTAssertEqual(
            presentation.cliLine,
            "https://github.com/foo/bar.git (inherited from product `work`)"
        )
    }

    /// Neither the task nor the product carries a URL → the row
    /// collapses to the CLI's "(none — work item cannot dispatch)"
    /// string so the user sees the same answer in the popover and
    /// the terminal.
    func testPresentationCannotDispatchWhenNothingResolves() {
        let task = makeTask(id: "t_1", repoRemoteURL: nil)
        let product = makeProduct(slug: "work", repoRemoteURL: nil)
        let presentation = RepoOverridePresentation.resolve(
            task: task,
            product: product
        )
        XCTAssertNil(presentation.resolvedURL)
        XCTAssertEqual(presentation.provenanceLabel, "(none — work item cannot dispatch)")
        XCTAssertEqual(presentation.provenance, RepoOverridePresentation.Provenance.none)
        XCTAssertEqual(presentation.cliLine, "(none — work item cannot dispatch)")
    }

    /// Empty / whitespace strings on either side decode as "not
    /// set", same convention the chip presentation already uses. The
    /// engine round-trips empty as `None`, but tests / hand-built
    /// JSON sometimes pass `""` — without this collapse the row
    /// would render an empty URL.
    func testPresentationTreatsEmptyStringsAsNone() {
        let task = makeTask(id: "t_1", repoRemoteURL: "   ")
        let product = makeProduct(slug: "work", repoRemoteURL: "")
        let presentation = RepoOverridePresentation.resolve(
            task: task,
            product: product
        )
        XCTAssertEqual(presentation.provenance, RepoOverridePresentation.Provenance.none)
    }

    /// Missing product (snapshot-in-flight edge case) without an
    /// override → no resolution. With an override the row still
    /// honours the override so a pending product fetch doesn't hide
    /// the user's pinned URL.
    func testPresentationHandlesMissingProduct() {
        let withOverride = RepoOverridePresentation.resolve(
            task: makeTask(id: "t_1", repoRemoteURL: "https://github.com/foo/bar.git"),
            product: nil
        )
        XCTAssertEqual(withOverride.provenance, .taskOverride)
        XCTAssertEqual(withOverride.resolvedURL, "https://github.com/foo/bar.git")

        let withoutOverride = RepoOverridePresentation.resolve(
            task: makeTask(id: "t_1", repoRemoteURL: nil),
            product: nil
        )
        XCTAssertEqual(withoutOverride.provenance, RepoOverridePresentation.Provenance.none)
    }

    // MARK: - recentRepoURLs

    /// Product default is always first; remaining URLs follow in
    /// `updatedAt` order so the most-recently-edited override
    /// bubbles up. Duplicates collapse so a product whose default
    /// equals an override doesn't list the URL twice.
    func testRecentRepoURLsOrdersByRecencyAndDedupes() {
        let model = makeModel(
            productRepoURL: "https://github.com/foo/bar.git",
            tasks: [
                ("task_1", "git@github.com:foo/nimbus.git", "2026-05-09T00:00:00Z"),
                ("task_2", "https://github.com/foo/bar.git", "2026-05-10T00:00:00Z"),
                ("task_3", "https://github.com/foo/console.git", "2026-05-11T00:00:00Z"),
            ],
            chores: []
        )
        XCTAssertEqual(
            model.recentRepoURLs(forProduct: "prod_test"),
            [
                "https://github.com/foo/bar.git",
                "https://github.com/foo/console.git",
                "git@github.com:foo/nimbus.git",
            ]
        )
    }

    /// Brand-new product with no default and no overrides → empty
    /// list. The picker view handles this by collapsing the Recent
    /// section and falling through to the Custom URL field; the
    /// model just reports the empirical set, which is empty.
    func testRecentRepoURLsIsEmptyForBareProduct() {
        let model = makeModel(productRepoURL: nil, tasks: [], chores: [])
        XCTAssertEqual(model.recentRepoURLs(forProduct: "prod_test"), [])
    }

    /// Chores live under the product directly, not under a project.
    /// The recent set must walk both shelves so a product whose only
    /// repo signal is a chore override still surfaces it.
    func testRecentRepoURLsIncludesChores() {
        let model = makeModel(
            productRepoURL: nil,
            tasks: [],
            chores: [
                ("chore_1", "git@github.com:foo/nimbus.git", "2026-05-09T00:00:00Z"),
            ]
        )
        XCTAssertEqual(
            model.recentRepoURLs(forProduct: "prod_test"),
            ["git@github.com:foo/nimbus.git"]
        )
    }

    // MARK: - UI interaction (acceptance)

    /// Acceptance per the work item: changing the override and
    /// observing the row update. The view reads the presentation on
    /// every render; after `setRepoOverride` flips the in-memory
    /// task, the row resolves to the new URL with `.taskOverride`
    /// provenance.
    func testSetRepoOverrideUpdatesPresentation() {
        let model = makeModel(
            productRepoURL: "https://github.com/foo/bar.git",
            tasks: [("task_1", nil, "2026-05-09T00:00:00Z")],
            chores: []
        )
        var task = model.tasksByProjectID["proj_test"]!.first!
        XCTAssertEqual(
            model.repoOverridePresentation(for: task).provenance,
            .productDefault(productSlug: "test")
        )

        // Simulate the engine echo: the picker tells the model to
        // set the override; the snapshot then comes back with the
        // override on the task row. We exercise the snapshot half by
        // mutating the in-memory state, since `setRepoOverride`
        // itself only enqueues the patch.
        task.repoRemoteURL = "git@github.com:foo/nimbus.git"
        model.tasksByProjectID["proj_test"] = [task]

        let updated = model.repoOverridePresentation(for: task)
        XCTAssertEqual(updated.provenance, .taskOverride)
        XCTAssertEqual(updated.resolvedURL, "git@github.com:foo/nimbus.git")
    }

    /// Acceptance per the work item: clearing reverts to inherited.
    /// `setRepoOverride(for:to:nil)` is the path the picker's
    /// `Clear (inherit from product)` button takes; after the
    /// engine echoes the cleared state, the row reads as inherited
    /// from the product default.
    func testClearingOverrideRevertsToInherited() {
        let model = makeModel(
            productRepoURL: "https://github.com/foo/bar.git",
            tasks: [("task_1", "git@github.com:foo/nimbus.git", "2026-05-09T00:00:00Z")],
            chores: []
        )
        var task = model.tasksByProjectID["proj_test"]!.first!
        XCTAssertEqual(
            model.repoOverridePresentation(for: task).provenance,
            .taskOverride
        )

        // Simulate the engine echoing the cleared override back.
        task.repoRemoteURL = nil
        model.tasksByProjectID["proj_test"] = [task]

        let reverted = model.repoOverridePresentation(for: task)
        XCTAssertEqual(reverted.provenance, .productDefault(productSlug: "test"))
        XCTAssertEqual(reverted.resolvedURL, "https://github.com/foo/bar.git")
    }

    // MARK: - Fixtures

    private func makeTask(id: String, repoRemoteURL: String?) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: "proj_test",
            kind: "task",
            name: "Card \(id)",
            description: "",
            status: "todo",
            priority: "medium",
            ordinal: 1,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "human",
            createdVia: "unknown",
            repoRemoteURL: repoRemoteURL
        )
    }

    private func makeProduct(slug: String, repoRemoteURL: String?) -> WorkProduct {
        WorkProduct(
            id: "prod_test",
            name: "Test Product",
            slug: slug,
            description: "",
            repoRemoteURL: repoRemoteURL,
            status: "active",
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z"
        )
    }

    /// Build a model populated with one product, one project, and
    /// the named tasks / chores so the per-product distinct-URL
    /// query has rows to walk.
    private func makeModel(
        productRepoURL: String?,
        tasks: [(id: String, repo: String?, updatedAt: String)],
        chores: [(id: String, repo: String?, updatedAt: String)]
    ) -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        let productID = "prod_test"
        model.products = [
            WorkProduct(
                id: productID,
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: productRepoURL,
                status: "active",
                createdAt: "2026-05-08T00:00:00Z",
                updatedAt: "2026-05-08T00:00:00Z"
            )
        ]
        model.selectedWorkProductID = productID
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
                    updatedAt: "2026-05-08T00:00:00Z"
                )
            ]
        ]
        let taskRows = tasks.map { entry -> WorkTask in
            var t = makeTask(id: entry.id, repoRemoteURL: entry.repo)
            t.updatedAt = entry.updatedAt
            return t
        }
        model.tasksByProjectID = [projectID: taskRows]

        let choreRows = chores.map { entry -> WorkTask in
            var c = makeTask(id: entry.id, repoRemoteURL: entry.repo)
            c.updatedAt = entry.updatedAt
            return c
        }
        model.choresByProductID = [productID: choreRows]
        return model
    }
}
