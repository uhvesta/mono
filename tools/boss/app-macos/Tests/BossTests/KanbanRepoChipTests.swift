import XCTest
@testable import Boss

/// Drives the kanban repo chip across the two modes called out in
/// design Q7 of `multi-repo-work-modeling.md` and the work item
/// description for "macOS: kanban card repo chip":
///
/// - **Single-repo product**: every visible card resolves to the same
///   URL. The chip lifts to the product header; per-card chips are
///   suppressed.
/// - **Multi-repo product**: any card carries an override (or the
///   product has no default but cards do). The chip appears on every
///   card with override-vs-inherited provenance in the tooltip.
///
/// Like `ProjectDesignDocAffordanceTests`, this file pins the
/// pure-data layer (mode + presentation) so the SwiftUI view stays a
/// thin reflection of values an XCTest can assert without spinning
/// up a host. Mode + chip computations are validated end-to-end
/// against `ChatViewModel` so the wiring through `selectedProduct` /
/// `visibleWorkItems` doesn't drift.
@MainActor
final class KanbanRepoChipTests: XCTestCase {
    // MARK: - shortRepoName

    /// `https://` URLs canonicalise to the basename minus `.git` —
    /// matches the Rust `short_name_for(url)` helper from design Q3.
    func testShortRepoNameStripsGitSuffixAndPath() {
        XCTAssertEqual(shortRepoName(for: "https://github.com/foo/bar.git"), "bar")
        XCTAssertEqual(shortRepoName(for: "https://github.com/foo/bar"), "bar")
    }

    /// SCP-style `git@host:owner/repo.git` is the GitHub default for
    /// SSH remotes; we must split on `:` after stripping the path so
    /// the chip reads `bar`, not `bar.git` or `owner/bar`.
    func testShortRepoNameHandlesScpStyle() {
        XCTAssertEqual(shortRepoName(for: "git@github.com:foo/bar.git"), "bar")
    }

    /// Garbage in → garbage echoed back, so the chip never renders
    /// empty even when the wire form is unrecognisable.
    func testShortRepoNameFallsBackOnUnparseableInput() {
        XCTAssertEqual(shortRepoName(for: "nimbus"), "nimbus")
    }

    // MARK: - WorkBoardRepoMode.compute

    /// Product default + zero overrides → single-repo. The chip is
    /// expected to lift to the product header so the column stays
    /// uncluttered.
    func testSingleRepoModeWhenNoOverridesAndProductHasDefault() {
        let mode = WorkBoardRepoMode.compute(
            productRepoURL: "https://github.com/foo/bar.git",
            cards: [
                makeTask(id: "t_1", repoRemoteURL: nil),
                makeTask(id: "t_2", repoRemoteURL: nil),
            ]
        )
        XCTAssertEqual(mode, .singleRepo(url: "https://github.com/foo/bar.git"))
    }

    /// Any per-card override flips the board to multi-repo, even when
    /// the override happens to match the product default. The override
    /// is itself a signal — the user wanted that row's repo to be
    /// visible — and suppressing the chip would erase it.
    func testMultiRepoModeWhenAnyCardCarriesOverride() {
        let mode = WorkBoardRepoMode.compute(
            productRepoURL: "https://github.com/foo/bar.git",
            cards: [
                makeTask(id: "t_1", repoRemoteURL: nil),
                makeTask(
                    id: "t_2",
                    repoRemoteURL: "https://github.com/foo/nimbus.git"
                ),
            ]
        )
        XCTAssertEqual(mode, .multiRepo)
    }

    /// No product default and no overrides → there is nothing
    /// repo-shaped to chip. The header stays bare and per-card chips
    /// stay hidden.
    func testNoneModeWhenNeitherProductNorCardCarriesURL() {
        let mode = WorkBoardRepoMode.compute(
            productRepoURL: nil,
            cards: [makeTask(id: "t_1", repoRemoteURL: nil)]
        )
        XCTAssertEqual(mode, .none)
    }

    /// Empty / whitespace strings are treated as "not set" rather
    /// than legitimate URLs — the engine round-trips empty as `None`
    /// in Rust, but tests / hand-built JSON sometimes pass `""`.
    func testEmptyStringsAreTreatedAsNone() {
        XCTAssertEqual(
            WorkBoardRepoMode.compute(
                productRepoURL: "  ",
                cards: [makeTask(id: "t_1", repoRemoteURL: "")]
            ),
            .none
        )
    }

    // MARK: - RepoChipPresentation

    /// Per-card chip is suppressed on rows that inherit the product
    /// default — the chip would just duplicate the product header.
    /// This is the most common case (most cards under a single-repo
    /// product carry no override), so suppressing it keeps cards
    /// uncluttered.
    func testCardChipIsSuppressedWhenInheritingProductDefault() {
        let chip = RepoChipPresentation.forCard(
            task: makeTask(id: "t_1", repoRemoteURL: nil),
            productRepoURL: "https://github.com/foo/bar.git"
        )
        XCTAssertNil(chip)
    }

    /// A per-task `repoRemoteURL` shows up in `.taskOverride`
    /// provenance — the tooltip says "Repo set on this card" so the
    /// reader can tell the URL lives on this row, not on the product.
    func testCardChipReflectsTaskOverrideAndTooltip() {
        let chip = RepoChipPresentation.forCard(
            task: makeTask(
                id: "t_1",
                repoRemoteURL: "git@github.com:foo/nimbus.git"
            ),
            productRepoURL: "https://github.com/foo/bar.git"
        )
        XCTAssertEqual(chip?.shortName, "nimbus")
        XCTAssertEqual(chip?.fullURL, "git@github.com:foo/nimbus.git")
        XCTAssertEqual(chip?.provenance, .taskOverride)
        XCTAssertEqual(
            chip?.tooltip,
            "git@github.com:foo/nimbus.git\nRepo set on this card"
        )
    }

    /// On a product with no default repo, the card's own URL is what
    /// the chip surfaces — there is nothing else to inherit from, so
    /// the chip is informative on every card.
    func testCardChipShowsOwnURLWhenProductHasNoDefault() {
        let chip = RepoChipPresentation.forCard(
            task: makeTask(
                id: "t_1",
                repoRemoteURL: "https://github.com/foo/bar.git"
            ),
            productRepoURL: nil
        )
        XCTAssertEqual(chip?.shortName, "bar")
        XCTAssertEqual(chip?.provenance, .taskOverride)
    }

    /// Card-level chip is hidden when there is no resolvable URL:
    /// returning `nil` here is what lets the view skip rendering
    /// without an awkward empty capsule.
    func testCardChipIsNilWhenNoResolvableURL() {
        let chip = RepoChipPresentation.forCard(
            task: makeTask(id: "t_1", repoRemoteURL: nil),
            productRepoURL: nil
        )
        XCTAssertNil(chip)
    }

    /// Product-header chip always carries `.productDefault`
    /// provenance — single-repo mode requires zero overrides by
    /// construction, so the chip's source is the product itself.
    func testProductHeaderChipPinsToProductDefaultProvenance() {
        let chip = RepoChipPresentation.forProductHeader(
            productRepoURL: "https://github.com/foo/bar.git"
        )
        XCTAssertEqual(chip.shortName, "bar")
        XCTAssertEqual(chip.provenance, .productDefault)
    }

    // MARK: - ChatViewModel wiring

    /// End-to-end: a product with a default and zero overrides
    /// surfaces single-repo mode through the view-model, so the view
    /// reads the header chip and skips the per-card chips.
    func testModelExposesSingleRepoModeAndSuppressesCardChips() {
        let model = makeModel(
            productRepoURL: "https://github.com/foo/bar.git",
            taskOverrides: [nil, nil]
        )
        XCTAssertEqual(
            model.workBoardRepoMode,
            .singleRepo(url: "https://github.com/foo/bar.git")
        )
        for task in model.visibleWorkItems {
            XCTAssertNil(
                model.repoChip(for: task),
                "single-repo mode should suppress the per-card chip"
            )
        }
    }

    /// End-to-end multi-repo: an override on one card flips the mode,
    /// and `repoChip(for:)` returns a chip only for the overriding
    /// row. The inheriting row stays bare — its repo matches the
    /// product default, so the chip would just duplicate the header.
    /// This is the user-visible signal "only rows that differ from
    /// the product default get called out".
    func testModelExposesMultiRepoModeAndChipsOnlyTheOverrideRow() {
        let model = makeModel(
            productRepoURL: "https://github.com/foo/bar.git",
            taskOverrides: [nil, "https://github.com/foo/nimbus.git"]
        )
        XCTAssertEqual(model.workBoardRepoMode, .multiRepo)

        let chips = model.visibleWorkItems.map { model.repoChip(for: $0) }
        XCTAssertEqual(chips.count, 2)
        XCTAssertNil(
            chips[0],
            "row inheriting the product default should not chip"
        )
        XCTAssertEqual(chips[1]?.shortName, "nimbus")
        XCTAssertEqual(chips[1]?.provenance, .taskOverride)
    }

    /// A product with no default at all + zero overrides falls into
    /// `.none` mode — the chip stays hidden everywhere. Validates
    /// that the view-model doesn't accidentally surface a chip with
    /// an empty URL.
    func testModelReturnsNoneModeWhenProductHasNoDefault() {
        let model = makeModel(productRepoURL: nil, taskOverrides: [nil])
        XCTAssertEqual(model.workBoardRepoMode, .none)
        for task in model.visibleWorkItems {
            XCTAssertNil(model.repoChip(for: task))
        }
    }

    /// Multi-repo product (no default) where every card carries its
    /// own `repoRemoteURL`: the chip MUST render on every card
    /// because the URL is the only place the repo is identified —
    /// there is no product header chip to lift it to. Locks in the
    /// "always show badge when product default is null" rule from
    /// the work item description.
    func testModelChipsEveryCardWhenProductHasNoDefault() {
        let model = makeModel(
            productRepoURL: nil,
            taskOverrides: [
                "https://github.com/foo/bar.git",
                "https://github.com/foo/nimbus.git",
            ]
        )
        XCTAssertEqual(model.workBoardRepoMode, .multiRepo)

        let chips = model.visibleWorkItems.map { model.repoChip(for: $0) }
        XCTAssertEqual(chips.count, 2)
        XCTAssertEqual(chips[0]?.shortName, "bar")
        XCTAssertEqual(chips[0]?.provenance, .taskOverride)
        XCTAssertEqual(chips[1]?.shortName, "nimbus")
        XCTAssertEqual(chips[1]?.provenance, .taskOverride)
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

    /// Build a model with one product (optional repo default), one
    /// project, and one task per entry in `taskOverrides`. Mirrors
    /// the fixture pattern in `ProjectDesignDocAffordanceTests`.
    private func makeModel(
        productRepoURL: String?,
        taskOverrides: [String?]
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
        let tasks = taskOverrides.enumerated().map { index, override in
            var t = makeTask(id: "task_\(index)", repoRemoteURL: override)
            t.ordinal = index + 1
            return t
        }
        model.tasksByProjectID = [projectID: tasks]
        return model
    }
}
