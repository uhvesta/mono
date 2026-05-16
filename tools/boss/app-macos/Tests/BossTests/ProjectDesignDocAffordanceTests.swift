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
            webURL: "https://github.com/foo/bar/blob/main/tools/boss/docs/designs/x.md",
            rawContentURL: nil
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
            webURL: "https://github.com/foo/bar/blob/main/docs/x.md",
            rawContentURL: nil
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

    /// `.resolved` with a same-product kind, a leased workspace, and
    /// **no `rawContentURL`** (non-GitHub repo or older engine) hands
    /// the open dispatcher a `file://` URL pointing at the resolved doc
    /// inside that workspace. This is the workspace fast-path fallback
    /// from design Q3 (`SameProduct + workspace available`), exercised
    /// only when GitHub raw-content is not available. When `rawContentURL`
    /// IS set the dispatcher prefers GitHub; see
    /// [[testOpenOnResolvedMainBranchWithRawContentURLPrefersGitHub]].
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
            webURL: "https://github.com/foo/bar/blob/main/tools/boss/docs/designs/x.md",
            rawContentURL: nil
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

    /// SameProduct + leased workspace + `designRendererOpener` wired
    /// (the production wiring `ContentView` installs) routes the click
    /// to the in-app renderer window — chore #12 of
    /// `project-design-doc-pointer.md`. Exercises the workspace fallback
    /// path when `rawContentURL` is **nil** (non-GitHub repo or older
    /// engine). When `rawContentURL` is present the dispatcher prefers
    /// GitHub; see [[testOpenOnResolvedMainBranchWithRawContentURLPrefersGitHub]].
    func testOpenOnResolvedSameProductWithRendererOpensInRendererWindow() {
        let model = makeModelWithProject()
        let project = model.projectsByProductID.values.first!.first!
        let workspacePath = "/Users/me/Documents/dev/workspaces/mono-agent-007"
        var renderedContents: [DesignRendererContent] = []
        model.designRendererOpener = { renderedContents.append($0) }
        model.designDocStateByProjectID[project.id] = .resolved(
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "https://github.com/foo/bar.git",
                branch: "main",
                path: "tools/boss/docs/designs/x.md",
                kind: .sameProduct(productID: project.productID)
            ),
            workspacePath: workspacePath,
            webURL: "https://github.com/foo/bar/blob/main/tools/boss/docs/designs/x.md",
            rawContentURL: nil
        )
        model.openProjectDesignDoc(project)
        XCTAssertNil(model.workErrorMessage)
        XCTAssertEqual(renderedContents.count, 1)
        let content = renderedContents.first!
        XCTAssertEqual(
            content.filePath,
            "\(workspacePath)/tools/boss/docs/designs/x.md"
        )
        XCTAssertEqual(
            content.webURL,
            "https://github.com/foo/bar/blob/main/tools/boss/docs/designs/x.md"
        )
        XCTAssertEqual(content.repoLabel, "foo/bar")
        XCTAssertEqual(content.title, project.name)
        XCTAssertEqual(content.projectID, project.id)
    }

    /// OtherProduct + leased workspace also flows through the renderer
    /// — Q3's dispatch table treats SameProduct/OtherProduct
    /// identically once a workspace is available, and chore #12's
    /// "doc renders identically in both surfaces" acceptance means
    /// the OtherProduct surface picks the same view.
    func testOpenOnResolvedOtherProductWithRendererOpensInRendererWindow() {
        let model = makeModelWithProject()
        let project = model.projectsByProductID.values.first!.first!
        let workspacePath = "/Users/me/Documents/dev/workspaces/wiki-agent-001"
        var renderedContents: [DesignRendererContent] = []
        model.designRendererOpener = { renderedContents.append($0) }
        model.designDocStateByProjectID[project.id] = .resolved(
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "git@github.com:myorg/wiki.git",
                branch: "main",
                path: "designs/x.md",
                kind: .otherProduct(productID: "prod_wiki")
            ),
            workspacePath: workspacePath,
            webURL: "https://github.com/myorg/wiki/blob/main/designs/x.md",
            rawContentURL: nil
        )
        model.openProjectDesignDoc(project)
        XCTAssertEqual(renderedContents.count, 1)
        let content = renderedContents.first!
        XCTAssertEqual(content.filePath, "\(workspacePath)/designs/x.md")
        XCTAssertEqual(content.repoLabel, "myorg/wiki")
    }

    /// External pointers must skip the renderer regardless of whether
    /// an opener is wired — the renderer reads the file from disk and
    /// there is no leased workspace for external repos. The
    /// dispatcher hands the click to the web URL via `urlOpener`
    /// (asserted in [[testOpenOnResolvedExternalAlwaysFallsBackToWebURL]]);
    /// here we additionally assert the renderer opener is NOT called.
    func testOpenOnResolvedExternalSkipsRenderer() {
        let model = makeModelWithProject()
        let project = model.projectsByProductID.values.first!.first!
        var renderedContents: [DesignRendererContent] = []
        model.designRendererOpener = { renderedContents.append($0) }
        var openedURLs: [URL] = []
        model.urlOpener = { openedURLs.append($0) }
        model.designDocStateByProjectID[project.id] = .resolved(
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "https://github.com/foo/bar.git",
                branch: "main",
                path: "x.md",
                kind: .external
            ),
            workspacePath: nil,
            webURL: "https://github.com/foo/bar/blob/main/x.md",
            rawContentURL: nil
        )
        model.openProjectDesignDoc(project)
        XCTAssertTrue(renderedContents.isEmpty)
        XCTAssertEqual(openedURLs.count, 1)
    }

    /// Other-product pointers with a leased workspace get the same
    /// fast path as same-product pointers — cube has the repo, the
    /// renderer / `$EDITOR` can read it directly. Design Q3's table
    /// row `OtherProduct + workspace available` mirrors `SameProduct`.
    /// This test exercises the legacy `urlOpener` fallback path (no
    /// renderer opener installed); the renderer-opener path is
    /// covered by [[testOpenOnResolvedOtherProductWithRendererOpensInRendererWindow]].
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
            webURL: "https://github.com/myorg/wiki/blob/main/designs/x.md",
            rawContentURL: nil
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
            webURL: webURL,
            rawContentURL: nil
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
            webURL: webURL,
            rawContentURL: nil
        )
        model.openProjectDesignDoc(project)
        XCTAssertEqual(openedURLs.map(\.absoluteString), [webURL])
    }

    /// In-review designs live on a PR branch. When a workspace path is
    /// available but the branch is not `main`, the dispatcher must skip
    /// the local-file fast-path (the workspace is likely on a different
    /// task's branch so the file won't be there) and instead fetch via
    /// `rawContentURL`. The window must open immediately (before the
    /// fetch settles) and transition to `.loaded` once the content
    /// arrives. Regression for the bug where clicking a design-doc icon
    /// on an in_review task surfaced a file-not-found error instead of
    /// opening the markdown viewer.
    func testOpenOnResolvedInReviewBranchOpensWindowImmediatelyThenLoads() async {
        let model = makeModelWithProject()
        let project = model.projectsByProductID.values.first!.first!

        // Capture any local-file opens (should stay empty).
        var openedLocalFiles: [URL] = []
        model.urlOpener = { url in
            if url.isFileURL { openedLocalFiles.append(url) }
        }

        // Record async-markdown-viewer window open calls.
        var asyncWindowOpens = 0
        model.asyncMarkdownViewerOpener = { asyncWindowOpens += 1 }

        // Record rawContentFetcher invocations.
        let fetchExpectation = XCTestExpectation(description: "rawContentFetcher called")
        model.rawContentFetcher = { _ in
            fetchExpectation.fulfill()
            return "# Design Doc"
        }

        // Non-main branch with a workspace path and a rawContentURL present.
        // This simulates an in_review design task (T371-style).
        let rawURL = "https://raw.githubusercontent.com/foo/bar/design-boss-ci-buildkite/tools/boss/docs/designs/x.md"
        model.designDocStateByProjectID[project.id] = .resolved(
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "git@github.com:foo/bar.git",
                branch: "design-boss-ci-buildkite",
                path: "tools/boss/docs/designs/x.md",
                kind: .sameProduct(productID: project.productID)
            ),
            workspacePath: "/Users/me/Documents/dev/workspaces/mono-agent-007",
            webURL: "https://github.com/foo/bar/blob/design-boss-ci-buildkite/tools/boss/docs/designs/x.md",
            rawContentURL: rawURL
        )
        model.openProjectDesignDoc(project)

        // The window open must be synchronous — one call before any await.
        XCTAssertEqual(asyncWindowOpens, 1, "async-markdown-viewer window must open immediately on click")
        // The VM must be in loading state right after the click.
        if case .loading = model.asyncMarkdownViewerVM.state { } else {
            XCTFail("expected .loading state immediately after click; got \(model.asyncMarkdownViewerVM.state)")
        }

        await fulfillment(of: [fetchExpectation], timeout: 1.0)
        XCTAssertTrue(
            openedLocalFiles.isEmpty,
            "expected no local-file open for in-review branch; got: \(openedLocalFiles)"
        )
        // After the fetch settles the VM must be in the loaded state.
        if case .loaded(let title, let markdown) = model.asyncMarkdownViewerVM.state {
            XCTAssertEqual(title, project.name)
            XCTAssertEqual(markdown, "# Design Doc")
        } else {
            XCTFail("expected .loaded state after fetch; got \(model.asyncMarkdownViewerVM.state)")
        }
        XCTAssertNil(model.workErrorMessage)
    }

    /// Regression test for P491: a merged (main-branch) design whose
    /// `rawContentURL` is set must be fetched from GitHub even when a
    /// cube workspace path is available for the repo. The workspace may
    /// be leased to a different task on a different branch, so the
    /// on-disk file can be absent or stale. `rawContentURL` is preferred
    /// in all cases, not just for in-review (non-main) branches.
    func testOpenOnResolvedMainBranchWithRawContentURLPrefersGitHub() async {
        let model = makeModelWithProject()
        let project = model.projectsByProductID.values.first!.first!

        // Capture any local-file opens — must stay empty.
        var openedLocalFiles: [URL] = []
        model.urlOpener = { url in
            if url.isFileURL { openedLocalFiles.append(url) }
        }

        // Wire the async viewer so we can observe the open call.
        var asyncWindowOpens = 0
        model.asyncMarkdownViewerOpener = { asyncWindowOpens += 1 }

        let fetchExpectation = XCTestExpectation(description: "rawContentFetcher called")
        model.rawContentFetcher = { _ in
            fetchExpectation.fulfill()
            return "# Merged design"
        }

        // main branch + rawContentURL + workspacePath — the P491 shape.
        let rawURL = "https://raw.githubusercontent.com/foo/bar/main/tools/boss/docs/designs/x.md"
        model.designDocStateByProjectID[project.id] = .resolved(
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "git@github.com:foo/bar.git",
                branch: "main",
                path: "tools/boss/docs/designs/x.md",
                kind: .sameProduct(productID: project.productID)
            ),
            workspacePath: "/Users/me/Documents/dev/workspaces/mono-agent-007",
            webURL: "https://github.com/foo/bar/blob/main/tools/boss/docs/designs/x.md",
            rawContentURL: rawURL
        )
        model.openProjectDesignDoc(project)

        // Window must open immediately.
        XCTAssertEqual(asyncWindowOpens, 1, "async-markdown-viewer window must open immediately")
        if case .loading = model.asyncMarkdownViewerVM.state { } else {
            XCTFail("expected .loading immediately after click; got \(model.asyncMarkdownViewerVM.state)")
        }

        await fulfillment(of: [fetchExpectation], timeout: 1.0)
        XCTAssertTrue(
            openedLocalFiles.isEmpty,
            "dispatcher must not open a local file when rawContentURL is present; got: \(openedLocalFiles)"
        )
        if case .loaded(let title, _) = model.asyncMarkdownViewerVM.state {
            XCTAssertEqual(title, project.name)
        } else {
            XCTFail("expected .loaded after fetch; got \(model.asyncMarkdownViewerVM.state)")
        }
        XCTAssertNil(model.workErrorMessage)
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
            webURL: "",
            rawContentURL: nil
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
                webURL: "https://github.com/foo/bar/blob/main/docs/x.md",
                rawContentURL: nil
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

    // MARK: - DesignRendererContent.from

    /// External resolved kinds carry no workspace path the renderer
    /// can read from, so the factory refuses to build content for
    /// them. The dispatcher in [[ChatViewModel.openProjectDesignDoc(_:)]]
    /// already short-circuits this case via the eligibility predicate,
    /// but the factory has to hold the line itself so a misuse can't
    /// hand a `.external` kind to the renderer window.
    func testDesignRendererContentRefusesExternalKind() {
        let content = DesignRendererContent.from(
            projectID: "proj",
            projectName: "Some Project",
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "https://github.com/foo/bar.git",
                branch: "main",
                path: "x.md",
                kind: .external
            ),
            workspacePath: "/tmp/ignored",
            webURL: "https://github.com/foo/bar/blob/main/x.md"
        )
        XCTAssertNil(content)
    }

    /// Empty project names fall back to the doc path so the window
    /// title is never blank — re-clicking a renderer window relies on
    /// the title for disambiguation in the macOS Window menu.
    func testDesignRendererContentFallsBackToPathWhenProjectNameEmpty() {
        let content = DesignRendererContent.from(
            projectID: "proj",
            projectName: "",
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "https://github.com/foo/bar.git",
                branch: "main",
                path: "tools/boss/docs/designs/x.md",
                kind: .sameProduct(productID: "prod_1")
            ),
            workspacePath: "/ws",
            webURL: "https://github.com/foo/bar/blob/main/tools/boss/docs/designs/x.md"
        )
        XCTAssertEqual(content?.title, "tools/boss/docs/designs/x.md")
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

// MARK: - Design task card affordance

/// Verifies that the design-doc affordance wires correctly for `kind=design`
/// task cards. The card derives its state from the same
/// `model.designDocStateByProjectID` dict as the project-section header, so
/// the same presentation rules apply — these tests exercise the path from
/// task → projectID → state → presentation that `WorkBoardCardItem` computes.
@MainActor
final class DesignTaskCardDesignDocAffordanceTests: XCTestCase {
    /// A `kind=design` task whose parent project has a resolved pointer
    /// should produce a non-nil affordance presentation — the doc icon
    /// must appear on the card so the worker can navigate to the design
    /// without first hunting for the project card.
    func testDesignTaskWithResolvedStateProducesPresentation() {
        let model = makeModelWithDesignTask()
        let projectID = model.projectsByProductID.values.first!.first!.id
        let resolvedState = ProjectDesignDocState.resolved(
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "https://github.com/foo/bar.git",
                branch: "main",
                path: "tools/boss/docs/designs/test.md",
                kind: .sameProduct(productID: "prod_test")
            ),
            workspacePath: "/Users/me/Documents/dev/workspaces/mono-agent-001",
            webURL: "https://github.com/foo/bar/blob/main/tools/boss/docs/designs/test.md",
            rawContentURL: nil
        )
        model.designDocStateByProjectID[projectID] = resolvedState
        let state = model.designDocStateByProjectID[projectID] ?? .notSet
        XCTAssertNotNil(ProjectDesignDocAffordancePresentation.from(state: state))
        XCTAssertEqual(
            ProjectDesignDocAffordancePresentation.from(state: state)?.systemImage,
            "doc.text"
        )
    }

    /// A `kind=design` task whose parent project has `.notSet` state
    /// must produce a nil presentation — the card renders no affordance
    /// rather than a broken / empty placeholder.
    func testDesignTaskWithNotSetStateProducesNoPresentation() {
        let model = makeModelWithDesignTask()
        let projectID = model.projectsByProductID.values.first!.first!.id
        model.designDocStateByProjectID[projectID] = .notSet
        let state = model.designDocStateByProjectID[projectID] ?? .notSet
        XCTAssertNil(ProjectDesignDocAffordancePresentation.from(state: state))
    }

    /// When the parent project's state has not been fetched yet
    /// (no entry in `designDocStateByProjectID`) the affordance must
    /// be hidden — the fallback to `.notSet` in `WorkBoardCardItem`
    /// ensures no stale icon appears while a resolve RPC is in flight.
    func testDesignTaskWithMissingStateDefaultsToHidden() {
        let model = makeModelWithDesignTask()
        let projectID = model.projectsByProductID.values.first!.first!.id
        XCTAssertNil(model.designDocStateByProjectID[projectID])
        let state = model.designDocStateByProjectID[projectID] ?? .notSet
        XCTAssertNil(ProjectDesignDocAffordancePresentation.from(state: state))
    }

    /// `openProjectDesignDoc` is called with the correct project when
    /// the design task's affordance is tapped. This mirrors the
    /// existing click-handler tests for the project-section header.
    func testOpenProjectDesignDocIsInvokedForDesignTask() {
        let model = makeModelWithDesignTask()
        let project = model.projectsByProductID.values.first!.first!
        var openedURLs: [URL] = []
        model.urlOpener = { url in openedURLs.append(url) }
        let resolvedState = ProjectDesignDocState.resolved(
            resolved: ResolvedDesignDoc(
                repoRemoteURL: "https://github.com/foo/bar.git",
                branch: "main",
                path: "tools/boss/docs/designs/test.md",
                kind: .external
            ),
            workspacePath: nil,
            webURL: "https://github.com/foo/bar/blob/main/tools/boss/docs/designs/test.md",
            rawContentURL: nil
        )
        model.designDocStateByProjectID[project.id] = resolvedState
        model.openProjectDesignDoc(project)
        XCTAssertEqual(openedURLs.count, 1)
        XCTAssertEqual(
            openedURLs.first?.absoluteString,
            "https://github.com/foo/bar/blob/main/tools/boss/docs/designs/test.md"
        )
    }

    // MARK: - Fixture

    private func makeModelWithDesignTask() -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
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
            id: "task_design",
            productID: productID,
            projectID: projectID,
            kind: "design",
            name: "Design the feature",
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
