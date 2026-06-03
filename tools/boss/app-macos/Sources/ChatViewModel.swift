import Foundation
import os
#if canImport(AppKit)
import AppKit
#endif

private let designDocTimingLog = Logger(subsystem: "com.boss.app", category: "DesignDocTiming")

@MainActor
final class ChatViewModel: ObservableObject {
    @Published var navigationMode: NavigationMode = .agents
    @Published var isConnected: Bool = false
    /// Full product list as reported by the engine, including archived
    /// rows. Keep the full set so id-based lookups (`product(withID:)`,
    /// work-tree merges) still resolve when a product was archived in
    /// another session; surfaces that let the user *select* a product
    /// should read [[activeProducts]] instead.
    @Published var products: [WorkProduct] = []

    /// Non-archived subset of [[products]], in the same sort order.
    /// This is what the sidebar Product picker, the Designs picker, and
    /// any other "products I work in actively" surface should bind to —
    /// archived products are history, not selection targets. Mirrors the
    /// CLI split: `boss product list` shows everything; the picker is
    /// for live products only.
    var activeProducts: [WorkProduct] {
        products.filter { $0.status != "archived" }
    }
    @Published var projectsByProductID: [String: [WorkProject]] = [:] {
        didSet { invalidateWorkCache() }
    }
    @Published var tasksByProjectID: [String: [WorkTask]] = [:] {
        didSet { invalidateWorkCache() }
    }
    @Published var choresByProductID: [String: [WorkTask]] = [:] {
        didSet { invalidateWorkCache() }
    }
    /// Revisions whose chain root is a chore. A revision inherits its
    /// `project_id` from the chain root (`insert_revision_in_tx`), so a
    /// chore-parented revision has none and cannot live in
    /// `tasksByProjectID`. Keyed by product so these rows still render as
    /// standalone Backlog/Doing cards and roll up under the parent chore's
    /// Review card. Without this bucket they were silently dropped at
    /// work-tree reception and invisible in the kanban (issue #789).
    @Published var productLevelRevisionsByProductID: [String: [WorkTask]] = [:] {
        didSet { invalidateWorkCache() }
    }
    /// Product-level work items (`project_id IS NULL`) that are neither
    /// chores nor revisions — `kind == "investigation"` today, and any
    /// future product-level kind the engine emits. The work-tree handler
    /// used to drop every non-revision product-level row on the floor,
    /// so an investigation with no project was invisible on the board even
    /// while a live worker produced against it (issue #886). Routing the
    /// catch-all here makes the omission impossible by construction: a new
    /// kind lands in a real bucket and renders instead of vanishing.
    @Published var productLevelTasksByProductID: [String: [WorkTask]] = [:] {
        didSet { invalidateWorkCache() }
    }
    @Published var taskRuntimesByID: [String: WorkTaskRuntime] = [:]
    /// Dependency edges keyed by product. Refreshed whenever the engine
    /// pushes a fresh `WorkTree` for that product. The kanban joins
    /// these against the task/chore/project name maps to render
    /// "Blocked by <prereq title>" on gated cards.
    @Published var dependenciesByProductID: [String: [WorkItemDependency]] = [:]
    /// Attention items keyed by work-item id (product id for external-tracker
    /// items). Populated on product selection and on every workTree refresh.
    @Published var attentionItemsByWorkItemID: [String: [WorkAttentionItem]] = [:]
    /// Attention *groups* keyed by product id — the agent-authored
    /// notification feature (attentions.md), distinct from the operational
    /// `attentionItemsByWorkItemID` store above. Loaded on product selection /
    /// work-tree refresh and kept live via `AttentionCreated` /
    /// `AttentionGroupUpdated` / `AttentionGroupActioned` pushes. Holds open
    /// groups plus any that flipped to actioned/dismissed this session (so the
    /// produced-artifact link lingers until the next full reload).
    @Published var attentionGroupsByProductID: [String: [AttentionGroup]] = [:]
    /// Attention group *members* keyed by `AttentionGroup.id`, in display
    /// order. Populated alongside [[attentionGroupsByProductID]].
    @Published var attentionMembersByGroupID: [String: [Attention]] = [:]
    /// Historical execution rows keyed by task id. Populated on demand when
    /// the transcript viewer window sends `list_executions`. Cleared per-task
    /// before each fresh fetch so the viewer never shows stale rows.
    @Published var executionsByTaskID: [String: [ExecutionVM]] = [:]
    /// Transcript load state keyed by execution id. Populated on demand when
    /// the transcript viewer selects an execution (`execution_transcript`
    /// RPC). A `nil` (absent) entry means "not requested yet"; live
    /// executions can be re-fetched via [[refreshTranscript(executionId:)]].
    @Published var transcriptsByExecutionID: [String: TranscriptLoadState] = [:]
    /// Automations keyed by product id. Loaded when the Automations tab is
    /// entered or the selected product changes while the tab is active.
    @Published var automationsByProductID: [String: [AppAutomation]] = [:]
    /// Open-task counts keyed by automation id. Refreshed alongside the list.
    @Published var openTaskCountByAutomationID: [String: Int] = [:]
    /// Run history keyed by automation id. Fetched on selection and refreshed
    /// when the automation's state changes (outcome updated, etc.).
    @Published var automationRunsByID: [String: [AppAutomationRun]] = [:]
    /// The automation currently selected in the Automations tab detail pane.
    @Published var selectedAutomationID: String?
    @Published var selectedWorkProductID: String? {
        didSet { invalidateWorkCache() }
    }
    @Published var selectedProjectFilterIDs: Set<String> = [] {
        didSet { invalidateWorkCache() }
    }
    /// When true, the board shows only chores (project-less tasks and their
    /// revisions). Mutually exclusive with `selectedProjectFilterIDs`.
    @Published var filterToChoresOnly: Bool = false {
        didSet { invalidateWorkCache() }
    }
    @Published var includeChores: Bool = true {
        didSet { invalidateWorkCache() }
    }
    @Published var showBlockedOnly: Bool = false {
        didSet { invalidateWorkCache() }
    }
    @Published var showArchivedProjects: Bool = false {
        didSet { invalidateWorkCache() }
    }
    @Published var selectedWorkCardID: String?
    /// Task id that the reveal animation is currently highlighting.
    /// Set by `revealWorkCard`; cleared after 1.5 s. Views observe
    /// this to apply a transient border-glow overlay on the matching
    /// card.
    @Published var revealHighlightID: String?
    /// Set of task IDs that should be highlighted as the actionable
    /// prerequisite frontier when the pointer is over a Dependency
    /// badge. Computed by `setDepBadgeHover`; cleared when the pointer
    /// leaves the badge. Views observe this to apply a transient
    /// amber border on every frontier card.
    @Published var depFrontierHighlightIDs: Set<String> = []
    /// Set of revision task IDs to highlight when the pointer is over an
    /// "In revision" badge. Computed by `setRevisionBadgeHover`; cleared
    /// on pointer exit. Uses the same green-border overlay as dep frontier.
    @Published var revisionHighlightIDs: Set<String> = []
    /// Task id that scroll views should bring into the visible area.
    /// Set by `revealWorkCard`; cleared after a short delay once the
    /// scroll has been triggered. Views observe this via `.onChange`
    /// on their `ScrollViewReader` proxies.
    @Published var revealScrollTarget: String?
    /// Task id whose card should be scrolled to once its product's
    /// work tree arrives. Used when a reveal crosses a product
    /// boundary — `revealWorkCard` sets this and the `workTree`
    /// event handler promotes it to `revealScrollTarget`.
    var pendingRevealScrollID: String?
    @Published var workBoardGrouping: WorkBoardGrouping = .none {
        didSet { invalidateWorkCache() }
    }
    @Published var selectedWorkNodeID: WorkNodeID?
    @Published var pendingWorkCreateRequest: WorkCreateRequest?
    @Published var pendingWorkEditRequest: WorkEditRequest?
    @Published var workErrorMessage: String?
    @Published var workSearchText: String = "" {
        didSet { invalidateWorkCache() }
    }
    @Published var isBossPanelCollapsed: Bool = false
    @Published var bossPanelWidth: CGFloat = 380
    /// Live runtime state for every active worker, sourced from the
    /// engine's LiveWorkerState snapshot (`worker_live_states_list`
    /// event) and refreshed on each push from the `worker.live_states`
    /// topic. Drives the kanban Doing icon (working / waiting / idle
    /// / errored) and the per-pane titlebar pill — replaces the
    /// screen-scrape-only signal that always rendered "Claude
    /// Unknown".
    ///
    /// Held on its own `ObservableObject` so the high-rate hook
    /// traffic that drives this snapshot doesn't invalidate every
    /// view that observes `ChatViewModel` (toolbar, sidebar, Boss
    /// panel, ContentView root). Only the views that actually read
    /// live state subscribe to the store.
    let liveWorkerStates = LiveWorkerStateStore()

    /// Slot ids whose live-status summarizer has been manually
    /// disabled by the human via the Agents-tab toggle. Sourced from
    /// `list_live_status_disabled_slots` at session start and kept
    /// in sync via `live_status_enabled_set` echoes. Persisted on
    /// the engine side so this is purely a UI mirror.
    @Published var liveStatusDisabledSlotIDs: Set<Int> = []

    /// Per-installation settings snapshot, sourced from `get_settings`
    /// on Settings window open and kept in sync via `setting_set`
    /// echoes after every toggle. Empty until the Settings window is
    /// first opened in this session.
    @Published var engineSettings: [EngineSetting] = []

    /// Engine-side configuration health issues sourced from
    /// `get_engine_health` at session start. Empty means the engine
    /// is healthy. Non-empty drives the top-of-window
    /// `EngineHealthBanner` and the Settings-pane warning so a
    /// missing `ANTHROPIC_API_KEY` (or any later "missing config"
    /// surface) is impossible to miss (#699).
    @Published var engineHealthIssues: [EngineHealthIssue] = []
    /// Top-level mirror of the same `get_engine_health` reply. Surfaced
    /// in the Settings pane next to the (future) API-key field so
    /// "key set" / "key missing" is legible without parsing the
    /// `issues` list. `true` until the engine answers at least once,
    /// so the banner doesn't flash on a transient reconnect.
    @Published var engineAnthropicApiKeyPresent: Bool = true

    /// Engine metrics snapshot — every registered counter and gauge —
    /// sourced from `metrics_list_live` on Metrics pane open and
    /// refreshed by the pane's 5-second polling timer. Empty until the
    /// pane has been opened in this session.
    @Published var engineMetrics: [EngineMetric] = []

    /// Engine feature-flag snapshot, sourced from `list_feature_flags`
    /// on debug-pane open and kept in sync via `feature_flag_set`
    /// echoes after every toggle. Backs the Feature Flags window
    /// (incident 001 AI #5). Empty when the pane has never been opened
    /// in this session.
    @Published var featureFlags: [FeatureFlag] = []

    /// Current GitHub OAuth auth state for github.com (OAuth device-flow
    /// design §3/§4). The engine owns a single per-host state; the app
    /// subscribes to the `github.auth` topic and refreshes this on every
    /// `git_hub_auth_state` push as the device flow advances. Backs the
    /// "GitHub account" subsection of the external-tracker settings.
    /// Defaults to `.disconnected` until the engine's first reply lands.
    @Published var gitHubAuthState: GitHubAuthState = .disconnected

    /// Resolved design-doc pointer state per project. Populated lazily
    /// when a project surface (kanban project header, future detail
    /// view) calls `resolveProjectDesignDoc(_:)`; refreshed whenever
    /// the engine pushes a fresh `WorkTree` so a re-pointing or unset
    /// from another session lands in the icon without a manual reload.
    /// A missing entry means "we haven't asked yet" — the affordance
    /// stays hidden until the engine replies.
    @Published var designDocStateByProjectID: [String: ProjectDesignDocState] = [:]
    /// In-flight resolve-RPC batch. The engine resolves design-doc
    /// pointers in lock-step (responses arrive back-to-back regardless of
    /// per-project work), so stamping each project with its own
    /// start-to-response delta produces N near-identical numbers and
    /// destroys per-project attribution. Instead we track one batch per
    /// `refreshDesignDocStates` call and emit a single
    /// `phase=resolve project=batch count=<n>` summary when the last
    /// pending response arrives. Stray responses for projects outside the
    /// current batch (a refresh that landed mid-flight) still update
    /// state — they just don't drive timing.
    private struct DesignDocResolveBatch {
        var startDate: Date
        var pendingProjectIDs: Set<String>
        let initialCount: Int
    }
    private var currentDesignDocResolveBatch: DesignDocResolveBatch?

    /// Engine-tab attempt list, freshest first. Refreshed on Engine-tab
    /// entry, on `conflict_resolution_*` topic pushes, and on `Refresh`
    /// button taps. Phase 5 #14 of the merge-conflict design.
    @Published var conflictResolutions: [WorkConflictResolution] = [] {
        didSet { invalidateWorkCache() }
    }

    /// Engine-tab CI-remediation attempt list, freshest first.
    /// Mirror of [[conflictResolutions]]; refreshed on Engine-tab
    /// entry, on `ci_remediation_*` topic pushes, and on `Refresh`
    /// button taps. Phase 11 #37 of the merge-conflict design.
    @Published var ciRemediations: [WorkCiRemediation] = [] {
        didSet { invalidateWorkCache() }
    }

    /// PR URLs whose most recent CI-remediation attempt succeeded,
    /// with the wall-clock timestamp the engine reported (or the local
    /// observation time as a fallback). Drives the `"✅ ci auto-fixed"`
    /// PR-card chip per design Q11; cards whose PR sits in this map
    /// with an age under [[badgeFreshnessWindow]] render the chip.
    @Published var recentlyClearedCIPRs: [String: Date] = [:]

    /// Per-PR snapshot of the most recent observed CI exhaustion event.
    /// Carries the (used, budget) pair the engine sent so the kanban
    /// card can render `🟧 ci failing (used/budget)` or
    /// `🛑 ci failing (exhausted)` chips per design Q11. Cleared from
    /// the front of the map when the matching PR returns to
    /// `in_review` (observed via `ciRemediationSucceeded`).
    @Published var ciFailureBadges: [String: CiFailureBadge] = [:]

    /// `true` when this PR has a CI auto-fix that landed inside the
    /// badge window. Cards bind to this on the `Identifiable` task
    /// id; non-PR cards always return `false`.
    func showsCIAutoFixedBadge(forPR prURL: String?) -> Bool {
        guard let prURL,
              let clearedAt = recentlyClearedCIPRs[prURL] else {
            return false
        }
        return Date().timeIntervalSince(clearedAt) < badgeFreshnessWindow
    }

    /// CI-fail / exhausted chip for a PR card. `nil` when no active CI
    /// remediation is in flight (or budget exhaustion has not been
    /// observed). Cards bind to this on the `Identifiable` task id.
    func ciFailureBadge(forPR prURL: String?) -> CiFailureBadge? {
        guard let prURL else { return nil }
        return ciFailureBadges[prURL]
    }

    /// PR URLs whose most recent conflict-resolution attempt succeeded,
    /// with the wall-clock timestamp the engine reported (or the local
    /// observation time as a fallback). Drives the
    /// `"🔧 conflict cleared"` PR-card badge: cards whose PR sits in
    /// this map with an age under [[badgeFreshnessWindow]] render the
    /// chip. Phase 5 #15.
    @Published var recentlyClearedConflictPRs: [String: Date] = [:]

    /// 24-hour rolling window for the PR-card "🔧 conflict cleared"
    /// chip. Matches the auto-rebase-stacked-prs.md Q7 cadence so the
    /// two surfaces feel symmetric.
    static let conflictBadgeFreshnessWindow: TimeInterval = 24 * 60 * 60

    var badgeFreshnessWindow: TimeInterval { Self.conflictBadgeFreshnessWindow }

    /// `true` when this PR's most recent successful conflict-resolution
    /// landed inside the badge window. Cards bind to this on the
    /// `Identifiable` task id; non-PR cards always return `false`.
    func showsConflictClearedBadge(forPR prURL: String?) -> Bool {
        guard let prURL,
              let clearedAt = recentlyClearedConflictPRs[prURL] else {
            return false
        }
        return Date().timeIntervalSince(clearedAt) < badgeFreshnessWindow
    }

    /// Indirection for the OS URL opener used by [[openProjectDesignDoc(_:)]].
    /// Production defaults to `NSWorkspace.shared.open`; tests inject a
    /// recording stub so a `.resolved` click never hands a real GitHub
    /// blob URL to the OS during `swift test`. A test that fires the
    /// resolved branch without overriding this *will* pop the user's
    /// browser — see `ProjectDesignDocAffordanceTests` for the stub
    /// pattern.
    var urlOpener: (URL) -> Void = { url in
        #if canImport(AppKit)
        NSWorkspace.shared.open(url)
        #endif
    }

    /// Indirection for opening the in-app `DesignRendererView` window.
    /// Installed by [[ContentView]] using `@Environment(\.openWindow)`
    /// — the view model can't reach the SwiftUI environment directly,
    /// so the closure crosses the boundary. `nil` (the default for
    /// tests and headless contexts) falls the dispatcher back to the
    /// legacy `urlOpener(fileURL)` path that hands the file to the
    /// OS-registered `.md` handler.
    ///
    /// Wiring this closure is what swaps the project-card click
    /// affordance from `$EDITOR` to the in-app Textual renderer —
    /// chore #12 of [[project-design-doc-pointer.md]] and Q9's
    /// renderer-reuse acceptance.
    var designRendererOpener: ((DesignRendererContent) -> Void)?

    /// Indirection for opening the markdown-viewer window with fetched
    /// content. Installed by [[ContentView]] using
    /// `@Environment(\.openWindow)` — same boundary-crossing pattern as
    /// [[designRendererOpener]]. Used when the design doc lives on a PR
    /// branch (not yet on `main`) and no leased workspace is available:
    /// the dispatcher fetches the raw content via [[rawContentFetcher]]
    /// and hands the rendered string to this opener. `nil` (tests and
    /// headless contexts) falls back to `urlOpener`.
    var markdownViewerOpener: ((MarkdownViewerContent) -> Void)?

    /// Indirection for opening the `"async-markdown-viewer"` Window
    /// immediately, before the design doc has been fetched. Installed by
    /// [[ContentView]] via `@Environment(\.openWindow)`. When set, the
    /// raw-content path opens the window first (loading state) then
    /// resolves content into [[asyncMarkdownViewerVM]]. `nil` (tests and
    /// headless) falls back to the legacy fetch-then-open path via
    /// [[markdownViewerOpener]].
    var asyncMarkdownViewerOpener: (() -> Void)?

    /// Shared state for the `"async-markdown-viewer"` Window scene.
    /// The window observes this object to transition from loading →
    /// loaded/failed without needing to pass content through the
    /// `openWindow` value type.
    let asyncMarkdownViewerVM = AsyncMarkdownViewerViewModel()

    /// Indirection for fetching raw markdown content from a URL.
    /// Production default routes through [[GitHubContentFetcher]] so
    /// the request authenticates as the user's active `gh` session and
    /// works for private repos. An unauthenticated `URLSession` fetch
    /// against `raw.githubusercontent.com` returns 404 for any private
    /// repo (issue #732), so this path must never reach `URLSession`.
    /// Tests inject a stub so the affordance tests never shell out.
    var rawContentFetcher: (URL) async throws -> String = { url in
        try await GitHubContentFetcher.fetch(url)
    }

    /// Indirection for opening the review-terminal window. Installed by
    /// [[ContentView]] using `@Environment(\.openWindow)`. Called on
    /// click (before the engine responds) so the window opens immediately
    /// in a loading state. `nil` in tests and headless contexts.
    var reviewTerminalOpener: (() -> Void)?

    /// Shared state for the `"review-terminal"` Window scene. Owned here
    /// and injected via EnvironmentObject so the window can observe the
    /// loading → ready transition without going through a value-type
    /// openWindow payload (which can't be updated after the window opens).
    let reviewTerminalVM = ReviewTerminalViewModel()

    /// Work item IDs for which `open_review_terminal` has been sent but
    /// `review_terminal_ready` (or `work_error`) has not yet arrived.
    /// Guards against a second click while the engine is still leasing.
    private var openingReviewTerminalIDs: Set<String> = []

    /// Work item IDs for which `merge_when_ready` has been sent but
    /// `merge_when_ready_accepted` (or `work_error`) has not yet arrived.
    /// Guards against a duplicate tap while the engine is running the merge.
    private var mergingWhenReadyIDs: Set<String> = []

    /// Ask the engine to merge (or queue for merging) the PR for the given
    /// Review-column task. Guards against a duplicate tap while the RPC is
    /// in flight. The engine runs `gh pr merge --auto --squash` and kicks
    /// the PR-reconciler so the kanban state updates promptly on success.
    func mergeWhenReady(for task: WorkTask) {
        guard let prURL = task.prURL, !prURL.isEmpty else { return }
        _ = prURL  // consumed by the engine; kept here for the guard above
        guard !mergingWhenReadyIDs.contains(task.id) else { return }
        mergingWhenReadyIDs.insert(task.id)
        engine.sendMergeWhenReady(workItemID: task.id)
    }

    /// Ask the engine to lease a workspace for the given Review-column
    /// task's PR branch and open a terminal there. Opens the window
    /// immediately with a loading spinner; the terminal becomes live once
    /// the engine sends back `ReviewTerminalReady`.
    func openReviewTerminal(for task: WorkTask) {
        guard let prURL = task.prURL, !prURL.isEmpty else { return }
        guard !openingReviewTerminalIDs.contains(task.id) else {
            // Same task still loading — just re-focus the window.
            reviewTerminalOpener?()
            return
        }
        reviewTerminalVM.state = .loading(taskName: task.name)
        reviewTerminalOpener?()
        openingReviewTerminalIDs.insert(task.id)
        engine.sendOpenReviewTerminal(workItemID: task.id)
    }

    /// Notify the engine that the review terminal for `leaseID` has
    /// closed so the workspace lease can be released. Called from the
    /// `ReviewTerminalView.onDisappear` handler.
    func releaseReviewTerminal(leaseID: String) {
        engine.sendReleaseReviewTerminal(leaseID: leaseID)
    }

    /// Fetch the execution history for `taskId` from the engine.
    /// Clears any cached rows first so the viewer shows a loading state.
    /// The engine replies with an `executions_list` event that populates
    /// [[executionsByTaskID]].
    func loadExecutions(taskId: String) {
        executionsByTaskID[taskId] = nil
        engine.sendListExecutions(taskId: taskId)
    }

    /// Fetch the rendered transcript for `executionId` the first time it is
    /// requested. Selecting an execution in the viewer calls this; an
    /// already-loaded, in-flight, or unavailable transcript is left
    /// untouched so re-selecting a row doesn't re-hit the engine. Use
    /// [[refreshTranscript(executionId:)]] to force a re-fetch.
    func loadTranscript(executionId: String) {
        if transcriptsByExecutionID[executionId] != nil { return }
        transcriptsByExecutionID[executionId] = .loading
        engine.sendExecutionTranscript(executionId: executionId)
    }

    /// Force a re-fetch of `executionId`'s transcript — the "Refresh"
    /// affordance on a still-running (live) execution.
    func refreshTranscript(executionId: String) {
        transcriptsByExecutionID[executionId] = .loading
        engine.sendExecutionTranscript(executionId: executionId)
    }

    /// Toggle the live-status summarizer for `slotId`. Sends the
    /// RPC and optimistically updates local state; the engine echo
    /// brings the two back in sync.
    func setLiveStatusEnabled(slotId: Int, enabled: Bool) {
        if enabled {
            liveStatusDisabledSlotIDs.remove(slotId)
        } else {
            liveStatusDisabledSlotIDs.insert(slotId)
        }
        engine.sendSetLiveStatusEnabled(slotId: slotId, enabled: enabled)
    }

    /// `true` if the live-status summarizer is currently enabled for
    /// `slotId`. Defaults to enabled — the disabled set is the
    /// minority case.
    func isLiveStatusEnabled(slotId: Int) -> Bool {
        !liveStatusDisabledSlotIDs.contains(slotId)
    }

    /// Ask the engine for the current per-installation settings
    /// snapshot. Called by the Settings window on appear.
    func refreshSettings() {
        engine.sendGetSettings()
    }

    /// Ask the engine for a fresh engine-health snapshot. Also called
    /// on every reconnect from the `.connected` arm of `handle`; this
    /// wrapper exists so the Settings pane can re-poll on appear
    /// without exposing the private `engine` field.
    func refreshEngineHealth() {
        engine.sendGetEngineHealth()
    }

    /// Toggle one per-installation setting. Optimistically patches the
    /// cached snapshot so the UI feels instantaneous; the engine's
    /// `setting_set` echo reconciles state once the on-disk write
    /// returns.
    func setEngineSetting(key: String, enabled: Bool) {
        if let idx = engineSettings.firstIndex(where: { $0.key == key }) {
            let prior = engineSettings[idx]
            engineSettings[idx] = EngineSetting(
                key: prior.key,
                description: prior.description,
                defaultEnabled: prior.defaultEnabled,
                enabled: enabled
            )
        }
        engine.sendSetSetting(key: key, enabled: enabled)
    }

    /// Ask the engine for a fresh snapshot of every registered metric.
    /// Called by the Metrics debug pane on appear and by its 5-second
    /// polling timer so values refresh without a manual reload.
    func refreshMetrics() {
        engine.sendMetricsListLive()
    }

    /// Ask the engine for the current feature-flag snapshot. Called by
    /// the Feature Flags debug pane on appear so the rendered state
    /// reflects whatever the engine has persisted (which may differ
    /// from what an earlier session in this app saw).
    func refreshFeatureFlags() {
        engine.sendListFeatureFlags()
    }

    /// Toggle a feature flag. Optimistically patches the cached
    /// snapshot so the UI feels instantaneous; the engine's
    /// `feature_flag_set` echo reconciles state once the on-disk
    /// write returns. If the engine rejects the call (unknown flag,
    /// IO error), the echo never arrives and the `work_error` path
    /// surfaces the failure — the next `refreshFeatureFlags()` corrects
    /// the optimistic UI state.
    func setFeatureFlag(name: String, enabled: Bool) {
        if let idx = featureFlags.firstIndex(where: { $0.name == name }) {
            let prior = featureFlags[idx]
            featureFlags[idx] = FeatureFlag(
                name: prior.name,
                description: prior.description,
                category: prior.category,
                defaultEnabled: prior.defaultEnabled,
                enabled: enabled
            )
        }
        engine.sendSetFeatureFlag(name: name, enabled: enabled)
    }

    var selectedProduct: WorkProduct? {
        guard let productID = currentSelectedProductID else { return nil }
        return product(withID: productID)
    }

    /// Automations for the currently selected product, ordered by `created_at`.
    var automationsForSelectedProduct: [AppAutomation] {
        guard let productID = currentSelectedProductID else { return [] }
        return automationsByProductID[productID] ?? []
    }

    /// The currently selected automation, looked up from the per-product list.
    var selectedAutomation: AppAutomation? {
        guard let id = selectedAutomationID else { return nil }
        return automationsForSelectedProduct.first { $0.id == id }
    }

    /// Unresolved attention items for the currently selected product.
    var selectedProductOpenAttentionItems: [WorkAttentionItem] {
        guard let productID = currentSelectedProductID else { return [] }
        return (attentionItemsByWorkItemID[productID] ?? []).filter { $0.resolvedAt == nil }
    }

    /// All known attention groups for the selected product (open plus any
    /// recently actioned/dismissed this session), newest-first.
    var selectedProductAttentionGroups: [AttentionGroup] {
        guard let productID = currentSelectedProductID else { return [] }
        return (attentionGroupsByProductID[productID] ?? [])
            .sorted { $0.createdAt > $1.createdAt }
    }

    /// Open (actionable) attention groups for the selected product — the
    /// Notifications window's primary list and the toolbar badge source.
    var selectedProductOpenAttentionGroups: [AttentionGroup] {
        selectedProductAttentionGroups.filter(\.isOpen)
    }

    /// Count of open attention groups for the selected product. Drives the
    /// Notifications toolbar bell badge (hidden when 0).
    var openAttentionGroupCount: Int {
        selectedProductOpenAttentionGroups.count
    }

    /// Members of a group, in display order.
    func attentionMembers(forGroup groupID: String) -> [Attention] {
        (attentionMembersByGroupID[groupID] ?? []).sorted { $0.ordinal < $1.ordinal }
    }

    var selectedProject: WorkProject? {
        guard selectedProjectFilterIDs.count == 1,
              let projectID = selectedProjectFilterIDs.first else { return nil }
        return project(withID: projectID)
    }

    var projectFilterDescription: String {
        if filterToChoresOnly { return "Chores only" }
        let visibleSelected = visibleSelectedProjectFilterIDs
        switch visibleSelected.count {
        case 0:
            return "All projects"
        case 1:
            if let id = visibleSelected.first, let project = self.project(withID: id) {
                return project.name
            }
            return "1 project"
        case let count:
            return "\(count) projects"
        }
    }

    var hasProjectFilters: Bool {
        !visibleSelectedProjectFilterIDs.isEmpty || filterToChoresOnly
    }

    /// Subset of `selectedProjectFilterIDs` whose projects are currently
    /// visible in the sidebar. When archived projects are hidden, their
    /// IDs may still be in the filter set (so toggling Show Archived
    /// back on restores the prior selection), but counts and badges
    /// must only reflect what the user can see.
    private var visibleSelectedProjectFilterIDs: Set<String> {
        guard !selectedProjectFilterIDs.isEmpty else { return [] }
        let visibleIDs = Set(projectsForSelectedProduct.map(\.id))
        return selectedProjectFilterIDs.intersection(visibleIDs)
    }

    var selectedTask: WorkTask? {
        guard let taskID = selectedWorkCardID else { return nil }
        return task(withID: taskID)
    }

    var projectsForSelectedProduct: [WorkProject] {
        let all = allProjectsForSelectedProduct
        guard !showArchivedProjects else { return all }
        return all.filter { $0.status != "archived" }
    }

    /// Unfiltered project list for the selected product, used by code
    /// paths that need full visibility regardless of the sidebar's
    /// Show Archived toggle (e.g. boss-agent context where the LLM
    /// must know archived projects exist so it doesn't recreate them).
    var allProjectsForSelectedProduct: [WorkProject] {
        guard let productID = currentSelectedProductID else { return [] }
        return (projectsByProductID[productID] ?? []).sorted(by: projectSort)
    }

    var visibleWorkItems: [WorkTask] {
        if let cached = cachedVisibleItems {
            return cached
        }
        let computed = computeVisibleWorkItems()
        cachedVisibleItems = computed
        return computed
    }

    /// Repo names (lowercased) that resolve to more than one org across
    /// the currently visible card set's PR URLs. Drives the board-local
    /// disambiguation rule for kanban PR-link labels: a repo name in
    /// this set must render as `org/repo#n`; everything else can drop
    /// to the bare `repo#n`. Cached on the same lifetime as
    /// [[visibleWorkItems]] — invalidated by [[invalidateWorkCache]].
    var ambiguousVisibleRepoNames: Set<String> {
        if let cached = cachedAmbiguousRepoNames {
            return cached
        }
        let computed = ambiguousPRRepoNames(in: visibleWorkItems)
        cachedAmbiguousRepoNames = computed
        return computed
    }

    /// The active board search query with surrounding whitespace removed,
    /// or `nil` when no search filter is in effect. Single source of truth
    /// for both the filter logic below and the persistent "filtered view"
    /// banner so the two can never disagree about whether the board is
    /// showing a subset (issue #1248).
    var activeWorkSearchQuery: String? {
        let query = workSearchText.trimmingCharacters(in: .whitespacesAndNewlines)
        return query.isEmpty ? nil : query
    }

    /// True while a free-text search is hiding non-matching cards. Drives
    /// the kanban filter banner so a stale search can't be mistaken for an
    /// empty or complete board.
    var isWorkSearchActive: Bool { activeWorkSearchQuery != nil }

    private func computeVisibleWorkItems() -> [WorkTask] {
        guard let productID = currentSelectedProductID else { return [] }

        let query = workSearchText.trimmingCharacters(in: .whitespacesAndNewlines)

        var items: [WorkTask] = []
        if filterToChoresOnly {
            items.append(contentsOf: (choresByProductID[productID] ?? []).sorted(by: taskSort))
            items.append(contentsOf: (productLevelRevisionsByProductID[productID] ?? []).sorted(by: taskSort))
        } else {
            let projectFilter = visibleSelectedProjectFilterIDs
            for project in projectsForSelectedProduct {
                guard projectFilter.isEmpty || projectFilter.contains(project.id) else { continue }
                items.append(contentsOf: (tasksByProjectID[project.id] ?? []).sorted(by: taskSort))
            }
            // Product-level work items (investigations, etc.) have no project, so a
            // project filter legitimately excludes them; otherwise they always
            // render. They are first-class work — not gated by the chores toggle,
            // which would otherwise hide an investigation a live worker is
            // producing against (issue #886).
            if projectFilter.isEmpty {
                items.append(contentsOf: (productLevelTasksByProductID[productID] ?? []).sorted(by: taskSort))
            }
            if includeChores && projectFilter.isEmpty {
                items.append(contentsOf: (choresByProductID[productID] ?? []).sorted(by: taskSort))
                // Chore-parented revisions have no project of their own; surface
                // them with the chores so their Backlog/Doing cards appear. The
                // in-review ones are rolled up under the parent and filtered out
                // of the Review column by `workItems(in:)`.
                items.append(contentsOf: (productLevelRevisionsByProductID[productID] ?? []).sorted(by: taskSort))
            }
        }

        // Automation-sourced chores are real work items that need human review.
        // They appear on the kanban like any other chore — the card detail view
        // marks them with a purple wand icon to indicate automation provenance.
        // Do NOT filter them out here: a chore in in_review status needs to
        // be visible so the operator can review and merge the PR.

        if showBlockedOnly {
            items = items.filter { $0.status == "blocked" }
        }

        guard !query.isEmpty else {
            return items
        }

        return items.filter { item in
            item.name.localizedCaseInsensitiveContains(query)
                || item.description.localizedCaseInsensitiveContains(query)
                || (item.prURL?.localizedCaseInsensitiveContains(query) ?? false)
                || (projectName(for: item.projectID)?.localizedCaseInsensitiveContains(query) ?? false)
                || item.status.localizedCaseInsensitiveContains(query)
        }
    }

    let engine: EngineClient
    /// Test-only hook: forwarded to `EngineClient.outboundRecorder`
    /// so an XCTest can assert that the form's submit lands the
    /// expected `repo_remote_url` on the wire. The real socket write
    /// still runs (against a stub path that fails harmlessly in
    /// tests).
    var outboundRecorder: (([String: Any]) -> Void)? {
        get { engine.outboundRecorder }
        set { engine.outboundRecorder = newValue }
    }
    private let processController: EngineProcessController
    private let paths: BossEnginePaths
    private let socketPath: String
    private let showSystemMessages: Bool
    private var didStart = false
    private var didStartEngine = false
    /// Becomes `true` the first time the socket reaches `.ready`. The
    /// Disconnected banner reads this so it stays hidden during the
    /// short initial-connect window (avoiding a flash on launch) and
    /// only appears once the engine has been reachable at least once.
    @Published private(set) var hasConnectedOnce = false
    private var subscribedWorkTopics: Set<String> = []
    private let defaults = UserDefaults.standard

    /// Notification manager for Review-lane transitions. Fires a system
    /// banner when a task reaches `in_review` while the app is backgrounded.
    private let reviewNotifier = ReviewNotificationCenter()
    #if canImport(AppKit)
    private var appActivationObserver: NSObjectProtocol?
    #endif

    /// Task IDs currently known to be in `in_review`. Populated from
    /// work-tree snapshots (without firing) on load/reconnect, and
    /// updated incrementally on `workItemUpdated` events. Guards against
    /// re-notifying for a task that was already in Review when the app
    /// launched or re-subscribed.
    var knownReviewTaskIDs: Set<String> = []

    private let navigationModeDefaultsKey = "boss.navigationMode"
    private let selectedWorkProductDefaultsKey = "boss.work.selectedProductID"
    private let selectedProjectFilterIDsDefaultsKey = "boss.work.projectFilterIDs"
    private let filterToChoresOnlyDefaultsKey = "boss.work.filterToChoresOnly"
    private let includeChoresDefaultsKey = "boss.work.includeChores"
    private let showBlockedOnlyDefaultsKey = "boss.work.showBlockedOnly"
    private let showArchivedProjectsDefaultsKey = "boss.work.showArchivedProjects"
    private let workBoardGroupingDefaultsKey = "boss.work.grouping"
    private let bossPanelCollapsedDefaultsKey = "boss.work.bossPanelCollapsed"
    private let bossPanelWidthDefaultsKey = "boss.work.bossPanelWidth"

    init(paths: BossEnginePaths) {
        self.paths = paths
        self.socketPath = paths.socketPath
        self.processController = EngineProcessController(paths: paths)
        let showSystem = ProcessInfo.processInfo.environment["BOSS_SHOW_SYSTEM_MESSAGES"] ?? ""
        showSystemMessages = showSystem == "1" || showSystem.lowercased() == "true"
        engine = EngineClient(socketPath: paths.socketPath)

        commonInit()
    }

    /// Test-only convenience: build a `ChatViewModel` whose engine
    /// paths are all derived from a single per-test `socketPath` so a
    /// test never touches the production pid file or control token.
    /// Mirrors the call shape `ChatViewModel(socketPath:)` that
    /// pre-issue-#705 tests used, but routes through
    /// `BossEnginePaths.forTest(...)` so the test-context refusal in
    /// `BossEnginePaths.production*()` still applies to anything that
    /// reaches for the canonical paths.
    convenience init(socketPath: String) {
        let paths = BossEnginePaths.forTest(
            socketPath: socketPath,
            pidPath: "\(socketPath).pid",
            controlTokenPath: "\(socketPath).token"
        )
        self.init(paths: paths)
    }

    private func commonInit() {

        if let rawMode = defaults.string(forKey: navigationModeDefaultsKey),
           let persistedMode = NavigationMode(rawValue: rawMode) {
            navigationMode = persistedMode
        }
        selectedWorkProductID = defaults.string(forKey: selectedWorkProductDefaultsKey)
        if let storedFilters = defaults.array(forKey: selectedProjectFilterIDsDefaultsKey) as? [String] {
            selectedProjectFilterIDs = Set(storedFilters)
        }
        filterToChoresOnly = defaults.bool(forKey: filterToChoresOnlyDefaultsKey)
        if defaults.object(forKey: includeChoresDefaultsKey) != nil {
            includeChores = defaults.bool(forKey: includeChoresDefaultsKey)
        }
        showBlockedOnly = defaults.bool(forKey: showBlockedOnlyDefaultsKey)
        showArchivedProjects = defaults.bool(forKey: showArchivedProjectsDefaultsKey)
        if let groupingRaw = defaults.string(forKey: workBoardGroupingDefaultsKey),
           let grouping = WorkBoardGrouping(rawValue: groupingRaw) {
            workBoardGrouping = grouping
        }
        isBossPanelCollapsed = defaults.bool(forKey: bossPanelCollapsedDefaultsKey)
        let savedWidth = defaults.double(forKey: bossPanelWidthDefaultsKey)
        if savedWidth > 0 {
            bossPanelWidth = savedWidth
        }

        processController.onOutputLine = { [weak self] line in
            self?.appendSystemMessage(line)
        }

        engine.onEvent = { [weak self] event in
            self?.handle(event)
        }

        reviewNotifier.configure()
        reviewNotifier.onSelectWorkItem = { [weak self] taskID in
            self?.setNavigationMode(.work)
            self?.selectWorkCard(taskID)
        }

        // In the AppKit-hosted macOS shell, the root SwiftUI `.task` can be
        // missed on some launches. Schedule the normal startup path here too so
        // the engine connection still comes up reliably.
        DispatchQueue.main.async { [weak self] in
            self?.startIfNeeded()
        }

        #if canImport(AppKit)
        // Kick PR-state reconcilers immediately when the user returns to Boss
        // from another app (e.g. after reviewing or merging a PR on GitHub).
        // The engine quiesces repeated kicks within a 15 s window so rapid
        // focus-toggle events don't hammer the GitHub API.
        //
        // `MainActor.assumeIsolated` is safe here because we pass `queue: .main`
        // — the closure always runs on the main queue, which is the main actor's
        // executor.
        appActivationObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.didBecomeActiveNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, self.isConnected else { return }
                self.engine.sendKickPrReconcilers()
            }
        }
        #endif
    }

    deinit {
        processController.stop()
        engine.stop()
    }

    func toggleBossPanelCollapsed() {
        isBossPanelCollapsed.toggle()
        defaults.set(isBossPanelCollapsed, forKey: bossPanelCollapsedDefaultsKey)
    }

    func setBossPanelWidth(_ width: CGFloat) {
        bossPanelWidth = width
        defaults.set(width, forKey: bossPanelWidthDefaultsKey)
    }

    func setNavigationMode(_ mode: NavigationMode) {
        navigationMode = mode
        defaults.set(mode.rawValue, forKey: navigationModeDefaultsKey)
        if mode == .work {
            refreshWork()
        }
        if mode == .automations {
            refreshAutomations()
        }
    }

    func selectWorkProduct(_ productID: String) {
        let isAlreadyShowingProductBoard =
            selectedWorkProductID == productID
            && selectedProjectFilterIDs.isEmpty
            && selectedWorkCardID == nil
        guard !isAlreadyShowingProductBoard else { return }
        selectedWorkProductID = productID
        selectedProjectFilterIDs = []
        selectedWorkCardID = nil
        workErrorMessage = nil
        defaults.set(productID, forKey: selectedWorkProductDefaultsKey)
        persistProjectFilterIDs()
        refreshWorkSubscriptions()
        if isConnected {
            engine.sendGetWorkTree(productId: productID)
            engine.sendListAttentionItemsForWorkItem(workItemID: productID)
            engine.sendListAttentionGroups(productId: productID)
        }
    }

    func toggleProjectFilter(_ projectID: String) {
        if filterToChoresOnly {
            filterToChoresOnly = false
            defaults.set(false, forKey: filterToChoresOnlyDefaultsKey)
        }
        if selectedProjectFilterIDs.contains(projectID) {
            selectedProjectFilterIDs.remove(projectID)
        } else {
            selectedProjectFilterIDs.insert(projectID)
        }
        selectedWorkCardID = nil
        persistProjectFilterIDs()
    }

    func clearProjectFilters() {
        guard !selectedProjectFilterIDs.isEmpty || filterToChoresOnly else { return }
        selectedProjectFilterIDs = []
        filterToChoresOnly = false
        defaults.set(false, forKey: filterToChoresOnlyDefaultsKey)
        selectedWorkCardID = nil
        persistProjectFilterIDs()
    }

    func setFilterToChoresOnly(_ value: Bool) {
        guard filterToChoresOnly != value else { return }
        filterToChoresOnly = value
        defaults.set(value, forKey: filterToChoresOnlyDefaultsKey)
        if value {
            selectedProjectFilterIDs = []
            persistProjectFilterIDs()
        }
        selectedWorkCardID = nil
    }

    func archiveProject(id: String) {
        engine.sendUpdateWorkItem(id: id, patch: ["status": "archived"])
    }

    func setIncludeChores(_ value: Bool) {
        guard includeChores != value else { return }
        includeChores = value
        defaults.set(value, forKey: includeChoresDefaultsKey)
    }

    func setShowBlockedOnly(_ value: Bool) {
        guard showBlockedOnly != value else { return }
        showBlockedOnly = value
        defaults.set(value, forKey: showBlockedOnlyDefaultsKey)
    }

    func setShowArchivedProjects(_ value: Bool) {
        guard showArchivedProjects != value else { return }
        showArchivedProjects = value
        defaults.set(value, forKey: showArchivedProjectsDefaultsKey)
    }

    func persistProjectFilterIDs() {
        if selectedProjectFilterIDs.isEmpty {
            defaults.removeObject(forKey: selectedProjectFilterIDsDefaultsKey)
        } else {
            defaults.set(Array(selectedProjectFilterIDs).sorted(), forKey: selectedProjectFilterIDsDefaultsKey)
        }
    }

    func selectWorkCard(_ taskID: String?) {
        selectedWorkCardID = taskID
        guard let taskID, let task = task(withID: taskID) else { return }
        selectedWorkProductID = task.productID
    }

    /// Navigate the kanban to `taskID` and play a 1.5 s highlight.
    /// Switches to the Work tab, selects the task's product, clears
    /// every active board filter, and queues a scroll. If the task's
    /// product is not the one currently loaded, the scroll is deferred
    /// until the `workTree` event for that product arrives.
    ///
    /// Reveal's contract is "show me this card", so it must override any
    /// filter that would hide the target — a stale search query, a
    /// blocked-only / chores-only toggle, a project filter, or chores
    /// being hidden — all of which can exclude the card and make the
    /// scroll silently land on nothing (#1249). We reset the board to its
    /// unfiltered state before scrolling so the revealed card is
    /// guaranteed visible.
    func revealWorkCard(_ taskID: String, productID: String) {
        setNavigationMode(.work)
        clearWorkFiltersForReveal()
        selectedWorkCardID = taskID
        let isProductSwitch = currentSelectedProductID != productID
        if isProductSwitch {
            selectWorkProduct(productID)
            pendingRevealScrollID = taskID
        } else {
            triggerRevealScroll(taskID)
        }
        revealHighlightID = taskID
        let capturedID = taskID
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { [weak self] in
            if self?.revealHighlightID == capturedID {
                self?.revealHighlightID = nil
            }
        }
    }

    /// Reset every board filter that could hide a reveal target so the
    /// full work board for the product is shown. Each assignment is a
    /// no-op when the filter is already in its neutral state, so this is
    /// cheap to call unconditionally. Keep this in sync with
    /// `computeVisibleWorkItems` — any new narrowing filter added there
    /// must be neutralized here too, or reveal can silently fail again.
    private func clearWorkFiltersForReveal() {
        selectedProjectFilterIDs = []
        workSearchText = ""
        showBlockedOnly = false
        filterToChoresOnly = false
        includeChores = true
    }

    private func triggerRevealScroll(_ taskID: String) {
        revealScrollTarget = taskID
        let capturedID = taskID
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [weak self] in
            if self?.revealScrollTarget == capturedID {
                self?.revealScrollTarget = nil
            }
        }
    }

    func setWorkBoardGrouping(_ grouping: WorkBoardGrouping) {
        workBoardGrouping = grouping
        defaults.set(grouping.rawValue, forKey: workBoardGroupingDefaultsKey)
    }

    func presentCreateProduct() {
        pendingWorkCreateRequest = WorkCreateRequest(kind: .product)
    }

    func presentCreateProject() {
        guard let productID = currentSelectedProductID else { return }
        pendingWorkCreateRequest = WorkCreateRequest(kind: .project(productID: productID))
    }

    func presentCreateTask() {
        guard let project = taskCreationProject else { return }
        pendingWorkCreateRequest = WorkCreateRequest(
            kind: .task(productID: project.productID, projectID: project.id)
        )
    }

    func presentCreateChore() {
        guard let productID = currentSelectedProductID else { return }
        pendingWorkCreateRequest = WorkCreateRequest(kind: .chore(productID: productID))
    }

    func dismissWorkCreateRequest() {
        pendingWorkCreateRequest = nil
    }

    func presentEditSelectedWorkItem() {
        if let task = selectedTask {
            pendingWorkEditRequest = WorkEditRequest(item: task.isChore ? .chore(task) : .task(task))
        } else if let project = selectedProject {
            pendingWorkEditRequest = WorkEditRequest(item: .project(project))
        } else if let product = selectedProduct {
            pendingWorkEditRequest = WorkEditRequest(item: .product(product))
        }
    }

    func presentEditSelectedProduct() {
        guard let product = selectedProduct else { return }
        pendingWorkEditRequest = WorkEditRequest(item: .product(product))
    }

    func dismissWorkEditRequest() {
        pendingWorkEditRequest = nil
    }

    func submitWorkCreateRequest(
        _ request: WorkCreateRequest,
        name: String,
        description: String,
        repoRemoteURL: String = "",
        goal: String = "",
        setAsProductDefault: Bool = false
    ) {
        let trimmedName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedName.isEmpty else { return }

        workErrorMessage = nil
        let repoOverride = repoRemoteURL.trimmingCharacters(in: .whitespacesAndNewlines)
        switch request.kind {
        case .product:
            engine.sendCreateProduct(
                name: trimmedName,
                description: description,
                repoRemoteURL: repoRemoteURL
            )
        case .project(let productID):
            engine.sendCreateProject(
                productId: productID,
                name: trimmedName,
                description: description,
                goal: goal
            )
        case .task(let productID, let projectID):
            engine.sendCreateTask(
                productId: productID,
                projectId: projectID,
                name: trimmedName,
                description: description,
                repoRemoteURL: repoOverride.isEmpty ? nil : repoOverride
            )
            if setAsProductDefault && !repoOverride.isEmpty {
                engine.sendUpdateWorkItem(
                    id: productID,
                    patch: ["repo_remote_url": repoOverride]
                )
            }
        case .chore(let productID):
            engine.sendCreateChore(
                productId: productID,
                name: trimmedName,
                description: description,
                repoRemoteURL: repoOverride.isEmpty ? nil : repoOverride
            )
            if setAsProductDefault && !repoOverride.isEmpty {
                engine.sendUpdateWorkItem(
                    id: productID,
                    patch: ["repo_remote_url": repoOverride]
                )
            }
        }

        pendingWorkCreateRequest = nil
    }

    /// Empirical known-repo set for `productID`, mirroring the CLI's
    /// `known_repos_for_product` (multi-repo design Q4). Returns the
    /// distinct, non-empty `repo_remote_url` values across the
    /// product's tasks and chores, plus the product's own default if
    /// set. Sorted by short-name for stable picker ordering, with the
    /// product default first when present so the picker leads with
    /// the "obvious" choice.
    ///
    /// All inputs come from the work tree the model already has on
    /// hand; no engine RPC. Returns an empty array when the product
    /// is unknown.
    func knownReposForProduct(_ productID: String) -> [String] {
        guard products.contains(where: { $0.id == productID }) else {
            return []
        }
        var seen: Set<String> = []
        var result: [String] = []
        let productDefault = products
            .first(where: { $0.id == productID })?
            .repoRemoteURL?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        if let productDefault, !productDefault.isEmpty {
            seen.insert(productDefault)
            result.append(productDefault)
        }
        var rest: [String] = []
        let projects = projectsByProductID[productID] ?? []
        for project in projects {
            for task in tasksByProjectID[project.id] ?? [] {
                if let url = task.repoRemoteURL?.trimmingCharacters(in: .whitespacesAndNewlines),
                   !url.isEmpty, !seen.contains(url) {
                    seen.insert(url)
                    rest.append(url)
                }
            }
        }
        for chore in choresByProductID[productID] ?? [] {
            if let url = chore.repoRemoteURL?.trimmingCharacters(in: .whitespacesAndNewlines),
               !url.isEmpty, !seen.contains(url) {
                seen.insert(url)
                rest.append(url)
            }
        }
        rest.sort { shortRepoName(for: $0) < shortRepoName(for: $1) }
        result.append(contentsOf: rest)
        return result
    }

    /// Product default repo URL, looked up by id. Used by
    /// `WorkCreateSheet` to construct a `WorkCreateRepoFormState`
    /// without reaching into `products` itself. `nil` for an unknown
    /// product or one whose URL is empty / whitespace.
    func productDefaultRepoURL(_ productID: String) -> String? {
        let raw = products.first(where: { $0.id == productID })?.repoRemoteURL
        let trimmed = raw?.trimmingCharacters(in: .whitespacesAndNewlines)
        if let trimmed, !trimmed.isEmpty { return trimmed }
        return nil
    }

    func submitWorkEditRequest(
        _ request: WorkEditRequest,
        name: String,
        description: String,
        status: String,
        repoRemoteURL: String = "",
        goal: String = "",
        priority: String = "",
        prURL: String = "",
        workerBranchPrefix: String = "",
        docsRepo: String = ""
    ) {
        let trimmedName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedName.isEmpty else { return }

        var patch: [String: Any] = [
            "name": trimmedName,
            "description": description,
            "status": status,
        ]

        let id: String
        switch request.item {
        case .product(let product):
            id = product.id
            patch["repo_remote_url"] = repoRemoteURL
            patch["worker_branch_prefix"] = workerBranchPrefix
            patch["docs_repo"] = docsRepo
        case .project(let project):
            id = project.id
            patch["goal"] = goal
            patch["priority"] = priority
        case .task(let task), .chore(let task):
            id = task.id
            patch["pr_url"] = prURL
            // Only send a priority patch when the user actually
            // touched the picker — keeps unrelated edits from
            // bouncing the field through serde-validation noise.
            if !priority.isEmpty, priority != task.priority {
                patch["priority"] = priority
            }
        }

        engine.sendUpdateWorkItem(id: id, patch: patch)
        pendingWorkEditRequest = nil
    }

    func setProductExternalTracker(
        productId: String,
        kind: String,
        org: String,
        repo: String,
        projectNumber: Int,
        reverseClose: Bool
    ) {
        let config: [String: Any] = [
            "org": org,
            "repo": repo,
            "project_number": projectNumber,
            "reverse_close": reverseClose,
        ]
        engine.sendSetProductExternalTracker(productId: productId, kind: kind, config: config)
    }

    func unsetProductExternalTracker(productId: String) {
        engine.sendUnsetProductExternalTracker(productId: productId)
    }

    // MARK: GitHub OAuth device-flow bridges (OAuth device-flow design §4)
    //
    // Thin pass-throughs to the engine RPCs. The engine owns the flow and
    // the token; these just kick state transitions. The resulting
    // `gitHubAuthState` updates arrive via `git_hub_auth_state` events.

    /// Begin the device flow (the "Connect" / "Start over" action).
    func gitHubAuthConnect() {
        engine.sendGitHubAuthStart()
    }

    /// Abort an in-progress device flow (the "Cancel" action).
    func gitHubAuthCancel() {
        engine.sendGitHubAuthCancel()
    }

    /// Delete the stored token and return to disconnected.
    func gitHubAuthDisconnect() {
        engine.sendGitHubAuthDisconnect()
    }

    /// Re-run the device flow, overwriting the stored token. Identical to
    /// `gitHubAuthConnect` at the wire level (the engine restarts the flow
    /// from `Authorized`); named separately so the call site reads clearly.
    func gitHubAuthReauthorize() {
        engine.sendGitHubAuthStart()
    }

    /// Re-request the current state, which re-runs the engine's org/SSO
    /// probe when connected (the "Re-check" affordance, design §7).
    func gitHubAuthRecheck() {
        engine.sendGitHubAuthStatus()
    }

    func deleteSelectedWorkItem() {
        guard let task = selectedTask else { return }
        engine.sendDeleteWorkItem(id: task.id)
    }

    func moveSelectedTask(offset: Int) {
        guard let task = selectedTask,
              !task.isChore,
              let projectID = task.projectID,
              var tasks = tasksByProjectID[projectID]?.sorted(by: taskSort),
              let currentIndex = tasks.firstIndex(where: { $0.id == task.id })
        else {
            return
        }

        let destination = currentIndex + offset
        guard tasks.indices.contains(destination) else { return }

        tasks.swapAt(currentIndex, destination)
        engine.sendReorderProjectTasks(projectId: projectID, taskIds: tasks.map(\.id))
    }

    /// Move a card between kanban columns. Two extra concerns vs. a
    /// pure status edit, both per `tools/boss/docs/designs/work-kanban.md`:
    ///
    /// - Drop into Doing (target status `active`) also fires
    ///   `RequestExecution` so the engine schedules a worker. The
    ///   engine is idempotent — a non-terminal execution already
    ///   running for this work item won't get a duplicate.
    /// - Move OUT of Doing while a live worker is attached is
    ///   blocked — except for two intentional gestures:
    ///   (a) Dragging back to Backlog (`todo`): engine stops the worker,
    ///       releases the lease, and parks the card — no autostart.
    ///   (b) Terminal transitions (`done`, `archived`): these mirror the
    ///       engine's own lifecycle resolutions and are always allowed.
    func moveTask(_ taskID: String, to column: WorkBoardColumnKey) {
        guard let task = task(withID: taskID) else { return }
        let targetStatus = column.targetStatus
        guard task.status != targetStatus else { return }

        if task.status == "active"
            && !Self.terminalKanbanStatuses.contains(targetStatus)
            && column != .backlog  // backlog drag = stop+park: engine handles teardown
            && hasLiveWorker(forTaskID: taskID)
        {
            appendSystemMessage(
                "\(task.name) is being worked on by a live worker. Stop the worker before moving the card out of Doing.",
                alwaysShow: true
            )
            return
        }

        // Optimistic update: move the card to the destination column immediately
        // before the RPC completes. The engine remains the authority — on failure
        // we bounce back via bounceBackOptimisticMoves.
        let originColumn = effectiveBoardColumn(for: task)
        pendingMoveOriginByTaskID[taskID] = originColumn
        optimisticColumnByTaskID[taskID] = column
        invalidateWorkCache()

        engine.sendUpdateWorkItem(id: task.id, patch: ["status": targetStatus])

        if targetStatus == "active" {
            engine.sendRequestExecution(workItemId: task.id)
        }
    }

    /// Statuses that the engine itself can drive a chore into at run
    /// completion. The kanban must allow the human to mirror those
    /// transitions even from `active` so a successful PR-merge flow
    /// can move a card to Done without first stopping the worker.
    private static let terminalKanbanStatuses: Set<String> = [
        "done",
        "archived",
    ]

    /// True iff the work item has a non-terminal worker currently
    /// attached (running, paused on input, or idle between turns).
    /// `WorkerActivity.terminated` and `.errored` count as "no live
    /// worker" — the slot is no longer holding the run open.
    private func hasLiveWorker(forTaskID taskID: String) -> Bool {
        guard let live = workerLiveState(forTaskID: taskID) else {
            return false
        }
        switch live.activity {
        case .terminated, .errored:
            return false
        case .spawning, .working, .waitingForInput, .idle:
            return true
        }
    }

    func toggleBlocked(for taskID: String) {
        guard let task = task(withID: taskID) else { return }
        let nextStatus: String
        switch task.status {
        case "blocked":
            nextStatus = "active"
        case "active":
            nextStatus = "blocked"
        default:
            return
        }
        engine.sendUpdateWorkItem(id: task.id, patch: ["status": nextStatus])
    }

    /// Update a task or chore's priority via the inline picker on the
    /// detail popover. No-ops when the new value matches the current
    /// one so an idle picker tap doesn't generate write traffic.
    func setPriority(for taskID: String, to priority: WorkPriority) {
        guard let task = task(withID: taskID) else { return }
        guard task.priority != priority.rawValue else { return }
        engine.sendUpdateWorkItem(id: task.id, patch: ["priority": priority.rawValue])
    }

    func startIfNeeded() {
        guard !didStart else { return }

        // Swap-on-startup fallback (design doc §4): if a staged update is ready and
        // the user is in automatic mode, replace the bundle *before* the engine
        // launches (so the new engine binary is what gets spawned), then hand off to
        // the detached relaunch helper and exit — it relaunches us into the new
        // version. If no swap applies, this returns false and we continue normally.
        // Placed here because this is the single chokepoint guaranteed to run before
        // `processController.start()`. See [[UpdateLifecycle]].
        if UpdateLifecycle.applyStartupSwapIfNeeded() {
            exit(0)
        }

        didStart = true

        let autostart = ProcessInfo.processInfo.environment["BOSS_ENGINE_AUTOSTART"] != "0"
        if autostart {
            let processController = self.processController
            DispatchQueue.global(qos: .userInitiated).async { [weak self] in
                do {
                    try processController.start()
                    DispatchQueue.main.async {
                        self?.startEngineIfNeeded()
                    }
                } catch {
                    DispatchQueue.main.async {
                        self?.appendSystemMessage(
                            "Failed to launch engine: \(error.localizedDescription)",
                            alwaysShow: true
                        )
                    }
                }
            }
        } else {
            startEngineIfNeeded()
        }
    }

    /// `true` while a user-initiated engine restart is running. The
    /// unreachable banner binds its "Restart engine" button to the
    /// inverse so a second click can't queue another terminate +
    /// relaunch on top of the first one (issue #697).
    @Published private(set) var isRestartingEngine = false

    /// User-initiated recovery from the unreachable banner. Terminates
    /// the engine the pid file points at (token-auth shutdown RPC
    /// first, then SIGTERM/SIGKILL — same path `stop()` uses) and
    /// relaunches it. The `EngineClient` reconnect loop picks the new
    /// socket up automatically once it accepts.
    ///
    /// Routes the terminate+launch through the same background queue
    /// `startIfNeeded()` uses so the main thread never blocks on
    /// `terminateEngine`'s up-to-5s SIGKILL wait. `isRestartingEngine`
    /// drives the banner button's `.disabled` state.
    func restartEngine() {
        guard !isRestartingEngine else { return }
        isRestartingEngine = true

        let processController = self.processController
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            var restartError: Error?
            do {
                try processController.restart()
            } catch {
                restartError = error
            }
            DispatchQueue.main.async {
                guard let self else { return }
                self.isRestartingEngine = false
                if let restartError {
                    self.appendSystemMessage(
                        "Failed to restart engine: \(restartError.localizedDescription)",
                        alwaysShow: true
                    )
                }
                // Make sure the EngineClient is started even if the
                // very first `startIfNeeded()` failed before launching
                // it (autostart=0 paths also flow through here).
                self.startEngineIfNeeded()
            }
        }
    }

    func refreshWork() {
        guard isConnected else { return }
        engine.sendListProducts()
        if let productID = currentSelectedProductID {
            engine.sendGetWorkTree(productId: productID)
        }
    }

    /// Ask the engine to resolve the design-doc pointer for every
    /// project whose row carries a non-nil `designDocPath`. Projects
    /// with no pointer set are skipped so the engine doesn't burn an
    /// RPC just to be told `not_set` — the affordance is hidden in
    /// that case anyway. Re-issued on every `WorkTree` so a re-point
    /// landed in another session flows through to the icon.
    func refreshDesignDocStates(for projects: [WorkProject]) {
        guard isConnected else { return }
        let pending = projects.filter { $0.designDocPath != nil }
        guard !pending.isEmpty else { return }
        currentDesignDocResolveBatch = DesignDocResolveBatch(
            startDate: Date(),
            pendingProjectIDs: Set(pending.map(\.id)),
            initialCount: pending.count
        )
        for project in pending {
            engine.sendResolveProjectDesignDoc(projectID: project.id)
        }
    }

    /// Open the design-doc pointer for `project`. Dispatch follows
    /// `ProjectDesignDocState`:
    ///
    /// - `.notSet` — affordance shouldn't have been clickable. No-op.
    /// - `.broken` — surface the engine's reason as a work error so
    ///   the user can re-point. The re-point sheet is tracked
    ///   separately (design Q5).
    /// - `.resolved` — dispatch priority:
    ///   1. `rawContentURL` present: fetch from GitHub via [[rawContentFetcher]]
    ///      and open in the async markdown viewer. This is correct for both
    ///      merged (main) and in-review (PR branch) docs — the GitHub ref in
    ///      the URL is the authoritative source regardless of cube workspace
    ///      state. A leased workspace may be on a different task's branch even
    ///      when `resolved.branch == "main"`, so reading from disk is not safe.
    ///   2. `rawContentURL` absent (non-GitHub repo or older engine) AND a
    ///      workspace is leased for the resolved repo AND branch is `main`:
    ///      render via [[designRendererOpener]] (in-app renderer) when wired,
    ///      otherwise hand the `file://` URL to [[urlOpener]].
    ///   3. Fall through to [[urlOpener]] with the web URL.
    func openProjectDesignDoc(_ project: WorkProject) {
        let shortID = project.shortID.map { "\($0)" } ?? project.id
        let state = designDocStateByProjectID[project.id] ?? .notSet
        switch state {
        case .notSet:
            return
        case .broken(let reason):
            workErrorMessage = "Design doc pointer is broken: \(reason)"
        case .resolved(let resolved, let workspacePath, let webURL, let rawContentURL):
            // Prefer fetching via rawContentURL (GitHub API). This is correct
            // regardless of cube workspace state — the workspace may be on a
            // different branch even when resolved.branch == "main".
            if let rawContentURL, let rawURL = URL(string: rawContentURL) {
                let projectName = project.name
                let clickStart = Date()
                designDocTimingLog.info("phase=dispatch project=\(shortID, privacy: .public) path=rawContentURL")
                if let opener = asyncMarkdownViewerOpener {
                    // Open the window immediately in a loading state, then
                    // resolve the content asynchronously — the user sees a
                    // window within one frame of the click (T-open-immediately).
                    asyncMarkdownViewerVM.state = .loading
                    asyncMarkdownViewerVM.clickStartTime = clickStart
                    let openWindowStart = Date()
                    opener()
                    let openWindowMs = Int(Date().timeIntervalSince(openWindowStart) * 1000)
                    designDocTimingLog.info("phase=open_window project=\(shortID, privacy: .public) duration_ms=\(openWindowMs, privacy: .public)")
                    Task { @MainActor in
                        await self.fetchAndUpdateAsyncMarkdownViewerVM(
                            projectName: projectName,
                            rawURL: rawURL,
                            projectShortID: shortID
                        )
                    }
                } else {
                    // Headless / test path: fetch first, then open via the
                    // legacy markdownViewerOpener (or fall back to urlOpener).
                    Task { @MainActor in
                        await self.fetchAndOpenDesignDoc(
                            projectName: projectName,
                            rawURL: rawURL,
                            webURL: webURL,
                            projectShortID: shortID
                        )
                    }
                }
                return
            }
            // rawContentURL absent (non-GitHub repo or older engine): fall back
            // to the workspace fast-path for merged docs when a workspace is
            // available. Only safe for branch == "main" designs where we can
            // reasonably assume the workspace holds the merged file.
            if let workspacePath, isWorkspaceFastPathEligible(kind: resolved.kind),
               resolved.branch == "main" {
                designDocTimingLog.info("phase=dispatch project=\(shortID, privacy: .public) path=workspace")
                if let opener = designRendererOpener,
                   let content = DesignRendererContent.from(
                       projectID: project.id,
                       projectName: project.name,
                       resolved: resolved,
                       workspacePath: workspacePath,
                       webURL: webURL
                   ) {
                    opener(content)
                    return
                }
                let absolute = (workspacePath as NSString)
                    .appendingPathComponent(resolved.path)
                urlOpener(URL(fileURLWithPath: absolute))
                return
            }
            guard let url = URL(string: webURL) else {
                workErrorMessage = "Design doc URL could not be parsed: \(webURL)"
                return
            }
            designDocTimingLog.info("phase=dispatch project=\(shortID, privacy: .public) path=webURL")
            urlOpener(url)
        }
    }

    /// Fetch raw markdown from `rawURL` and open it in the
    /// [[markdownViewerOpener]] window. Falls back to `urlOpener(webURL)`
    /// if the fetch fails or [[markdownViewerOpener]] is not wired.
    @MainActor
    private func fetchAndOpenDesignDoc(
        projectName: String,
        rawURL: URL,
        webURL: String,
        projectShortID: String
    ) async {
        do {
            let fetchStart = Date()
            designDocTimingLog.info("phase=fetch_start project=\(projectShortID, privacy: .public) url=\(rawURL.absoluteString, privacy: .public)")
            let markdown = try await rawContentFetcher(rawURL)
            let fetchMs = Int(Date().timeIntervalSince(fetchStart) * 1000)
            designDocTimingLog.info("phase=fetch_end project=\(projectShortID, privacy: .public) duration_ms=\(fetchMs, privacy: .public) bytes=\(markdown.utf8.count, privacy: .public)")
            if let opener = markdownViewerOpener {
                let title = projectName.isEmpty ? rawURL.lastPathComponent : projectName
                opener(MarkdownViewerContent(title: title, markdown: markdown))
            } else if let url = URL(string: webURL) {
                urlOpener(url)
            }
        } catch {
            if let url = URL(string: webURL) {
                urlOpener(url)
            } else {
                workErrorMessage = "Failed to fetch design doc: \(error.localizedDescription)"
            }
        }
    }

    /// Fetch raw markdown from `rawURL` and update [[asyncMarkdownViewerVM]]
    /// state. Called after the viewer window is already open in `.loading`
    /// state. Transitions to `.loaded` on success or `.failed` on error so
    /// the window always resolves to a terminal state.
    @MainActor
    private func fetchAndUpdateAsyncMarkdownViewerVM(
        projectName: String,
        rawURL: URL,
        projectShortID: String
    ) async {
        let title = projectName.isEmpty ? rawURL.lastPathComponent : projectName
        do {
            let fetchStart = Date()
            designDocTimingLog.info("phase=fetch_start project=\(projectShortID, privacy: .public) url=\(rawURL.absoluteString, privacy: .public)")
            let markdown = try await rawContentFetcher(rawURL)
            let fetchMs = Int(Date().timeIntervalSince(fetchStart) * 1000)
            designDocTimingLog.info("phase=fetch_end project=\(projectShortID, privacy: .public) duration_ms=\(fetchMs, privacy: .public) bytes=\(markdown.utf8.count, privacy: .public)")
            asyncMarkdownViewerVM.pendingRenderProjectShortID = projectShortID
            asyncMarkdownViewerVM.renderStartTime = Date()
            asyncMarkdownViewerVM.renderContentID = UUID()
            asyncMarkdownViewerVM.state = .loaded(title: title, markdown: markdown)
        } catch {
            asyncMarkdownViewerVM.state = .failed(
                title: title,
                message: error.localizedDescription
            )
        }
    }

    /// Kanban open-affordance fast-path predicate: a `ResolvedDesignDocKind`
    /// is editor-eligible exactly when the doc lives in a repo Boss
    /// tracks as a Product (same- or other-product). External pointers
    /// always fall through to the web URL because cube can't lease
    /// untracked repos.
    private func isWorkspaceFastPathEligible(kind: ResolvedDesignDocKind) -> Bool {
        switch kind {
        case .sameProduct, .otherProduct:
            return true
        case .external:
            return false
        }
    }

    // MARK: - Boss Session Registration

    /// Called by ContentView when the Boss pane's libghostty surface attaches
    /// (initial creation or after a restart). Sends RegisterBossSession if the
    /// app session is already confirmed; otherwise the registration fires when
    /// appSessionRegistered arrives.
    func bossPaneShellPidAvailable() {
        maybeRegisterBossSession()
    }

    private func maybeRegisterBossSession() {
        guard isAppSessionRegistered else { return }
        guard let pid = bossPaneShellPidProvider?(), pid > 0 else { return }
        engine.sendRegisterBossSession(shellPid: pid)
    }

    // MARK: - Event Handling

    var paneSpawnHandler: ((EngineSpawnRequest) -> EngineSpawnResult)?
    var paneReleaseHandler: ((Int, UInt32) -> EngineReleaseResult)?
    var paneSendHandler: ((Int, String) -> EngineSendResult)?
    var paneFocusHandler: ((Int) -> EngineFocusResult)?
    var paneInterruptHandler: ((Int) -> EngineInterruptResult)?

    /// Whether the engine has confirmed this client is the registered app session.
    /// Reset on disconnect; set when `appSessionRegistered` is received.
    private var isAppSessionRegistered = false
    /// Returns the Boss pane's current shell pid from
    /// `ghostty_surface_foreground_pid`. Injected by ContentView (GhosttyKit
    /// build only). Returns 0 when the surface is not yet live.
    var bossPaneShellPidProvider: (() -> Int32)?

    private func handle(_ event: EngineEvent) {
        switch event {
        case .connected:
            isConnected = true
            hasConnectedOnce = true
            engine.sendRegisterAppSession()
            refreshWorkSubscriptions()
            engine.sendListProducts()
            engine.sendListWorkerLiveStates()
            engine.sendListLiveStatusDisabledSlots()
            // Pull the engine's configuration health on every (re)connect
            // so the top-of-window banner reflects the *current* engine,
            // not the one we attached to before a restart (#699).
            engine.sendGetEngineHealth()
            // Pull the current GitHub OAuth auth state so the "GitHub
            // account" settings subsection reflects a token persisted by a
            // prior session (the engine restores it from the keychain at
            // boot) without waiting for a device-flow transition.
            engine.sendGitHubAuthStatus()
            if let productID = currentSelectedProductID {
                engine.sendGetWorkTree(productId: productID)
                engine.sendListAttentionGroups(productId: productID)
            }
        case .appSessionRegistered:
            isAppSessionRegistered = true
            maybeRegisterBossSession()
        case .bossSessionRegistered:
            break
        case .engineRequest(let requestId, let request):
            switch request {
            case .spawnWorkerPane(let spawn):
                let result: EngineSpawnResult
                if let handler = paneSpawnHandler {
                    result = handler(spawn)
                } else {
                    result = .failure(.internalFailure(
                        "no pane allocator wired into this build (Bazel without GhosttyKit)"
                    ))
                }
                engine.sendSpawnWorkerPaneResponse(requestId: requestId, result: result)
            case .releaseWorkerPane(let slotId, let killGrace):
                let result: EngineReleaseResult
                if let handler = paneReleaseHandler {
                    result = handler(slotId, killGrace)
                } else {
                    result = .failure(.internalFailure(
                        "no pane allocator wired into this build (Bazel without GhosttyKit)"
                    ))
                }
                engine.sendReleaseWorkerPaneResponse(requestId: requestId, result: result)
            case .sendToPane(let slotId, let text):
                let result: EngineSendResult
                if let handler = paneSendHandler {
                    result = handler(slotId, text)
                } else {
                    result = .failure(.internalFailure(
                        "no pane allocator wired into this build (Bazel without GhosttyKit)"
                    ))
                }
                engine.sendSendToPaneResponse(requestId: requestId, result: result)
            case .focusWorkerPane(let slotId):
                let result: EngineFocusResult
                if let handler = paneFocusHandler {
                    result = handler(slotId)
                } else {
                    result = .failure(.internalFailure(
                        "no pane allocator wired into this build (Bazel without GhosttyKit)"
                    ))
                }
                engine.sendFocusWorkerPaneResponse(requestId: requestId, result: result)
            case .interruptWorkerPane(let slotId):
                let result: EngineInterruptResult
                if let handler = paneInterruptHandler {
                    result = handler(slotId)
                } else {
                    result = .failure(.internalFailure(
                        "no pane allocator wired into this build (Bazel without GhosttyKit)"
                    ))
                }
                engine.sendInterruptWorkerPaneResponse(requestId: requestId, result: result)
            case .revealWorkItem(let workItemId, let productId):
                revealWorkCard(workItemId, productID: productId)
                engine.sendRevealWorkItemResponse(requestId: requestId, result: .success)
            }
        case .disconnected:
            isConnected = false
            isAppSessionRegistered = false
            subscribedWorkTopics.removeAll()
        case .workInvalidated(let topic, let productId, _):
            if topic == "work.products" {
                engine.sendListProducts()
            }
            if let selectedProductID = currentSelectedProductID,
               topic == workTopic(forProductID: selectedProductID)
            {
                engine.sendGetWorkTree(productId: selectedProductID)
                engine.sendListAttentionItemsForWorkItem(workItemID: selectedProductID)
                engine.sendListAttentionGroups(productId: selectedProductID)
            } else if let productId,
                      productId == currentSelectedProductID {
                engine.sendGetWorkTree(productId: productId)
                engine.sendListAttentionItemsForWorkItem(workItemID: productId)
                engine.sendListAttentionGroups(productId: productId)
            }
        case .productsList(let products):
            self.products = products.sorted(by: { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending })
            let activeIDs = Set(activeProducts.map(\.id))
            if let selectedWorkProductID,
               !activeIDs.contains(selectedWorkProductID) {
                let archivedName = self.products.first(where: { $0.id == selectedWorkProductID })?.name
                self.selectedWorkProductID = nil
                self.selectedProjectFilterIDs = []
                self.selectedWorkCardID = nil
                defaults.removeObject(forKey: selectedWorkProductDefaultsKey)
                persistProjectFilterIDs()
                if let archivedName {
                    workErrorMessage = "Product \"\(archivedName)\" was archived elsewhere; switching to the next active product."
                }
            }
            if currentSelectedProductID == nil, let first = activeProducts.first {
                self.selectedWorkProductID = first.id
                defaults.set(first.id, forKey: selectedWorkProductDefaultsKey)
                engine.sendGetWorkTree(productId: first.id)
            } else if let productID = currentSelectedProductID {
                engine.sendGetWorkTree(productId: productID)
            }
            refreshWorkSubscriptions()
        case .projectsList(let productId, let projects):
            projectsByProductID[productId] = projects.sorted(by: projectSort)
        case .workTree(let product, let projects, let tasks, let chores, let taskRuntimes, let dependencies):
            upsertProduct(product)
            if currentSelectedProductID == nil {
                selectedWorkProductID = product.id
            }
            projectsByProductID[product.id] = projects.sorted(by: projectSort)
            tasksByProjectID = tasksByProjectID.filter { _, existingTasks in
                existingTasks.first?.productID != product.id
            }
            var productLevelRevisions: [WorkTask] = []
            var productLevelTasks: [WorkTask] = []
            for task in tasks {
                guard let projectID = task.projectID else {
                    // Product-level rows (`project_id IS NULL`) have no project
                    // lane to live under. Route every one of them into a bucket
                    // rather than dropping the ones we don't special-case — a
                    // chore-parented revision rolls up under its parent (issue
                    // #789), and everything else (investigations, any future
                    // product-level kind) renders as a first-class card (issue
                    // #886). The `else` is a catch-all on purpose: nothing the
                    // engine sends should silently disappear here.
                    if task.kind == "revision" {
                        productLevelRevisions.append(task)
                    } else {
                        productLevelTasks.append(task)
                    }
                    continue
                }
                tasksByProjectID[projectID, default: []].append(task)
            }
            for (projectID, projectTasks) in tasksByProjectID where
                projectTasks.first?.productID == product.id {
                tasksByProjectID[projectID] = projectTasks.sorted(by: taskSort)
            }
            choresByProductID[product.id] = chores.sorted(by: taskSort)
            productLevelRevisionsByProductID[product.id] = productLevelRevisions.sorted(by: taskSort)
            productLevelTasksByProductID[product.id] = productLevelTasks.sorted(by: taskSort)
            mergeTaskRuntimes(taskRuntimes, for: product.id, tasks: tasks, chores: chores)
            dependenciesByProductID[product.id] = dependencies
            seedReviewTaskIDs(tasks: tasks, chores: chores, productID: product.id)
            // After tasksByProjectID reflects real engine state, clear optimistic
            // overrides for cards whose true column now matches the target.
            // Done before the @Published assignments take effect in the view so
            // the next render uses real boardColumn values — no visible flicker.
            reconcileOptimisticOverrides(from: tasks + chores)
            reconcileWorkSelection()
            refreshWorkSubscriptions()
            refreshDesignDocStates(for: projects)
            engine.sendListAttentionItemsForWorkItem(workItemID: product.id)
            engine.sendListAttentionGroups(productId: product.id)
            workErrorMessage = nil
            if let pending = pendingRevealScrollID {
                let allIDs = Set(tasks.map(\.id) + chores.map(\.id))
                if allIDs.contains(pending) {
                    pendingRevealScrollID = nil
                    triggerRevealScroll(pending)
                }
            }
        case .workItemCreated(let item):
            handleCreatedWorkItem(item)
        case .workItemUpdated(let item):
            handleUpdatedWorkItem(item)
        case .projectTasksReordered(let projectId, _):
            if let productID = productID(forProjectID: projectId) {
                engine.sendGetWorkTree(productId: productID)
            }
        case .workItemDeleted(let id):
            let deletedTask = task(withID: id)
            if selectedTask?.id == id {
                selectedWorkCardID = nil
            }
            if let productID = deletedTask?.productID ?? currentSelectedProductID {
                engine.sendGetWorkTree(productId: productID)
            }
        case .workError(let message):
            // Allow the user to retry any in-flight review terminal or
            // merge-when-ready request that failed.
            openingReviewTerminalIDs.removeAll()
            mergingWhenReadyIDs.removeAll()
            if case .loading = reviewTerminalVM.state {
                reviewTerminalVM.state = .idle
            }
            if !pendingMoveOriginByTaskID.isEmpty {
                // Error is likely from an in-flight kanban move: bounce the
                // card(s) back and show an inline non-blocking notice instead
                // of interrupting with a modal dialog.
                bounceBackOptimisticMoves(message: message)
            } else {
                workErrorMessage = message
            }
        case .error(let message):
            if isSocketTransportError(message) {
                // Transport errors fire continuously while the engine
                // is unreachable (every reconnect attempt re-emits a
                // `socket waiting:` line). Routing them through the
                // work-error modal makes the app unusable: dismissing
                // re-opens it on the next retry. The disconnected
                // banner in the main chrome is the user-facing signal
                // for this state — see `hasConnectedOnce` /
                // `isConnected` in ContentView.
                appendSystemMessage(message)
                return
            }
            workErrorMessage = message
        case .workerLiveStatesList(let states):
            liveWorkerStates.update(states: states)
        case .liveStatusDisabledSlotsList(let slotIds):
            liveStatusDisabledSlotIDs = Set(slotIds)
        case .liveStatusEnabledSet(let slotId, let enabled):
            if enabled {
                liveStatusDisabledSlotIDs.remove(slotId)
            } else {
                liveStatusDisabledSlotIDs.insert(slotId)
            }
        case .featureFlagsList(let flags):
            featureFlags = flags
        case .featureFlagSet(let name, let enabled):
            // Patch the cached snapshot so the toggle commits without
            // a second round-trip. The engine has already persisted
            // the value at this point — the patch is a UI mirror.
            if let idx = featureFlags.firstIndex(where: { $0.name == name }) {
                let prior = featureFlags[idx]
                featureFlags[idx] = FeatureFlag(
                    name: prior.name,
                    description: prior.description,
                    category: prior.category,
                    defaultEnabled: prior.defaultEnabled,
                    enabled: enabled
                )
            }
        case .engineHealthResult(let apiKeyPresent, let issues):
            engineAnthropicApiKeyPresent = apiKeyPresent
            engineHealthIssues = issues
        case .settingsList(let settings):
            engineSettings = settings
        case .settingSet(let key, let enabled):
            if let idx = engineSettings.firstIndex(where: { $0.key == key }) {
                let prior = engineSettings[idx]
                engineSettings[idx] = EngineSetting(
                    key: prior.key,
                    description: prior.description,
                    defaultEnabled: prior.defaultEnabled,
                    enabled: enabled
                )
            }
        case .metricsListLiveResult(let entries):
            engineMetrics = entries
        case .projectDesignDocResolved(let output):
            if var batch = currentDesignDocResolveBatch,
               batch.pendingProjectIDs.remove(output.projectID) != nil {
                if batch.pendingProjectIDs.isEmpty {
                    let ms = Int(Date().timeIntervalSince(batch.startDate) * 1000)
                    designDocTimingLog.info("phase=resolve project=batch count=\(batch.initialCount, privacy: .public) duration_ms=\(ms, privacy: .public)")
                    currentDesignDocResolveBatch = nil
                } else {
                    currentDesignDocResolveBatch = batch
                }
            }
            designDocStateByProjectID[output.projectID] = output.state
        case .conflictResolutionsList(let attempts):
            conflictResolutions = attempts
        case .conflictResolutionStarted(_, _, _, let prURL):
            // A (re)dispatch of the conflict resolver means the PR is
            // conflicting again — the prior "conflict cleared" badge is
            // stale and must be removed (T778). Mirrors the ciRemediationStarted
            // arm that clears recentlyClearedCIPRs for the same reason.
            recentlyClearedConflictPRs.removeValue(forKey: prURL)
            engine.sendListConflictResolutions(limit: 200)
        case .conflictResolutionFailed, .conflictResolutionAbandoned:
            // Refreshes the engine-tab list so the status column re-renders.
            // These don't touch the badge: failure/abandon don't un-clear a
            // previously cleared conflict — only a new start signals re-conflict.
            engine.sendListConflictResolutions(limit: 200)
        case .conflictResolutionSucceeded(_, _, _, let prURL):
            // Stamp the PR url so the kanban card shows the
            // "🔧 conflict cleared" chip for the next 24h (#15). The
            // engine doesn't carry a finished_at on the push, so we
            // record the wall-clock observation time — close enough
            // for an ageing window measured in hours.
            recentlyClearedConflictPRs[prURL] = Date()
            engine.sendListConflictResolutions(limit: 200)
        case .ciRemediationsList(let attempts):
            ciRemediations = attempts
            // Reconcile the in-flight chip set with the row list: for
            // every PR whose latest attempt is non-terminal, mark
            // `in_flight` if no chip already exists. Exhausted chips
            // are sticky until the user clears them via retry — they
            // are not derivable from the row list alone (the engine
            // tracks them via `task_blocked_signals`), so we leave
            // pre-existing exhausted chips alone.
            var seenPRs = Set<String>()
            for row in attempts where row.status == "pending" || row.status == "running" {
                guard seenPRs.insert(row.prURL).inserted else { continue }
                if ciFailureBadges[row.prURL] == nil {
                    ciFailureBadges[row.prURL] = CiFailureBadge(
                        state: .inFlight,
                        attemptsUsed: 0,
                        budget: 0,
                    )
                }
            }
        case .ciRemediationStarted(_, _, _, let prURL, _):
            // A fresh CI attempt was created (detect path or `retry`).
            // The card stays in `blocked: ci_failure` — the in-flight
            // chip lives until the next probe either reports clean or
            // hits the budget. We don't know used/budget here; the
            // exhausted arm carries those. Show a stub chip with
            // (0, 0) so the card surfaces the in-flight state until
            // the next list refresh fills in real numbers.
            // A new failure makes any prior "ci auto-fixed" claim stale:
            // if the auto-fix didn't stick, the badge is misleading (T606).
            recentlyClearedCIPRs.removeValue(forKey: prURL)
            if ciFailureBadges[prURL] == nil {
                ciFailureBadges[prURL] = CiFailureBadge(state: .inFlight, attemptsUsed: 0, budget: 0)
            } else if var existing = ciFailureBadges[prURL] {
                existing.state = .inFlight
                ciFailureBadges[prURL] = existing
            }
            engine.sendListCiRemediations(limit: 200)
        case .ciRemediationSucceeded(_, _, _, let prURL):
            // Engine observed CI back at clean and retired the attempt.
            // Drop the failure chip and stamp the "✅ ci auto-fixed"
            // chip for the next 24h (per design Q11).
            ciFailureBadges.removeValue(forKey: prURL)
            recentlyClearedCIPRs[prURL] = Date()
            engine.sendListCiRemediations(limit: 200)
        case .ciFailureCleared(_, _, let prURL):
            // Engine cleared `blocked: ci_failure` but found no active
            // remediation attempt (the prior attempt was already terminal).
            // Clear the failure badge only — do NOT set the auto-fixed badge
            // because the clearance was not driven by an auto-fix (T606).
            ciFailureBadges.removeValue(forKey: prURL)
        case .ciRemediationFailed(_, _, _, _, _),
             .ciRemediationAbandoned(_, _, _, _, _):
            // Terminal failures keep the parent `blocked: ci_failure`
            // until the engine either retries or exhausts. The list
            // refresh keeps the engine tab consistent.
            engine.sendListCiRemediations(limit: 200)
        case .ciRemediationExhausted(_, _, let prURL, let used, let budget):
            // Budget exhausted means CI is still failing and auto-fix
            // cannot help further. Any prior "ci auto-fixed" claim is now
            // stale (T606).
            recentlyClearedCIPRs.removeValue(forKey: prURL)
            ciFailureBadges[prURL] = CiFailureBadge(state: .exhausted, attemptsUsed: used, budget: budget)
            engine.sendListCiRemediations(limit: 200)
        case .attentionItemsForWorkItemList(let workItemID, let items):
            attentionItemsByWorkItemID[workItemID] = items
        case .attentionGroupsList(let productID, let groups, let members):
            applyAttentionGroupsList(productID: productID, groups: groups, members: members)
        case .attentionGroupResult(let group, let members):
            upsertAttentionGroup(group)
            attentionMembersByGroupID[group.id] = members
        case .attentionCreated(let attention, let group):
            upsertAttentionGroup(group)
            upsertAttentionMember(attention)
        case .attentionGroupUpdated(let group, let members):
            upsertAttentionGroup(group)
            attentionMembersByGroupID[group.id] = members
        case .attentionGroupActioned(let group, let members):
            upsertAttentionGroup(group)
            attentionMembersByGroupID[group.id] = members
        case .reviewTerminalReady(let workItemID, let workspacePath, let leaseID):
            openingReviewTerminalIDs.remove(workItemID)
            let resolved = task(withID: workItemID)
            let content = ReviewTerminalContent(
                workItemID: workItemID,
                workspacePath: workspacePath,
                leaseID: leaseID,
                taskName: resolved?.name,
                taskShortID: resolved?.shortID
            )
            if reviewTerminalVM.windowIsOpen {
                reviewTerminalVM.state = .ready(content)
            } else {
                // Window was closed while the engine was still setting up.
                // Release the lease immediately since nobody will consume it.
                engine.sendReleaseReviewTerminal(leaseID: leaseID)
            }
        case .mergeWhenReadyAccepted(let workItemID, _, _):
            // Engine successfully initiated the merge. Clear the in-flight
            // guard so the button re-enables if the user wants to retry.
            // The PR-reconciler was kicked on the engine side, so a
            // WorkItemUpdated event carrying the new merge-queue / merged
            // state will arrive shortly.
            mergingWhenReadyIDs.remove(workItemID)
        case .gitHubAuthState(let state):
            // The engine pushes this on every device-flow transition (and
            // as the reply to a `git_hub_auth_*` request). The settings
            // subsection observes `gitHubAuthState` and re-renders.
            gitHubAuthState = state
        case .executionsList(let taskId, let executions):
            executionsByTaskID[taskId] = executions
        case .executionTranscriptResult(let executionId, let segments, let isLive, let complete):
            transcriptsByExecutionID[executionId] = .loaded(
                TranscriptDoc(
                    executionId: executionId,
                    segments: segments,
                    isLive: isLive,
                    complete: complete
                )
            )
        case .executionTranscriptUnavailable(let executionId, let reason):
            transcriptsByExecutionID[executionId] = .unavailable(reason: reason)
        // MARK: Automation events
        case .automationsList(let productID, let automations):
            automationsByProductID[productID] = automations
            for automation in automations {
                engine.sendGetAutomationOpenTaskCount(automationId: automation.id)
                engine.sendListAutomationRuns(automationId: automation.id)
            }
        case .automationCreated(let automation):
            upsertAutomation(automation)
            selectedAutomationID = automation.id
            engine.sendGetAutomationOpenTaskCount(automationId: automation.id)
            engine.sendListAutomationRuns(automationId: automation.id)
        case .automationResult(let automation):
            upsertAutomation(automation)
            engine.sendListAutomationRuns(automationId: automation.id)
        case .automationUpdated(let automation):
            upsertAutomation(automation)
            engine.sendGetAutomationOpenTaskCount(automationId: automation.id)
            engine.sendListAutomationRuns(automationId: automation.id)
        case .automationDeleted(let automationID):
            for productID in automationsByProductID.keys {
                automationsByProductID[productID]?.removeAll { $0.id == automationID }
            }
            openTaskCountByAutomationID.removeValue(forKey: automationID)
            automationRunsByID.removeValue(forKey: automationID)
        case .automationOpenTaskCount(let automationID, let count):
            openTaskCountByAutomationID[automationID] = count
        case .automationRunsList(let automationID, let runs):
            automationRunsByID[automationID] = runs
        }
    }

    private func upsertAutomation(_ automation: AppAutomation) {
        let productID = automation.productID
        if var list = automationsByProductID[productID] {
            if let idx = list.firstIndex(where: { $0.id == automation.id }) {
                list[idx] = automation
            } else {
                list.append(automation)
            }
            automationsByProductID[productID] = list
        } else {
            automationsByProductID[productID] = [automation]
        }
    }

    // MARK: - Private Helpers

    var currentSelectedProductID: String? {
        selectedWorkProductID
    }

    private var taskCreationProject: WorkProject? {
        if let selectedProject {
            return selectedProject
        }
        if let selectedTask, let projectID = selectedTask.projectID {
            return project(withID: projectID)
        }
        return nil
    }

    private func workTopic(forProductID productID: String) -> String {
        "work.product.\(productID)"
    }

    private var desiredWorkTopics: Set<String> {
        // `github.auth` is a global (per-host, not per-product) topic
        // carrying GitHub OAuth auth-state pushes; the engine fans every
        // device-flow transition out on it. We stay subscribed for the
        // whole session so the "GitHub account" settings subsection
        // re-renders live (OAuth device-flow design §4, TOPIC_GITHUB_AUTH).
        var topics: Set<String> = ["work.products", "worker.live_states", "github.auth"]
        if let productID = currentSelectedProductID {
            topics.insert(workTopic(forProductID: productID))
        }
        return topics
    }

    private func refreshWorkSubscriptions() {
        guard isConnected else { return }
        let desired = desiredWorkTopics
        let toSubscribe = desired.subtracting(subscribedWorkTopics)
        let toUnsubscribe = subscribedWorkTopics.subtracting(desired)

        if !toUnsubscribe.isEmpty {
            engine.sendUnsubscribe(topics: Array(toUnsubscribe).sorted())
        }
        if !toSubscribe.isEmpty {
            engine.sendSubscribe(topics: Array(toSubscribe).sorted())
        }

        subscribedWorkTopics = desired
    }

    private func startEngineIfNeeded() {
        guard !didStartEngine else { return }
        didStartEngine = true
        engine.start()
    }

    /// Whether an `.error` message is a transport-level signal from
    /// `EngineClient` rather than a real engine-reported error.
    /// Transport errors are emitted on every reconnect attempt while
    /// the socket can't be opened, so they must not drive any modal
    /// UI — see the `.error` arm of `handle(_:)` for context.
    private func isSocketTransportError(_ message: String) -> Bool {
        return message.hasPrefix("socket failed:")
            || message.hasPrefix("socket waiting:")
            || message.hasPrefix("socket send failed:")
            || message.hasPrefix("socket receive failed:")
    }

    private func appendSystemMessage(_ text: String, alwaysShow: Bool = false) {
        guard alwaysShow || showSystemMessages else { return }
        FileHandle.standardError.write(Data("\(text)\n".utf8))
    }

    private func product(withID id: String) -> WorkProduct? {
        products.first { $0.id == id }
    }

    /// Lookup a project row by id across every product the model has
    /// loaded. Non-private so view code (the kanban project-card
    /// affordance) can resolve a section's `projectID` to a full
    /// `WorkProject` without re-walking the projects map itself.
    func project(withID id: String) -> WorkProject? {
        for projects in projectsByProductID.values {
            if let project = projects.first(where: { $0.id == id }) {
                return project
            }
        }
        return nil
    }

    func task(withID id: String) -> WorkTask? {
        for tasks in tasksByProjectID.values {
            if let task = tasks.first(where: { $0.id == id }) {
                return task
            }
        }
        for chores in choresByProductID.values {
            if let chore = chores.first(where: { $0.id == id }) {
                return chore
            }
        }
        // Chore-parented revisions are not in either bucket above; they live
        // in the product-level revision bucket (issue #789). Search it so the
        // revision card's parent lookup and other id resolution find them.
        for revisions in productLevelRevisionsByProductID.values {
            if let revision = revisions.first(where: { $0.id == id }) {
                return revision
            }
        }
        // Product-level investigations (and any other product-level kind) live
        // here; search it so card selection and detail lookups resolve them
        // (issue #886).
        for tasks in productLevelTasksByProductID.values {
            if let task = tasks.first(where: { $0.id == id }) {
                return task
            }
        }
        return nil
    }

    /// Look up any task or chore by id. Used by the kanban to resolve
    /// the parent task for revision card chrome.
    func workTask(withID id: String) -> WorkTask? {
        task(withID: id)
    }

    /// All `kind == "revision"` tasks whose `parentTaskId` matches the
    /// supplied id AND whose status is `"in_review"`. Used by the Review-
    /// lane parent card to render per-revision rollup lines.
    func inReviewRevisions(forParentTaskID parentID: String) -> [WorkTask] {
        let matches: (WorkTask) -> Bool = {
            $0.kind == "revision"
                && $0.parentTaskId == parentID
                && $0.status == "in_review"
        }
        var result: [WorkTask] = []
        // Project-task-parented revisions live under their project; chore-
        // parented ones live in the product-level bucket. Search both so the
        // parent's Review card rolls up every in-review revision regardless of
        // whether the chain root is a project_task or a chore (issue #789).
        for tasks in tasksByProjectID.values {
            result.append(contentsOf: tasks.filter(matches))
        }
        for revisions in productLevelRevisionsByProductID.values {
            result.append(contentsOf: revisions.filter(matches))
        }
        return result.sorted { ($0.revisionSeq ?? 0) < ($1.revisionSeq ?? 0) }
    }

    /// All `kind == "revision"` tasks whose `parentTaskId` matches the
    /// supplied id AND whose status is `"done"`. Used by the Done-lane
    /// parent card to render per-revision rollup lines.
    func doneRevisions(forParentTaskID parentID: String) -> [WorkTask] {
        let matches: (WorkTask) -> Bool = {
            $0.kind == "revision"
                && $0.parentTaskId == parentID
                && $0.status == "done"
        }
        var result: [WorkTask] = []
        for tasks in tasksByProjectID.values {
            result.append(contentsOf: tasks.filter(matches))
        }
        for revisions in productLevelRevisionsByProductID.values {
            result.append(contentsOf: revisions.filter(matches))
        }
        return result.sorted { ($0.revisionSeq ?? 0) < ($1.revisionSeq ?? 0) }
    }

    private func productID(for nodeID: WorkNodeID?) -> String? {
        switch nodeID {
        case .product(let productID):
            return productID
        case .project(let projectID):
            return project(withID: projectID)?.productID
        case .task(let taskID), .chore(let taskID):
            return task(withID: taskID)?.productID
        case nil:
            return nil
        }
    }

    private func productID(forProjectID projectID: String) -> String? {
        project(withID: projectID)?.productID
    }

    func projectName(for projectID: String?) -> String? {
        guard let projectID else { return nil }
        return project(withID: projectID)?.name
    }

    /// Project-badge text for a kanban card, or `nil` when the badge
    /// should be suppressed. Chores never have one; when the board is
    /// grouped by project the lane header already names the project,
    /// so the per-card badge would just duplicate the column header.
    func cardProjectBadge(for task: WorkTask) -> String? {
        if task.isChore { return nil }
        if workBoardGrouping == .project { return nil }
        return projectName(for: task.projectID)
    }

    /// Count of `todo` tasks for `projectID`. A `todo` task has no
    /// unsatisfied dependency gating it — if it did, the engine would
    /// have set `status = "blocked"`. These are tasks ready to dispatch.
    func unblockedTaskCount(forProjectID projectID: String) -> Int {
        (tasksByProjectID[projectID] ?? []).filter { $0.status == "todo" }.count
    }

    /// Count of dependency-blocked tasks for `projectID`. The engine
    /// sets `blocked_reason = "dependency"` when a task is gated by at
    /// least one unsatisfied prerequisite edge.
    func blockedTaskCount(forProjectID projectID: String) -> Int {
        (tasksByProjectID[projectID] ?? []).filter {
            $0.status == "blocked" && $0.blockedReason == "dependency"
        }.count
    }

    var unblockedChoreCount: Int {
        guard let productID = currentSelectedProductID else { return 0 }
        return (choresByProductID[productID] ?? []).filter { $0.status == "todo" }.count
    }

    var blockedChoreCount: Int {
        guard let productID = currentSelectedProductID else { return 0 }
        return (choresByProductID[productID] ?? []).filter {
            $0.status == "blocked" && $0.blockedReason == "dependency"
        }.count
    }

    /// Repo-chip mode for the kanban under the currently selected
    /// product. Drives both the product-header chip (single-repo) and
    /// the per-card chip (multi-repo) per design Q7. Computed off the
    /// *visible* work items so a project filter that hides the
    /// overriding card collapses the board back to single-repo
    /// presentation, matching the rule "every visible card resolves
    /// to the same URL".
    var workBoardRepoMode: WorkBoardRepoMode {
        guard let product = selectedProduct else { return .none }
        return WorkBoardRepoMode.compute(
            productRepoURL: product.repoRemoteURL,
            cards: visibleWorkItems
        )
    }

    /// Distinct repo URLs known under a product, ordered by recency
    /// of the work item they last appeared on. Drives both the Repo:
    /// row's `Change…` picker (per Follow-up chore #12) and the
    /// work-item create form's recent-repos picker (chore #10) so the
    /// two affordances agree on what counts as "recent". The product
    /// default is always first when set; the rest sort by the work
    /// item's `updatedAt` descending so the most-recently-edited
    /// repo bubbles up.
    ///
    /// Pure derivation over the in-memory snapshot — no RPC. Empty
    /// list is a legal answer (a brand-new product with no overrides
    /// and no default).
    func recentRepoURLs(forProduct productID: String) -> [String] {
        var seen = Set<String>()
        var ordered: [String] = []

        func push(_ value: String?) {
            guard let trimmed = nonEmptyTrim(value) else { return }
            if seen.insert(trimmed).inserted {
                ordered.append(trimmed)
            }
        }

        if let product = product(withID: productID) {
            push(product.repoRemoteURL)
        }

        var taskRows: [WorkTask] = []
        for project in projectsByProductID[productID] ?? [] {
            taskRows.append(contentsOf: tasksByProjectID[project.id] ?? [])
        }
        taskRows.append(contentsOf: choresByProductID[productID] ?? [])
        taskRows.append(contentsOf: productLevelTasksByProductID[productID] ?? [])
        let byRecency = taskRows.sorted { lhs, rhs in
            lhs.updatedAt > rhs.updatedAt
        }
        for task in byRecency {
            push(task.repoRemoteURL)
        }

        return ordered
    }

    /// Set or clear the per-work-item repo override. `url == nil` (or
    /// an empty/whitespace-only string) routes to the engine as
    /// `repo_remote_url = ""`, which is the patch shape that clears
    /// the column and falls back to product inheritance. No-ops when
    /// the new value equals the current one.
    func setRepoOverride(for taskID: String, to url: String?) {
        guard let task = task(withID: taskID) else { return }
        let trimmed = nonEmptyTrim(url) ?? ""
        let current = nonEmptyTrim(task.repoRemoteURL) ?? ""
        guard trimmed != current else { return }
        engine.sendUpdateWorkItem(id: task.id, patch: ["repo_remote_url": trimmed])
    }

    /// Repo-row presentation for the work-item detail popover. Wraps
    /// `RepoOverridePresentation.resolve` against the cached product
    /// row so the view stays a thin reflection of a value type.
    /// Returns `nil` only when the work item itself isn't loaded.
    func repoOverridePresentation(for task: WorkTask) -> RepoOverridePresentation {
        RepoOverridePresentation.resolve(
            task: task,
            product: product(withID: task.productID)
        )
    }

    private func nonEmptyTrim(_ value: String?) -> String? {
        guard let value else { return nil }
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }

    /// Per-card chip presentation, returning `nil` whenever the chip
    /// should not render: the board is in single-repo mode (chip
    /// already lives on the product header), or the card has no
    /// resolvable URL. Read by `WorkBoardCardView` to decide whether
    /// to draw the chip in the card header.
    func repoChip(for task: WorkTask) -> RepoChipPresentation? {
        switch workBoardRepoMode {
        case .singleRepo, .none:
            return nil
        case .multiRepo:
            let product = product(withID: task.productID)
            return RepoChipPresentation.forCard(
                task: task,
                productRepoURL: product?.repoRemoteURL
            )
        }
    }

    /// The column that `task` renders into, overriding `task.boardColumn`
    /// when a merge-resolution worker is actively running against it. A
    /// `blocked: merge_conflict` task with a `pending` or `running`
    /// conflict resolution routes to `.doing` so the kanban invariant
    /// holds: Doing = a worker is touching this right now.
    func effectiveBoardColumn(for task: WorkTask) -> WorkBoardColumnKey {
        // Optimistic override wins while a drag is in-flight.
        if let override = optimisticColumnByTaskID[task.id] {
            return override
        }
        if task.status == "blocked",
           task.blockedReason == "merge_conflict",
           let attemptID = task.blockedAttemptID,
           conflictResolutions.contains(where: {
               $0.id == attemptID && ($0.status == "pending" || $0.status == "running")
           }) {
            return .doing
        }
        if task.status == "blocked",
           task.blockedReason == "ci_failure",
           let attemptID = task.blockedAttemptID,
           ciRemediations.contains(where: {
               $0.id == attemptID && ($0.status == "pending" || $0.status == "running")
           }) {
            return .doing
        }
        return task.boardColumn
    }

    /// Effective board column based solely on real engine state, ignoring any
    /// in-flight optimistic override. Used during work-tree reconciliation to
    /// compare actual task state against the optimistic position.
    func realEffectiveBoardColumn(for task: WorkTask) -> WorkBoardColumnKey {
        if task.status == "blocked",
           task.blockedReason == "merge_conflict",
           let attemptID = task.blockedAttemptID,
           conflictResolutions.contains(where: {
               $0.id == attemptID && ($0.status == "pending" || $0.status == "running")
           }) {
            return .doing
        }
        if task.status == "blocked",
           task.blockedReason == "ci_failure",
           let attemptID = task.blockedAttemptID,
           ciRemediations.contains(where: {
               $0.id == attemptID && ($0.status == "pending" || $0.status == "running")
           }) {
            return .doing
        }
        return task.boardColumn
    }

    /// The active conflict resolution for `taskID`, if any. A resolution
    /// is "active" when its status is `pending` or `running`. Returns
    /// `nil` when no such attempt exists.
    func activeConflictResolution(for taskID: String) -> WorkConflictResolution? {
        conflictResolutions.first {
            $0.workItemID == taskID && ($0.status == "pending" || $0.status == "running")
        }
    }

    /// The active CI remediation for `taskID`, if any. A remediation is
    /// "active" when its status is `pending` or `running`. Returns `nil`
    /// when no such attempt exists. Parallel to [[activeConflictResolution(for:)]].
    func activeCiRemediation(for taskID: String) -> WorkCiRemediation? {
        ciRemediations.first {
            $0.workItemID == taskID && ($0.status == "pending" || $0.status == "running")
        }
    }

    func workItems(in column: WorkBoardColumnKey) -> [WorkTask] {
        if let cached = cachedItemsByColumn[column] {
            return cached
        }
        // The Review column gets a dedicated ordering: newest by creation
        // time at the top, so the column is predictable and scannable (the
        // generic board sort keys on `ordinal`, which review-phase tasks
        // rarely carry, leaving them in an apparently-random order). See
        // boss issue #1250.
        let sort = column == .review ? reviewBoardSort : boardTaskSort
        var items = visibleWorkItems
            .filter { effectiveBoardColumn(for: $0) == column }
            .sorted(by: sort)
        // Revisions don't appear as standalone cards in Review or Done — they
        // roll up as single lines on the parent task's card in both lanes.
        // They are still visible in Backlog/Doing as distinct cards.
        if column == .review {
            items = items.filter { !($0.kind == "revision" && $0.status == "in_review") }
        }
        if column == .done {
            items = items.filter { !($0.kind == "revision" && $0.status == "done") }
        }
        cachedItemsByColumn[column] = items
        return items
    }

    func workSections(in column: WorkBoardColumnKey) -> [WorkBoardSection] {
        if let cached = cachedSectionsByColumn[column] {
            return cached
        }
        let sections = computeWorkSections(in: column)
        cachedSectionsByColumn[column] = sections
        return sections
    }

    private func computeWorkSections(in column: WorkBoardColumnKey) -> [WorkBoardSection] {
        let items = workItems(in: column)
        if column == .done {
            return Self.doneSections(items: items)
        }
        guard workBoardGrouping == .project else {
            return [WorkBoardSection(id: column.rawValue, title: column.title, items: items)]
        }

        let grouped = Dictionary(grouping: items) { task in
            if task.isChore { return "Chores" }
            // Chore-parented revisions inherit nil projectID from the chain
            // root (a chore). Group them with chores so they don't land in
            // a confusing "No Project" section — they are logically part of
            // the chore world.
            if task.kind == "revision", task.projectID == nil { return "Chores" }
            return projectName(for: task.projectID) ?? "No Project"
        }

        return grouped.keys.sorted().compactMap { key in
            guard let sectionItems = grouped[key], !sectionItems.isEmpty else { return nil }
            let projectID = sectionItems.first(where: { !$0.isChore })?.projectID
            return WorkBoardSection(
                id: "\(column.rawValue)-\(key)",
                title: key,
                items: sectionItems,
                projectID: projectID
            )
        }
    }

    /// Cached output of `visibleWorkItems`. Filled lazily on read; reset to
    /// `nil` whenever a published input changes (see `invalidateWorkCache`).
    /// Keeps engine pushes that don't touch the work tree (e.g.
    /// `worker.live_states`) from re-walking the projects/tasks/chores trees.
    private var cachedVisibleItems: [WorkTask]?
    private var cachedItemsByColumn: [WorkBoardColumnKey: [WorkTask]] = [:]
    private var cachedSectionsByColumn: [WorkBoardColumnKey: [WorkBoardSection]] = [:]
    private var cachedAmbiguousRepoNames: Set<String>?

    func invalidateWorkCache() {
        cachedVisibleItems = nil
        cachedItemsByColumn.removeAll(keepingCapacity: true)
        cachedSectionsByColumn.removeAll(keepingCapacity: true)
        cachedAmbiguousRepoNames = nil
    }



    /// Inline drag-refusal banner shown next to the source card when a
    /// drag from Blocked → Doing is rejected because the row still has
    /// unsatisfied gating prereqs (design item 11). Single-slot — the
    /// previous notice is replaced when a new refusal fires.
    @Published var dragRefusalNotice: DragRefusalNotice?

    // MARK: - Optimistic kanban moves

    /// Optimistic column override for a card whose drop has been accepted
    /// in the UI but not yet confirmed by the engine. `effectiveBoardColumn`
    /// consults this before falling back to the real task status, giving an
    /// instant visual response on drop.
    var optimisticColumnByTaskID: [String: WorkBoardColumnKey] = [:]
    /// Origin column for each in-flight optimistic move. Kept until the
    /// engine's `workItemUpdated` event confirms the transition (at which
    /// point it is removed without clearing the override). If `work_error`
    /// arrives while entries remain here, the card bounces back.
    var pendingMoveOriginByTaskID: [String: WorkBoardColumnKey] = [:]



    /// Resolve a task to its current LiveWorkerState by joining
    /// `task → execution_id → run_id`. Returns `nil` when the task
    /// has no active execution or the engine has not yet seen any
    /// hook events for the run (so the live state map is empty).
    func workerLiveState(forTaskID taskID: String) -> WorkerLiveState? {
        guard let runtime = taskRuntimesByID[taskID],
              let executionID = runtime.executionID
        else {
            return nil
        }
        return liveWorkerStates.byRunID[executionID]
    }

    private func upsertProduct(_ product: WorkProduct) {
        if let index = products.firstIndex(where: { $0.id == product.id }) {
            products[index] = product
        } else {
            products.append(product)
            products.sort(by: { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending })
        }
    }

    private func handleCreatedWorkItem(_ item: WorkItemPayload) {
        workErrorMessage = nil
        switch item {
        case .product(let product):
            upsertProduct(product)
            selectedWorkProductID = product.id
            selectedProjectFilterIDs = []
            selectedWorkCardID = nil
            defaults.set(product.id, forKey: selectedWorkProductDefaultsKey)
            persistProjectFilterIDs()
            engine.sendGetWorkTree(productId: product.id)
        case .project(let project):
            selectedWorkProductID = project.productID
            selectedProjectFilterIDs = [project.id]
            selectedWorkCardID = nil
            defaults.set(project.productID, forKey: selectedWorkProductDefaultsKey)
            persistProjectFilterIDs()
            engine.sendGetWorkTree(productId: project.productID)
        case .task(let task):
            selectedWorkProductID = task.productID
            if let projectID = task.projectID {
                selectedProjectFilterIDs = [projectID]
            } else {
                selectedProjectFilterIDs = []
            }
            selectedWorkCardID = task.id
            defaults.set(task.productID, forKey: selectedWorkProductDefaultsKey)
            persistProjectFilterIDs()
            engine.sendGetWorkTree(productId: task.productID)
        case .chore(let task):
            selectedWorkProductID = task.productID
            selectedWorkCardID = task.id
            includeChores = true
            defaults.set(task.productID, forKey: selectedWorkProductDefaultsKey)
            defaults.set(true, forKey: includeChoresDefaultsKey)
            engine.sendGetWorkTree(productId: task.productID)
        }
        refreshWorkSubscriptions()
    }

    private func handleUpdatedWorkItem(_ item: WorkItemPayload) {
        switch item {
        case .product(let product):
            let wasSelected = selectedWorkProductID == product.id
            upsertProduct(product)
            if wasSelected && product.status == "archived" {
                workErrorMessage = "Product \"\(product.name)\" was archived; switching to the next active product."
                reconcileWorkSelection()
                if let nextID = selectedWorkProductID {
                    engine.sendGetWorkTree(productId: nextID)
                }
                refreshWorkSubscriptions()
                return
            }
        case .project(let project):
            engine.sendGetWorkTree(productId: project.productID)
        case .task(let updatedTask), .chore(let updatedTask):
            // When the engine confirms an optimistic move, drop the origin record
            // so a subsequent work_error from an unrelated operation won't bounce
            // a card that is already confirmed. The optimistic override itself
            // stays until reconcileOptimisticOverrides clears it after the full
            // work-tree arrives — removing it here would cause a flicker because
            // tasksByProjectID still holds the old status.
            if let targetColumn = optimisticColumnByTaskID[updatedTask.id],
               updatedTask.boardColumn == targetColumn {
                pendingMoveOriginByTaskID.removeValue(forKey: updatedTask.id)
            } else if optimisticColumnByTaskID[updatedTask.id] != nil {
                // Engine returned a different status — move silently rejected.
                bounceBackOptimisticMoves(message: nil)
            }
            maybeFireReviewNotification(for: updatedTask)
            engine.sendGetWorkTree(productId: updatedTask.productID)
        }
        workErrorMessage = nil
    }

    /// Fire a review notification when `updatedTask` enters `in_review`
    /// for the first time (not already in [[knownReviewTaskIDs]]).
    /// Clears the task from the set when it leaves `in_review` so a
    /// subsequent re-entry (e.g. worker re-opens a revised PR) fires again.
    private func maybeFireReviewNotification(for updatedTask: WorkTask) {
        if updatedTask.status == "in_review" {
            guard !knownReviewTaskIDs.contains(updatedTask.id) else { return }
            knownReviewTaskIDs.insert(updatedTask.id)
            reviewNotifier.notifyReadyForReview(task: updatedTask)
        } else {
            knownReviewTaskIDs.remove(updatedTask.id)
        }
    }

    /// Sync [[knownReviewTaskIDs]] from a full product work-tree snapshot
    /// without firing notifications. Called on initial load and reconnect
    /// so tasks already in Review at startup don't trigger spurious banners.
    private func seedReviewTaskIDs(tasks: [WorkTask], chores: [WorkTask], productID: String) {
        // Remove all IDs belonging to this product, then re-add the current in-review ones.
        // Avoids stale entries when a task leaves review between two tree snapshots.
        let productItemIDs = Set(tasks.map(\.id) + chores.map(\.id))
        knownReviewTaskIDs.subtract(productItemIDs)
        for item in tasks + chores where item.status == "in_review" {
            knownReviewTaskIDs.insert(item.id)
        }
    }

    private func reconcileWorkSelection() {
        guard let selectedWorkProductID else { return }

        let activeIDs = Set(activeProducts.map(\.id))
        if !activeIDs.contains(selectedWorkProductID) {
            self.selectedWorkProductID = activeProducts.first?.id
            if let firstProductID = activeProducts.first?.id {
                defaults.set(firstProductID, forKey: selectedWorkProductDefaultsKey)
            } else {
                defaults.removeObject(forKey: selectedWorkProductDefaultsKey)
            }
        }

        let validProjectIDs = selectedProjectFilterIDs.filter { projectID in
            project(withID: projectID)?.productID == selectedWorkProductID
        }
        if validProjectIDs != selectedProjectFilterIDs {
            selectedProjectFilterIDs = validProjectIDs
            persistProjectFilterIDs()
        }

        if let selectedTask, !isTaskVisible(selectedTask) {
            selectedWorkCardID = nil
        }

        refreshWorkSubscriptions()
    }

    /// Test-only entry point that funnels a synthetic engine event
    /// through the same `handle` dispatch the live socket uses, so
    /// picker-side reactions (selection fallback, archived-product
    /// fan-out) can be asserted without booting a real engine.
    func applyEventForTest(_ event: EngineEvent) {
        handle(event)
    }
}

private func projectSort(_ lhs: WorkProject, _ rhs: WorkProject) -> Bool {
    if lhs.createdAt == rhs.createdAt {
        return lhs.name.localizedCaseInsensitiveCompare(rhs.name) == .orderedAscending
    }
    return lhs.createdAt < rhs.createdAt
}

private func taskSort(_ lhs: WorkTask, _ rhs: WorkTask) -> Bool {
    switch (lhs.ordinal, rhs.ordinal) {
    case let (left?, right?) where left != right:
        return left < right
    default:
        if lhs.createdAt == rhs.createdAt {
            return lhs.name.localizedCaseInsensitiveCompare(rhs.name) == .orderedAscending
        }
        return lhs.createdAt < rhs.createdAt
    }
}

/// Ordering for the Review column: newest by creation time at the top.
/// `createdAt` is an RFC 3339 string, which sorts lexicographically in
/// chronological order, so a descending string compare yields newest-first.
/// Name then id break ties so the order is fully deterministic when two
/// cards share a `createdAt`. See boss issue #1250.
private func reviewBoardSort(_ lhs: WorkTask, _ rhs: WorkTask) -> Bool {
    if lhs.createdAt != rhs.createdAt {
        return lhs.createdAt > rhs.createdAt
    }
    let nameOrder = lhs.name.localizedCaseInsensitiveCompare(rhs.name)
    if nameOrder != .orderedSame {
        return nameOrder == .orderedAscending
    }
    return lhs.id < rhs.id
}

private func boardTaskSort(_ lhs: WorkTask, _ rhs: WorkTask) -> Bool {
    if lhs.status != rhs.status {
        if lhs.status == "blocked" {
            return true
        }
        if rhs.status == "blocked" {
            return false
        }
    }
    return taskSort(lhs, rhs)
}
