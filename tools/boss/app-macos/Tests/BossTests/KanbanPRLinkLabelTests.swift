import XCTest
@testable import Boss

/// Pins the label-rendering rules for kanban-card PR links per the
/// "show only repo name (not org/repo) in Kanban card PR links unless
/// ambiguous" work item. The display label drops the org prefix when
/// the repo is unique among visible cards and falls back to the full
/// `org/repo#n` form only when another visible card uses the same
/// repo name under a different org. The link target (URL) is never
/// rewritten — only the label is shortened.
@MainActor
final class KanbanPRLinkLabelTests: XCTestCase {
    // MARK: - parseGitHubPRURL

    func testParseGitHubPRURLExtractsTriple() {
        let parsed = parseGitHubPRURL("https://github.com/linkedin-multiproduct/dev-infra/pull/250")
        XCTAssertEqual(parsed?.org, "linkedin-multiproduct")
        XCTAssertEqual(parsed?.repo, "dev-infra")
        XCTAssertEqual(parsed?.number, "250")
    }

    /// Anything non-GitHub returns nil — the caller falls back to the
    /// raw URL so we don't accidentally mangle Gerrit/GitLab/etc. links
    /// that happen to flow through the same field.
    func testParseGitHubPRURLRejectsOtherHosts() {
        XCTAssertNil(parseGitHubPRURL("https://gitlab.com/foo/bar/-/merge_requests/250"))
        XCTAssertNil(parseGitHubPRURL("https://example.com/foo/bar/pull/250"))
    }

    /// GitHub Issue or Commit URLs share the host but not the `pull`
    /// segment. They must not collapse to a fake PR triple.
    func testParseGitHubPRURLRejectsNonPullPaths() {
        XCTAssertNil(parseGitHubPRURL("https://github.com/foo/bar/issues/250"))
        XCTAssertNil(parseGitHubPRURL("https://github.com/foo/bar/commit/abc123"))
    }

    func testParseGitHubPRURLRejectsNonNumericPRId() {
        XCTAssertNil(parseGitHubPRURL("https://github.com/foo/bar/pull/abc"))
    }

    // MARK: - ambiguousPRRepoNames

    /// One PR per repo across the board — the bare `repo#n` label is
    /// always unambiguous, so the set is empty. This is the common
    /// "small handful of distinct repos" case the design optimises for.
    func testAmbiguousPRRepoNamesEmptyWhenAllReposDistinct() {
        let cards = [
            makeTaskWithPR("https://github.com/linkedin-eng/foo/pull/1"),
            makeTaskWithPR("https://github.com/linkedin-eng/bar/pull/2"),
            makeTaskWithPR("https://github.com/linkedin-multiproduct/baz/pull/3"),
        ]
        XCTAssertEqual(ambiguousPRRepoNames(in: cards), [])
    }

    /// Two PRs share a repo name but live under different orgs — the
    /// classic collision the spec calls out (`linkedin-eng/foo` vs
    /// `linkedin-multiproduct/foo`). Both cards must render `org/repo#n`.
    func testAmbiguousPRRepoNamesFlagsCrossOrgCollision() {
        let cards = [
            makeTaskWithPR("https://github.com/linkedin-eng/foo/pull/1"),
            makeTaskWithPR("https://github.com/linkedin-multiproduct/foo/pull/2"),
            makeTaskWithPR("https://github.com/linkedin-eng/bar/pull/3"),
        ]
        XCTAssertEqual(ambiguousPRRepoNames(in: cards), ["foo"])
    }

    /// Multiple PRs under the same `org/repo` aren't ambiguous — repo
    /// `dev-infra` only appears under `linkedin-multiproduct`, so
    /// `dev-infra#250` is still a unique label across the board.
    func testAmbiguousPRRepoNamesIgnoresSameOrgSameRepo() {
        let cards = [
            makeTaskWithPR("https://github.com/linkedin-multiproduct/dev-infra/pull/250"),
            makeTaskWithPR("https://github.com/linkedin-multiproduct/dev-infra/pull/251"),
        ]
        XCTAssertEqual(ambiguousPRRepoNames(in: cards), [])
    }

    /// Cards without a PR URL — or with a PR URL on a non-GitHub host
    /// — can never collide with a GitHub repo name, so they must not
    /// inflate the ambiguity set.
    func testAmbiguousPRRepoNamesIgnoresCardsWithoutGitHubPR() {
        let cards = [
            makeTaskWithPR(nil),
            makeTaskWithPR("https://gitlab.com/linkedin-eng/foo/-/merge_requests/1"),
            makeTaskWithPR("https://github.com/linkedin-eng/foo/pull/2"),
        ]
        XCTAssertEqual(ambiguousPRRepoNames(in: cards), [])
    }

    /// Org/repo name comparison is case-insensitive — GitHub itself
    /// treats `LinkedIn-Eng/Foo` and `linkedin-eng/foo` as the same
    /// project, and we shouldn't claim ambiguity from a casing
    /// difference alone.
    func testAmbiguousPRRepoNamesCaseInsensitive() {
        let cards = [
            makeTaskWithPR("https://github.com/LinkedIn-Eng/Foo/pull/1"),
            makeTaskWithPR("https://github.com/linkedin-eng/foo/pull/2"),
        ]
        XCTAssertEqual(ambiguousPRRepoNames(in: cards), [])
    }

    // MARK: - pullRequestLinkLabel

    /// With no ambiguity set passed (nil), the label always renders
    /// the full `org/repo#n` form. This is the "tooltip / detail
    /// surface" path the design carves out — the popover passes nil
    /// so the user always sees the disambiguating context there.
    func testPullRequestLinkLabelFullFormWhenAmbiguousSetNil() {
        XCTAssertEqual(
            pullRequestLinkLabel(
                for: "https://github.com/linkedin-multiproduct/dev-infra/pull/250",
                ambiguousRepoNames: nil
            ),
            "linkedin-multiproduct/dev-infra#250"
        )
    }

    /// Empty ambiguity set + recognised PR URL → short `repo#n` label.
    /// This is the on-board default: when nothing collides, drop the
    /// org prefix so the card has room for the PR number and badges.
    func testPullRequestLinkLabelShortFormWhenRepoUnambiguous() {
        XCTAssertEqual(
            pullRequestLinkLabel(
                for: "https://github.com/linkedin-multiproduct/dev-infra/pull/250",
                ambiguousRepoNames: []
            ),
            "dev-infra#250"
        )
    }

    /// Repo name appears in the ambiguity set → fall back to the full
    /// form so the reader can tell which org the card refers to.
    func testPullRequestLinkLabelFullFormWhenRepoAmbiguous() {
        XCTAssertEqual(
            pullRequestLinkLabel(
                for: "https://github.com/linkedin-eng/foo/pull/12",
                ambiguousRepoNames: ["foo"]
            ),
            "linkedin-eng/foo#12"
        )
    }

    /// Non-GitHub URLs return nil so the caller falls back to the raw
    /// URL — we don't want to silently truncate or mislabel a link
    /// the helper doesn't understand.
    func testPullRequestLinkLabelReturnsNilForUnrecognisedURL() {
        XCTAssertNil(
            pullRequestLinkLabel(
                for: "https://gitlab.com/foo/bar/-/merge_requests/5",
                ambiguousRepoNames: []
            )
        )
    }

    // MARK: - ChatViewModel.ambiguousVisibleRepoNames

    /// End-to-end through `visibleWorkItems`: two cards on the board
    /// share repo `foo` under different orgs, so the model reports
    /// `{"foo"}` and the kanban will render both cards' PR labels
    /// with the org prefix.
    func testModelExposesAmbiguousRepoNameAcrossOrgs() {
        let model = makeModel(prURLs: [
            "https://github.com/linkedin-eng/foo/pull/1",
            "https://github.com/linkedin-multiproduct/foo/pull/2",
            "https://github.com/linkedin-eng/bar/pull/3",
        ])
        XCTAssertEqual(model.ambiguousVisibleRepoNames, ["foo"])
    }

    /// When no two visible cards collide, the set is empty — every
    /// card on the board renders the short `repo#n` form.
    func testModelReportsNoAmbiguityForDistinctRepos() {
        let model = makeModel(prURLs: [
            "https://github.com/linkedin-eng/foo/pull/1",
            "https://github.com/linkedin-eng/bar/pull/2",
        ])
        XCTAssertEqual(model.ambiguousVisibleRepoNames, [])
    }

    // MARK: - Fixtures

    private func makeTaskWithPR(_ prURL: String?, id: String = UUID().uuidString) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: "proj_test",
            kind: "task",
            name: "Card",
            description: "",
            status: "todo",
            priority: "medium",
            ordinal: 1,
            prURL: prURL,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z",
            lastStatusActor: "human",
            createdVia: "unknown",
            repoRemoteURL: nil
        )
    }

    private func makeModel(prURLs: [String?]) -> ChatViewModel {
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
        let tasks = prURLs.enumerated().map { index, prURL in
            var t = makeTaskWithPR(prURL, id: "task_\(index)")
            t.ordinal = index + 1
            return t
        }
        model.tasksByProjectID = [projectID: tasks]
        return model
    }
}
