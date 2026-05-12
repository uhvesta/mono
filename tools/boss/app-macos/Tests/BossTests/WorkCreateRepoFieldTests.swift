import XCTest
@testable import Boss

/// Drives the work-item (chore + task) create form's repo field
/// across the two render modes called out in design Q10 / follow-up
/// chore #10 of `multi-repo-work-modeling.md`:
///
/// - **Product has a default repo**: field is hidden behind an
///   "Override repo…" disclosure; inheriting the product default is
///   the no-action path.
/// - **Product has no default**: field is shown and required; a
///   "Set as product default" checkbox piggybacks a product update
///   onto the work-item create when the user types a fresh URL.
///
/// Following the pattern in `KanbanRepoChipTests` and
/// `ProductCreateFormCopyTests`, this file pins the pure-data form
/// state and the ChatViewModel wiring so the SwiftUI view stays a
/// thin reflection of values an XCTest can assert without spinning
/// up a host. The interaction test at the bottom exercises the
/// view-model end-to-end and asserts the wire payload via an
/// `outboundRecorder` spy.
@MainActor
final class WorkCreateRepoFieldTests: XCTestCase {
    // MARK: - WorkCreateRepoFormState snapshots

    /// Product-with-default snapshot: mode resolves to
    /// `.productHasDefault`, the override disclosure is closed,
    /// submission is unblocked, and `submittedURL` is `nil` (the
    /// engine treats absent as "inherit").
    func testFormStateSnapshotForProductWithDefault() {
        let state = WorkCreateRepoFormState(
            productRepoURL: "https://github.com/foo/bar.git",
            knownRepos: ["https://github.com/foo/bar.git"]
        )
        XCTAssertEqual(
            state.mode,
            .productHasDefault(defaultURL: "https://github.com/foo/bar.git")
        )
        XCTAssertFalse(state.overrideEnabled)
        XCTAssertEqual(state.enteredURL, "")
        XCTAssertFalse(state.setAsProductDefault)
        XCTAssertFalse(state.isSubmissionBlocked)
        XCTAssertFalse(state.showSetAsProductDefaultCheckbox)
        XCTAssertFalse(state.shouldSetAsProductDefault)
        XCTAssertNil(state.submittedURL)
    }

    /// Product-without-default snapshot: mode flips to
    /// `.productHasNoDefault`, submission is blocked until a URL is
    /// supplied, and `submittedURL` is `nil` while the field is
    /// empty.
    func testFormStateSnapshotForProductWithoutDefault() {
        let state = WorkCreateRepoFormState(
            productRepoURL: nil,
            knownRepos: []
        )
        XCTAssertEqual(state.mode, .productHasNoDefault)
        XCTAssertFalse(state.overrideEnabled)
        XCTAssertEqual(state.enteredURL, "")
        XCTAssertFalse(state.setAsProductDefault)
        XCTAssertTrue(
            state.isSubmissionBlocked,
            "no-default mode must block submission while the repo field is empty"
        )
        XCTAssertFalse(state.showSetAsProductDefaultCheckbox)
        XCTAssertFalse(state.shouldSetAsProductDefault)
        XCTAssertNil(state.submittedURL)
    }

    /// Empty / whitespace-only product URL collapses to no-default
    /// mode — the engine round-trips empty as `None`, so the form
    /// must treat it the same as an unset column.
    func testFormStateTreatsWhitespaceProductURLAsNoDefault() {
        let state = WorkCreateRepoFormState(
            productRepoURL: "   ",
            knownRepos: []
        )
        XCTAssertEqual(state.mode, .productHasNoDefault)
    }

    /// In product-has-default mode, expanding the disclosure and
    /// typing a URL produces a `submittedURL` that overrides the
    /// inherited default.
    func testOverridingProductDefaultProducesSubmittedURL() {
        var state = WorkCreateRepoFormState(
            productRepoURL: "https://github.com/foo/bar.git",
            knownRepos: ["https://github.com/foo/bar.git"]
        )
        state.overrideEnabled = true
        state.enteredURL = "git@github.com:foo/nimbus.git"
        XCTAssertEqual(state.submittedURL, "git@github.com:foo/nimbus.git")
        XCTAssertFalse(state.isSubmissionBlocked)
    }

    /// In product-has-default mode, a closed disclosure leaves
    /// `submittedURL` at `nil` even when there's text in the field —
    /// the disclosure-state gate is intentional, so a half-typed
    /// override the user thought better of doesn't leak into the
    /// wire payload.
    func testClosedDisclosureSuppressesOverride() {
        var state = WorkCreateRepoFormState(
            productRepoURL: "https://github.com/foo/bar.git",
            knownRepos: []
        )
        state.overrideEnabled = false
        state.enteredURL = "git@github.com:foo/nimbus.git"
        XCTAssertNil(state.submittedURL)
    }

    /// In no-default mode with a *known* URL, the
    /// "Set as product default" checkbox stays hidden — the design
    /// only offers it on fresh URLs (URLs not already in the
    /// product's empirical known-repo set).
    func testSetAsProductDefaultHiddenForKnownURL() {
        var state = WorkCreateRepoFormState(
            productRepoURL: nil,
            knownRepos: ["https://github.com/foo/bar.git"]
        )
        state.enteredURL = "https://github.com/foo/bar.git"
        XCTAssertFalse(state.showSetAsProductDefaultCheckbox)
        XCTAssertFalse(state.shouldSetAsProductDefault)
        XCTAssertEqual(state.submittedURL, "https://github.com/foo/bar.git")
    }

    /// In no-default mode with a *fresh* URL the checkbox is shown,
    /// and toggling it on flips `shouldSetAsProductDefault` — which
    /// the view-model uses as the signal to piggyback an
    /// `update_work_item` patch on the parent product.
    func testSetAsProductDefaultActivatesForFreshURL() {
        var state = WorkCreateRepoFormState(
            productRepoURL: nil,
            knownRepos: ["https://github.com/foo/bar.git"]
        )
        state.enteredURL = "git@github.com:foo/nimbus.git"
        XCTAssertTrue(state.showSetAsProductDefaultCheckbox)
        XCTAssertFalse(state.shouldSetAsProductDefault)
        state.setAsProductDefault = true
        XCTAssertTrue(state.shouldSetAsProductDefault)
    }

    // MARK: - WorkItemRepoFieldCopy

    /// The required-mode placeholder must read as required so a
    /// glance at the field tells the user they need to supply a URL.
    func testRequiredPlaceholderSurfacesRequirement() {
        XCTAssertTrue(
            WorkItemRepoFieldCopy.requiredPlaceholder
                .localizedCaseInsensitiveContains("required"),
            "got: \(WorkItemRepoFieldCopy.requiredPlaceholder)"
        )
    }

    /// The override disclosure must carry the "Override" word so a
    /// user scanning a populated product knows what the affordance
    /// does.
    func testOverrideDisclosureLabelIsObvious() {
        XCTAssertTrue(
            WorkItemRepoFieldCopy.overrideDisclosureLabel
                .localizedCaseInsensitiveContains("override"),
            "got: \(WorkItemRepoFieldCopy.overrideDisclosureLabel)"
        )
    }

    // MARK: - ChatViewModel known-repos derivation

    /// `knownReposForProduct` mirrors the CLI's
    /// `known_repos_for_product`: the product default first, then
    /// distinct override URLs across the product's tasks and chores.
    func testKnownReposForProductReturnsProductDefaultAndOverrides() {
        let model = makeModel(
            productRepoURL: "https://github.com/foo/bar.git",
            taskOverrides: ["git@github.com:foo/nimbus.git", nil],
            choreOverrides: ["https://github.com/foo/ledger.git"]
        )
        let repos = model.knownReposForProduct("prod_test")
        XCTAssertEqual(repos.first, "https://github.com/foo/bar.git")
        XCTAssertEqual(Set(repos), [
            "https://github.com/foo/bar.git",
            "git@github.com:foo/nimbus.git",
            "https://github.com/foo/ledger.git",
        ])
    }

    /// An unknown product id yields an empty list rather than `nil`
    /// — the picker can iterate the result directly without
    /// special-casing absent products.
    func testKnownReposForUnknownProductIsEmpty() {
        let model = makeModel(
            productRepoURL: "https://github.com/foo/bar.git",
            taskOverrides: [nil],
            choreOverrides: []
        )
        XCTAssertEqual(model.knownReposForProduct("prod_missing"), [])
    }

    // MARK: - End-to-end interaction

    /// Toggling the override disclosure on a product *with* a
    /// default and typing a URL produces a `create_chore` payload
    /// that carries `repo_remote_url`. No product update — the
    /// "Set as product default" checkbox isn't offered in this mode.
    func testOverrideOnDefaultedProductLandsRepoOnWire() {
        let model = makeModel(
            productRepoURL: "https://github.com/foo/bar.git",
            taskOverrides: [nil],
            choreOverrides: []
        )
        let payloads = installRecorder(on: model)

        let request = WorkCreateRequest(kind: .chore(productID: "prod_test"))
        model.submitWorkCreateRequest(
            request,
            name: "tidy build files",
            description: "",
            repoRemoteURL: "git@github.com:foo/nimbus.git",
            setAsProductDefault: false
        )

        let creates = payloads.value.filter { ($0["type"] as? String) == "create_chore" }
        XCTAssertEqual(creates.count, 1)
        XCTAssertEqual(
            creates.first?["repo_remote_url"] as? String,
            "git@github.com:foo/nimbus.git"
        )

        let productPatches = payloads.value
            .filter { ($0["type"] as? String) == "update_work_item" }
        XCTAssertTrue(
            productPatches.isEmpty,
            "override on a defaulted product must not silently rewrite the product default"
        )
    }

    /// Submitting a fresh URL on a defaultless product with
    /// `setAsProductDefault = true` produces *two* wire payloads:
    /// the `create_chore` carrying `repo_remote_url`, and an
    /// `update_work_item` patch against the parent product that
    /// promotes the URL to the product default.
    func testFreshURLWithSetAsDefaultUpdatesProduct() {
        let model = makeModel(
            productRepoURL: nil,
            taskOverrides: [],
            choreOverrides: []
        )
        let payloads = installRecorder(on: model)

        let request = WorkCreateRequest(kind: .chore(productID: "prod_test"))
        model.submitWorkCreateRequest(
            request,
            name: "spin up first chore",
            description: "",
            repoRemoteURL: "git@github.com:foo/nimbus.git",
            setAsProductDefault: true
        )

        let creates = payloads.value.filter { ($0["type"] as? String) == "create_chore" }
        XCTAssertEqual(creates.count, 1)
        XCTAssertEqual(
            creates.first?["repo_remote_url"] as? String,
            "git@github.com:foo/nimbus.git"
        )

        let productPatches = payloads.value.filter { payload in
            guard (payload["type"] as? String) == "update_work_item" else { return false }
            return (payload["id"] as? String) == "prod_test"
        }
        XCTAssertEqual(productPatches.count, 1)
        let patch = productPatches.first?["patch"] as? [String: Any]
        XCTAssertEqual(patch?["repo_remote_url"] as? String, "git@github.com:foo/nimbus.git")
    }

    /// Same shape but for the task form: the override URL lands on
    /// `create_task`, and the product update fires when the user ticks
    /// the checkbox. Drives the same code path with a different
    /// `WorkCreateKind` to catch a regression that wired one verb but
    /// not the other.
    func testFreshURLOnTaskUpdatesProduct() {
        let model = makeModel(
            productRepoURL: nil,
            taskOverrides: [],
            choreOverrides: []
        )
        let payloads = installRecorder(on: model)

        let request = WorkCreateRequest(
            kind: .task(productID: "prod_test", projectID: "proj_test")
        )
        model.submitWorkCreateRequest(
            request,
            name: "ship a thing",
            description: "",
            repoRemoteURL: "git@github.com:foo/nimbus.git",
            setAsProductDefault: true
        )

        let creates = payloads.value.filter { ($0["type"] as? String) == "create_task" }
        XCTAssertEqual(creates.count, 1)
        XCTAssertEqual(
            creates.first?["repo_remote_url"] as? String,
            "git@github.com:foo/nimbus.git"
        )

        let productPatches = payloads.value.filter { payload in
            guard (payload["type"] as? String) == "update_work_item" else { return false }
            return (payload["id"] as? String) == "prod_test"
        }
        XCTAssertEqual(productPatches.count, 1)
    }

    /// A blank `repoRemoteURL` submission against a defaulted product
    /// must NOT emit `repo_remote_url` on the wire — letting the
    /// engine apply the product default via the absent-field
    /// inheritance path. This pins the "closed disclosure → no
    /// override" contract end-to-end.
    func testBlankRepoOnDefaultedProductOmitsField() {
        let model = makeModel(
            productRepoURL: "https://github.com/foo/bar.git",
            taskOverrides: [nil],
            choreOverrides: []
        )
        let payloads = installRecorder(on: model)

        let request = WorkCreateRequest(kind: .chore(productID: "prod_test"))
        model.submitWorkCreateRequest(
            request,
            name: "no-override chore",
            description: "",
            repoRemoteURL: "",
            setAsProductDefault: false
        )

        let creates = payloads.value.filter { ($0["type"] as? String) == "create_chore" }
        XCTAssertEqual(creates.count, 1)
        XCTAssertNil(creates.first?["repo_remote_url"])
    }

    // MARK: - Fixtures

    /// Closure-recorder wrapper. Tests read `payloads.value` after
    /// driving the model; the array is appended to in the order the
    /// model fires `sendLine` calls, so the test can assert relative
    /// ordering (chore-create before product-update).
    private final class PayloadRecorder {
        var value: [[String: Any]] = []
    }

    private func installRecorder(on model: ChatViewModel) -> PayloadRecorder {
        let recorder = PayloadRecorder()
        model.outboundRecorder = { payload in
            recorder.value.append(payload)
        }
        return recorder
    }

    private func makeTask(
        id: String,
        projectID: String?,
        kind: String,
        repoRemoteURL: String?
    ) -> WorkTask {
        WorkTask(
            id: id,
            productID: "prod_test",
            projectID: projectID,
            kind: kind,
            name: "Card \(id)",
            description: "",
            status: "todo",
            priority: "medium",
            ordinal: 1,
            prURL: nil,
            deletedAt: nil,
            createdAt: "2026-05-12T00:00:00Z",
            updatedAt: "2026-05-12T00:00:00Z",
            lastStatusActor: "human",
            createdVia: "mac_app",
            repoRemoteURL: repoRemoteURL
        )
    }

    /// Build a `ChatViewModel` populated with one product, one
    /// project, and the named override sets. The socket path points
    /// at a unique nonexistent file so the EngineClient's reconnect
    /// loop fires harmlessly in the background — the outbound
    /// recorder still captures every `sendLine` call.
    private func makeModel(
        productRepoURL: String?,
        taskOverrides: [String?],
        choreOverrides: [String?]
    ) -> ChatViewModel {
        let model = ChatViewModel(socketPath: "/tmp/boss-test-\(UUID().uuidString).sock")
        model.products = [
            WorkProduct(
                id: "prod_test",
                name: "Test Product",
                slug: "test",
                description: "",
                repoRemoteURL: productRepoURL,
                status: "active",
                createdAt: "2026-05-12T00:00:00Z",
                updatedAt: "2026-05-12T00:00:00Z"
            )
        ]
        model.selectedWorkProductID = "prod_test"
        model.projectsByProductID = [
            "prod_test": [
                WorkProject(
                    id: "proj_test",
                    productID: "prod_test",
                    name: "Test Project",
                    slug: "test",
                    description: "",
                    goal: "",
                    status: "active",
                    priority: "medium",
                    createdAt: "2026-05-12T00:00:00Z",
                    updatedAt: "2026-05-12T00:00:00Z"
                )
            ]
        ]
        let tasks = taskOverrides.enumerated().map { idx, repo in
            makeTask(
                id: "task_\(idx)",
                projectID: "proj_test",
                kind: "task",
                repoRemoteURL: repo
            )
        }
        model.tasksByProjectID = ["proj_test": tasks]
        let chores = choreOverrides.enumerated().map { idx, repo in
            makeTask(
                id: "chore_\(idx)",
                projectID: nil,
                kind: "chore",
                repoRemoteURL: repo
            )
        }
        model.choresByProductID = ["prod_test": chores]
        return model
    }
}
