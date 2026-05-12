import XCTest
@testable import Boss

/// Drives the kanban project-card design-doc affordance. The view
/// itself (`ProjectDesignDocAffordance`) is a thin reflection of the
/// pure-data `ProjectDesignDocAffordancePresentation.from(state:)`
/// mapping plus the engine-resolved state on `ChatViewModel` — so
/// covering both is what the design doc's "snapshot tests for each
/// state; click handler covered by UI test" boils down to without a
/// snapshot library wired into the package, mirroring the pattern
/// already used in `WorkDependencyKanbanTests`.
@MainActor
final class ProjectDesignDocAffordanceTests: XCTestCase {
    // MARK: - Icon variants per ProjectDesignDocState

    /// `.notSet` collapses the affordance entirely — Q3 of the design
    /// is explicit that the icon must be hidden when no pointer is
    /// set so the kanban doesn't promise something it can't deliver.
    func testPresentationIsNilForNotSet() {
        XCTAssertNil(ProjectDesignDocAffordancePresentation.from(state: .notSet))
    }

    /// `.resolved` renders the plain doc icon with a tooltip that
    /// summarises `<owner/repo>:<path>` so a hover tells the reader
    /// where the click will land before they take it.
    func testPresentationForResolvedRendersDocIconWithRepoPathTooltip() {
        let state = ProjectDesignDocState.resolved(
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "https://github.com/foo/bar.git",
                branch: "main",
                path: "tools/boss/docs/designs/x.md",
                kind: .sameProduct(productID: "prod_1")
            ),
            workspacePath: "/Users/me/Documents/dev/workspaces/mono-agent-001",
            webURL: "https://github.com/foo/bar/blob/main/tools/boss/docs/designs/x.md"
        )
        let presentation = ProjectDesignDocAffordancePresentation.from(state: state)
        XCTAssertEqual(presentation?.systemImage, "doc.text")
        XCTAssertEqual(presentation?.kind, .resolved)
        XCTAssertEqual(presentation?.tooltip, "foo/bar:tools/boss/docs/designs/x.md")
        XCTAssertEqual(presentation?.accessibilityLabel, "Open design doc")
    }

    /// SCP-style repo URLs (`git@host:owner/repo.git`) are a legal
    /// `repo_remote_url` storage form, so the tooltip helper must
    /// strip the `.git` suffix and surface `owner/repo` rather than
    /// the raw url. Without this the hover would read
    /// `git@github.com:owner/repo.git:<path>` which is noise.
    func testPresentationParsesScpStyleRepoURL() {
        let state = ProjectDesignDocState.resolved(
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "git@github.com:foo/bar.git",
                branch: "main",
                path: "docs/x.md",
                kind: .external
            ),
            workspacePath: nil,
            webURL: "https://github.com/foo/bar/blob/main/docs/x.md"
        )
        XCTAssertEqual(
            ProjectDesignDocAffordancePresentation.from(state: state)?.tooltip,
            "foo/bar:docs/x.md"
        )
    }

    /// `.broken` flips the icon to the warning glyph and surfaces the
    /// engine's reason in the tooltip — Q5 of the design wants the
    /// pointer-broken state to be fixable rather than silent.
    func testPresentationForBrokenRendersWarningGlyphWithReason() {
        let state = ProjectDesignDocState.broken(reason: "no repo to resolve against")
        let presentation = ProjectDesignDocAffordancePresentation.from(state: state)
        XCTAssertEqual(presentation?.systemImage, "exclamationmark.triangle")
        XCTAssertEqual(presentation?.kind, .broken)
        XCTAssertEqual(
            presentation?.tooltip,
            "Design doc pointer is broken: no repo to resolve against"
        )
        XCTAssertEqual(
            presentation?.accessibilityLabel,
            "Design doc pointer is broken"
        )
    }

    // MARK: - Click handler dispatch by kind

    /// Clicking a `.notSet` row is a no-op. The affordance shouldn't
    /// have been clickable in the first place (the view returns `nil`),
    /// but the dispatcher must hold the line as well so a stale model
    /// snapshot can't drive a misclick into an error.
    func testOpenIsNoOpWhenStateIsNotSet() {
        let model = makeModelWithProject()
        let project = model.projectsByProductID.values.first!.first!
        model.designDocStateByProjectID[project.id] = .notSet
        model.openProjectDesignDoc(project)
        XCTAssertNil(model.workErrorMessage)
    }

    /// `.resolved` with a same-product kind and a leased cube workspace
    /// hands the open dispatcher a `file://` URL that points at the
    /// resolved doc inside that workspace, **not** the GitHub web URL.
    /// This is the "open in `$EDITOR` / OS-registered .md handler"
    /// fast path from design Q3 (the table row `SameProduct + workspace
    /// available`). The stub captures the URL so the test asserts (a)
    /// the dispatcher chose the local file branch and (b) the path
    /// composition is `workspacePath/resolved.path`.
    func testOpenOnResolvedSameProductWithWorkspaceOpensLocalFile() {
        let model = makeModelWithProject()
        let project = model.projectsByProductID.values.first!.first!
        let workspacePath = "/Users/me/Documents/dev/workspaces/mono-agent-007"
        var openedURLs: [URL] = []
        model.urlOpener = { openedURLs.append($0) }
        model.designDocStateByProjectID[project.id] = .resolved(
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "https://github.com/foo/bar.git",
                branch: "main",
                path: "tools/boss/docs/designs/x.md",
                kind: .sameProduct(productID: project.productID)
            ),
            workspacePath: workspacePath,
            webURL: "https://github.com/foo/bar/blob/main/tools/boss/docs/designs/x.md"
        )
        model.openProjectDesignDoc(project)
        XCTAssertNil(model.workErrorMessage)
        XCTAssertEqual(openedURLs.count, 1)
        let opened = openedURLs.first!
        XCTAssertTrue(opened.isFileURL, "expected a file:// URL, got \(opened)")
        XCTAssertEqual(
            opened.path,
            "\(workspacePath)/tools/boss/docs/designs/x.md"
        )
    }

    /// Other-product pointers with a leased workspace get the same
    /// fast path as same-product pointers — cube has the repo, the
    /// renderer / `$EDITOR` can read it directly. Design Q3's table
    /// row `OtherProduct + workspace available` mirrors `SameProduct`.
    func testOpenOnResolvedOtherProductWithWorkspaceOpensLocalFile() {
        let model = makeModelWithProject()
        let project = model.projectsByProductID.values.first!.first!
        let workspacePath = "/Users/me/Documents/dev/workspaces/wiki-agent-001"
        var openedURLs: [URL] = []
        model.urlOpener = { openedURLs.append($0) }
        model.designDocStateByProjectID[project.id] = .resolved(
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "https://github.com/myorg/wiki.git",
                branch: "main",
                path: "designs/x.md",
                kind: .otherProduct(productID: "prod_wiki")
            ),
            workspacePath: workspacePath,
            webURL: "https://github.com/myorg/wiki/blob/main/designs/x.md"
        )
        model.openProjectDesignDoc(project)
        XCTAssertEqual(openedURLs.count, 1)
        let opened = openedURLs.first!
        XCTAssertTrue(opened.isFileURL)
        XCTAssertEqual(opened.path, "\(workspacePath)/designs/x.md")
    }

    /// Same-product pointer but no workspace leased → fall back to
    /// the web URL. Design Q3's table row `SameProduct + workspace
    /// unavailable` is identical to `External` in terms of open
    /// target. The stub asserts the dispatcher routed the engine's
    /// pre-rendered `web_url`, not a fabricated local path.
    func testOpenOnResolvedSameProductWithoutWorkspaceFallsBackToWebURL() {
        let model = makeModelWithProject()
        let project = model.projectsByProductID.values.first!.first!
        let webURL = "https://github.com/foo/bar/blob/main/tools/boss/docs/designs/x.md"
        var openedURLs: [URL] = []
        model.urlOpener = { openedURLs.append($0) }
        model.designDocStateByProjectID[project.id] = .resolved(
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "https://github.com/foo/bar.git",
                branch: "main",
                path: "tools/boss/docs/designs/x.md",
                kind: .sameProduct(productID: project.productID)
            ),
            workspacePath: nil,
            webURL: webURL
        )
        model.openProjectDesignDoc(project)
        XCTAssertNil(model.workErrorMessage)
        XCTAssertEqual(openedURLs.map(\.absoluteString), [webURL])
    }

    /// External pointers always open the web URL, even if a workspace
    /// for the same repo happens to be leased — Boss doesn't track
    /// the repo as a Product so the kanban can't promise the editor
    /// will land in the right place. (In practice the engine returns
    /// `workspace_path = nil` for `.external` anyway because the repo
    /// isn't on any product, but the dispatcher must hold the line if
    /// that invariant ever drifts.)
    func testOpenOnResolvedExternalAlwaysFallsBackToWebURL() {
        let model = makeModelWithProject()
        let project = model.projectsByProductID.values.first!.first!
        let webURL = "https://github.com/foo/bar/blob/main/x.md"
        var openedURLs: [URL] = []
        model.urlOpener = { openedURLs.append($0) }
        model.designDocStateByProjectID[project.id] = .resolved(
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "https://github.com/foo/bar.git",
                branch: "main",
                path: "x.md",
                kind: .external
            ),
            workspacePath: "/tmp/should-be-ignored",
            webURL: webURL
        )
        model.openProjectDesignDoc(project)
        XCTAssertEqual(openedURLs.map(\.absoluteString), [webURL])
    }

    /// An unparseable web URL on a resolved state still must not
    /// silently swallow the click. The dispatcher surfaces a
    /// workErrorMessage the user can act on.
    func testOpenOnResolvedWithUnparseableURLSurfacesError() {
        let model = makeModelWithProject()
        let project = model.projectsByProductID.values.first!.first!
        model.designDocStateByProjectID[project.id] = .resolved(
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "https://github.com/foo/bar.git",
                branch: "main",
                path: "x.md",
                kind: .external
            ),
            workspacePath: nil,
            webURL: ""
        )
        model.openProjectDesignDoc(project)
        XCTAssertNotNil(model.workErrorMessage)
    }

    /// `.broken` surfaces the engine's reason as a workErrorMessage so
    /// the user can re-point. This is the only signal the click
    /// handler emits when the state is broken.
    func testOpenOnBrokenSurfacesReasonAsWorkError() {
        let model = makeModelWithProject()
        let project = model.projectsByProductID.values.first!.first!
        model.designDocStateByProjectID[project.id] = .broken(reason: "file not found")
        model.openProjectDesignDoc(project)
        XCTAssertEqual(
            model.workErrorMessage,
            "Design doc pointer is broken: file not found"
        )
    }

    // MARK: - Event handling

    /// A `ResolveProjectDesignDoc` reply lands as an
    /// `EngineEvent.projectDesignDocResolved` push; the model must
    /// store it under the right project id so subsequent renders pick
    /// up the right icon without re-fetching.
    func testProjectDesignDocResolvedEventUpdatesModelState() {
        let model = makeModelWithProject()
        let project = model.projectsByProductID.values.first!.first!
        let output = ResolveProjectDesignDocOutput(
            projectID: project.id,
            state: .resolved(
                resolved: ResolvedDesignDoc(
                    repoRemoteURL: "https://github.com/foo/bar.git",
                    branch: "main",
                    path: "docs/x.md",
                    kind: .sameProduct(productID: project.productID)
                ),
                workspacePath: "/Users/me/Documents/dev/workspaces/mono-agent-001",
                webURL: "https://github.com/foo/bar/blob/main/docs/x.md"
            )
        )
        model.handleForTest(event: .projectDesignDocResolved(output: output))
        XCTAssertEqual(model.designDocStateByProjectID[project.id], output.state)
    }

    // MARK: - Section header populates projectID

    /// The kanban section header reads `WorkBoardSection.projectID` to
    /// find the right `WorkProject` for the affordance. The grouping
    /// helper must populate it when a project section's items belong
    /// to a single project — without that, the icon can't render.
    func testProjectGroupedSectionCarriesProjectID() {
        let model = makeModelWithProject()
        let project = model.projectsByProductID.values.first!.first!
        model.workBoardGrouping = .project
        let sections = model.workSections(in: .backlog)
        guard let section = sections.first(where: { $0.title == project.name }) else {
            XCTFail("expected a section titled \(project.name)"); return
        }
        XCTAssertEqual(section.projectID, project.id)
    }

    /// Chores live in their own section titled "Chores" — they have no
    /// project, so the section must carry `projectID == nil` and the
    /// view code must skip the affordance entirely (the design's "one
    /// project, one pointer" rule).
    func testChoreSectionCarriesNilProjectID() {
        let model = makeModelWithProject()
        let productID = model.products.first!.id
        let chore = WorkTask(
            id: "chore_1",
            productID: productID,
            projectID: nil,
            kind: "chore",
            name: "Sweep",
            description: "",
            status: "todo",
            priority: "medium",
            ordinal: 1,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z"
        )
        model.choresByProductID[productID] = [chore]
        model.workBoardGrouping = .project
        let sections = model.workSections(in: .backlog)
        guard let chores = sections.first(where: { $0.title == "Chores" }) else {
            XCTFail("expected a Chores section"); return
        }
        XCTAssertNil(chores.projectID)
    }

    // MARK: - Fixture

    private func makeModelWithProject() -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        // Trap the production default so a test that reaches the
        // `.resolved` branch without installing a recording stub can't
        // silently fire `NSWorkspace.shared.open` during `swift test`
        // (which pops the user's browser on every iteration). Tests
        // that want to exercise the opener must override this with a
        // recording stub.
        model.urlOpener = { url in
            XCTFail("urlOpener was invoked with \(url) — install a recording stub before exercising `.resolved`.")
        }
        let productID = "prod_test"
        model.products = [
            WorkProduct(
                id: productID,
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: "https://github.com/foo/bar.git",
                status: "active",
                createdAt: "2026-05-08T00:00:00Z",
                updatedAt: "2026-05-08T00:00:00Z"
            )
        ]
        model.selectedWorkProductID = productID
        let projectID = "proj_test"
        let project = WorkProject(
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
            lastStatusActor: "human",
            designDocPath: "tools/boss/docs/designs/test.md"
        )
        model.projectsByProductID = [productID: [project]]
        let task = WorkTask(
            id: "task_test",
            productID: productID,
            projectID: projectID,
            kind: "task",
            name: "Phase 1",
            description: "",
            status: "todo",
            priority: "medium",
            ordinal: 1,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-08T00:00:00Z",
            updatedAt: "2026-05-08T00:00:00Z"
        )
        model.tasksByProjectID = [projectID: [task]]
        return model
    }
}

// MARK: - Test-only handler hook

extension ChatViewModel {
    /// The view-model's event handler is private so production code
    /// can't reach in around the engine socket. The kanban affordance
    /// tests need to drive a `projectDesignDocResolved` push without
    /// standing up a fake socket, so we expose a tiny re-emit helper
    /// for tests — same shape as `upsertTaskForTest` in the dependency
    /// tests file.
    fileprivate func handleForTest(event: EngineEvent) {
        switch event {
        case .projectDesignDocResolved(let output):
            designDocStateByProjectID[output.projectID] = output.state
        default:
            XCTFail("handleForTest received unsupported event")
        }
    }
}
