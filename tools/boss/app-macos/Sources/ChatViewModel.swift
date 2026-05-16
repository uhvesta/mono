import Foundation
#if canImport(AppKit)
import AppKit
#endif

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
    @Published var taskRuntimesByID: [String: WorkTaskRuntime] = [:]
    /// Dependency edges keyed by product. Refreshed whenever the engine
    /// pushes a fresh `WorkTree` for that product. The kanban joins
    /// these against the task/chore/project name maps to render
    /// "Blocked by <prereq title>" on gated cards.
    @Published var dependenciesByProductID: [String: [WorkItemDependency]] = [:]
    @Published var selectedWorkProductID: String? {
        didSet { invalidateWorkCache() }
    }
    @Published var selectedProjectFilterIDs: Set<String> = [] {
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

    /// Resolved design-doc pointer state per project. Populated lazily
    /// when a project surface (kanban project header, future detail
    /// view) calls `resolveProjectDesignDoc(_:)`; refreshed whenever
    /// the engine pushes a fresh `WorkTree` so a re-pointing or unset
    /// from another session lands in the icon without a manual reload.
    /// A missing entry means "we haven't asked yet" — the affordance
    /// stays hidden until the engine replies.
    @Published var designDocStateByProjectID: [String: ProjectDesignDocState] = [:]

    /// Engine-tab attempt list, freshest first. Refreshed on Engine-tab
    /// entry, on `conflict_resolution_*` topic pushes, and on `Refresh`
    /// button taps. Phase 5 #14 of the merge-conflict design.
    @Published var conflictResolutions: [WorkConflictResolution] = [] {
        didSet { invalidateWorkCache() }
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
    /// Production default hits `URLSession.shared`; tests inject a stub
    /// so the affordance tests never make live network calls.
    var rawContentFetcher: (URL) async throws -> String = { url in
        let (data, _) = try await URLSession.shared.data(from: url)
        guard let text = String(data: data, encoding: .utf8) else {
            throw URLError(.cannotDecodeContentData)
        }
        return text
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

    var selectedProject: WorkProject? {
        guard selectedProjectFilterIDs.count == 1,
              let projectID = selectedProjectFilterIDs.first else { return nil }
        return project(withID: projectID)
    }

    var projectFilterDescription: String {
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
        !visibleSelectedProjectFilterIDs.isEmpty
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

    private func computeVisibleWorkItems() -> [WorkTask] {
        guard let productID = currentSelectedProductID else { return [] }

        let query = workSearchText.trimmingCharacters(in: .whitespacesAndNewlines)
        let projectFilter = visibleSelectedProjectFilterIDs

        var items: [WorkTask] = []
        for project in projectsForSelectedProduct {
            guard projectFilter.isEmpty || projectFilter.contains(project.id) else { continue }
            items.append(contentsOf: (tasksByProjectID[project.id] ?? []).sorted(by: taskSort))
        }
        if includeChores && projectFilter.isEmpty {
            items.append(contentsOf: (choresByProductID[productID] ?? []).sorted(by: taskSort))
        }

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

    private let engine: EngineClient
    /// Test-only hook: forwarded to `EngineClient.outboundRecorder`
    /// so an XCTest can assert that the form's submit lands the
    /// expected `repo_remote_url` on the wire. The real socket write
    /// still runs (against a stub path that fails harmlessly in
    /// tests).
    var outboundRecorder: (([String: Any]) -> Void)? {
        get { engine.outboundRecorder }
        set { engine.outboundRecorder = newValue }
    }
    private let processController = EngineProcessController()
    private let socketPath: String
    private let showSystemMessages: Bool
    private var didStart = false
    private var didStartEngine = false
    private var hasConnectedOnce = false
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
    private let includeChoresDefaultsKey = "boss.work.includeChores"
    private let showBlockedOnlyDefaultsKey = "boss.work.showBlockedOnly"
    private let showArchivedProjectsDefaultsKey = "boss.work.showArchivedProjects"
    private let workBoardGroupingDefaultsKey = "boss.work.grouping"
    private let bossPanelCollapsedDefaultsKey = "boss.work.bossPanelCollapsed"
    private let bossPanelWidthDefaultsKey = "boss.work.bossPanelWidth"

    init(
        socketPath: String = ProcessInfo.processInfo.environment["BOSS_SOCKET_PATH"]
            ?? "/tmp/boss-engine.sock"
    ) {
        self.socketPath = socketPath
        let showSystem = ProcessInfo.processInfo.environment["BOSS_SHOW_SYSTEM_MESSAGES"] ?? ""
        showSystemMessages = showSystem == "1" || showSystem.lowercased() == "true"
        engine = EngineClient(socketPath: socketPath)

        if let rawMode = defaults.string(forKey: navigationModeDefaultsKey),
           let persistedMode = NavigationMode(rawValue: rawMode) {
            navigationMode = persistedMode
        }
        selectedWorkProductID = defaults.string(forKey: selectedWorkProductDefaultsKey)
        if let storedFilters = defaults.array(forKey: selectedProjectFilterIDsDefaultsKey) as? [String] {
            selectedProjectFilterIDs = Set(storedFilters)
        }
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
        }
    }

    func toggleProjectFilter(_ projectID: String) {
        if selectedProjectFilterIDs.contains(projectID) {
            selectedProjectFilterIDs.remove(projectID)
        } else {
            selectedProjectFilterIDs.insert(projectID)
        }
        selectedWorkCardID = nil
        persistProjectFilterIDs()
    }

    func clearProjectFilters() {
        guard !selectedProjectFilterIDs.isEmpty else { return }
        selectedProjectFilterIDs = []
        selectedWorkCardID = nil
        persistProjectFilterIDs()
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

    private func persistProjectFilterIDs() {
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
        prURL: String = ""
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
    ///   blocked. The kanban must not show a card in a column whose
    ///   status contradicts the live-worker reality. Terminal
    ///   transitions (`done`, `archived`) are still allowed because
    ///   those mirror the engine's own lifecycle resolutions.
    func moveTask(_ taskID: String, to column: WorkBoardColumnKey) {
        guard let task = task(withID: taskID) else { return }
        let targetStatus = column.targetStatus
        guard task.status != targetStatus else { return }

        if task.status == "active"
            && !Self.terminalKanbanStatuses.contains(targetStatus)
            && hasLiveWorker(forTaskID: taskID)
        {
            appendSystemMessage(
                "\(task.name) is being worked on by a live worker. Stop the worker before moving the card out of Doing.",
                alwaysShow: true
            )
            return
        }

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
        didStart = true

        let autostart = ProcessInfo.processInfo.environment["BOSS_ENGINE_AUTOSTART"] != "0"
        if autostart {
            let socketPath = self.socketPath
            let processController = self.processController
            DispatchQueue.global(qos: .userInitiated).async { [weak self] in
                do {
                    try processController.start(socketPath: socketPath)
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
        for project in projects where project.designDocPath != nil {
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
                if let opener = asyncMarkdownViewerOpener {
                    // Open the window immediately in a loading state, then
                    // resolve the content asynchronously — the user sees a
                    // window within one frame of the click (T-open-immediately).
                    asyncMarkdownViewerVM.state = .loading
                    opener()
                    Task { @MainActor in
                        await self.fetchAndUpdateAsyncMarkdownViewerVM(
                            projectName: projectName,
                            rawURL: rawURL
                        )
                    }
                } else {
                    // Headless / test path: fetch first, then open via the
                    // legacy markdownViewerOpener (or fall back to urlOpener).
                    Task { @MainActor in
                        await self.fetchAndOpenDesignDoc(
                            projectName: projectName,
                            rawURL: rawURL,
                            webURL: webURL
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
            urlOpener(url)
        }
    }

    /// Fetch raw markdown from `rawURL` and open it in the
    /// [[markdownViewerOpener]] window. Falls back to `urlOpener(webURL)`
    /// if the fetch fails or [[markdownViewerOpener]] is not wired.
    @MainActor
    private func fetchAndOpenDesignDoc(projectName: String, rawURL: URL, webURL: String) async {
        do {
            let markdown = try await rawContentFetcher(rawURL)
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
        rawURL: URL
    ) async {
        let title = projectName.isEmpty ? rawURL.lastPathComponent : projectName
        do {
            let markdown = try await rawContentFetcher(rawURL)
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

    // MARK: - Event Handling

    var paneSpawnHandler: ((EngineSpawnRequest) -> EngineSpawnResult)?
    var paneReleaseHandler: ((Int, UInt32) -> EngineReleaseResult)?
    var paneSendHandler: ((Int, String) -> EngineSendResult)?
    var paneFocusHandler: ((Int) -> EngineFocusResult)?
    var paneInterruptHandler: ((Int) -> EngineInterruptResult)?

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
            if let productID = currentSelectedProductID {
                engine.sendGetWorkTree(productId: productID)
            }
        case .appSessionRegistered:
            // No additional state for now; the engine has confirmed
            // this client is the registered app session.
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
            }
        case .disconnected:
            isConnected = false
            subscribedWorkTopics.removeAll()
        case .workInvalidated(let topic, let productId, _):
            if topic == "work.products" {
                engine.sendListProducts()
            }
            if let selectedProductID = currentSelectedProductID,
               topic == workTopic(forProductID: selectedProductID)
            {
                engine.sendGetWorkTree(productId: selectedProductID)
            } else if let productId,
                      productId == currentSelectedProductID {
                engine.sendGetWorkTree(productId: productId)
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
            for task in tasks {
                guard let projectID = task.projectID else { continue }
                tasksByProjectID[projectID, default: []].append(task)
            }
            for (projectID, projectTasks) in tasksByProjectID where
                projectTasks.first?.productID == product.id {
                tasksByProjectID[projectID] = projectTasks.sorted(by: taskSort)
            }
            choresByProductID[product.id] = chores.sorted(by: taskSort)
            mergeTaskRuntimes(taskRuntimes, for: product.id, tasks: tasks, chores: chores)
            dependenciesByProductID[product.id] = dependencies
            seedReviewTaskIDs(tasks: tasks, chores: chores, productID: product.id)
            reconcileWorkSelection()
            refreshWorkSubscriptions()
            refreshDesignDocStates(for: projects)
            workErrorMessage = nil
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
            workErrorMessage = message
        case .error(let message):
            if shouldSuppressSocketStartupError(message) { return }
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
            designDocStateByProjectID[output.projectID] = output.state
        case .conflictResolutionsList(let attempts):
            conflictResolutions = attempts
        case .conflictResolutionStarted, .conflictResolutionFailed,
             .conflictResolutionAbandoned:
            // Any transition refreshes the engine-tab list so the
            // status column re-renders. The badge state only updates
            // on the `succeeded` arm.
            engine.sendListConflictResolutions(limit: 200)
        case .conflictResolutionSucceeded(_, _, _, let prURL):
            // Stamp the PR url so the kanban card shows the
            // "🔧 conflict cleared" chip for the next 24h (#15). The
            // engine doesn't carry a finished_at on the push, so we
            // record the wall-clock observation time — close enough
            // for an ageing window measured in hours.
            recentlyClearedConflictPRs[prURL] = Date()
            engine.sendListConflictResolutions(limit: 200)
        }
    }

    /// Engine-tab entry point: ask the engine for the current attempt
    /// list. Idempotent — the view-model just overwrites the array
    /// when the reply lands.
    func refreshConflictResolutions() {
        engine.sendListConflictResolutions(limit: 200)
    }

    // MARK: - Private Helpers

    private var currentSelectedProductID: String? {
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
        var topics: Set<String> = ["work.products", "worker.live_states"]
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

    private func shouldSuppressSocketStartupError(_ message: String) -> Bool {
        guard !showSystemMessages, !hasConnectedOnce else { return false }
        return message.hasPrefix("socket failed:") || message.hasPrefix("socket waiting:")
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

    private func task(withID id: String) -> WorkTask? {
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
        return nil
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
        if task.status == "blocked",
           task.blockedReason == "merge_conflict",
           let attemptID = task.blockedAttemptID,
           conflictResolutions.contains(where: {
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

    func workItems(in column: WorkBoardColumnKey) -> [WorkTask] {
        if let cached = cachedItemsByColumn[column] {
            return cached
        }
        let items = visibleWorkItems
            .filter { effectiveBoardColumn(for: $0) == column }
            .sorted(by: boardTaskSort)
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
            task.isChore ? "Chores" : (projectName(for: task.projectID) ?? "No Project")
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

    private func invalidateWorkCache() {
        cachedVisibleItems = nil
        cachedItemsByColumn.removeAll(keepingCapacity: true)
        cachedSectionsByColumn.removeAll(keepingCapacity: true)
    }

    /// Bucket completed tasks by recency for the Done lane:
    ///   Today | Yesterday | <weekday names back to start of current week>
    ///   | Last Week | Earlier
    /// Bucketing uses `updated_at` because the engine doesn't yet record a
    /// dedicated `done_at` (status-transition) timestamp — see PR description.
    static func doneSections(
        items: [WorkTask],
        now: Date = Date(),
        calendar: Calendar = .current
    ) -> [WorkBoardSection] {
        let isoFractional = ISO8601DateFormatter()
        isoFractional.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        let isoPlain = ISO8601DateFormatter()
        isoPlain.formatOptions = [.withInternetDateTime]

        let nowDay = calendar.startOfDay(for: now)
        guard let yesterdayDay = calendar.date(byAdding: .day, value: -1, to: nowDay) else {
            return [WorkBoardSection(id: "done-all", title: "Done", items: items)]
        }
        let weekday = calendar.component(.weekday, from: nowDay)
        let firstWeekday = calendar.firstWeekday
        let daysSinceStartOfWeek = (weekday - firstWeekday + 7) % 7
        guard let startOfWeek = calendar.date(byAdding: .day, value: -daysSinceStartOfWeek, to: nowDay),
              let startOfLastWeek = calendar.date(byAdding: .day, value: -7, to: startOfWeek)
        else {
            return [WorkBoardSection(id: "done-all", title: "Done", items: items)]
        }

        let weekdayFormatter = DateFormatter()
        weekdayFormatter.locale = .current
        weekdayFormatter.dateFormat = "EEEE"

        struct BucketSpec {
            let id: String
            let title: String
            let defaultExpanded: Bool
        }

        var bucketOrder: [BucketSpec] = [
            BucketSpec(id: "today", title: "Today", defaultExpanded: true),
            BucketSpec(id: "yesterday", title: "Yesterday", defaultExpanded: false),
        ]
        if daysSinceStartOfWeek >= 2 {
            for daysAgo in 2...daysSinceStartOfWeek {
                if let date = calendar.date(byAdding: .day, value: -daysAgo, to: nowDay) {
                    bucketOrder.append(
                        BucketSpec(
                            id: "weekday-\(daysAgo)",
                            title: weekdayFormatter.string(from: date),
                            defaultExpanded: false
                        )
                    )
                }
            }
        }
        bucketOrder.append(BucketSpec(id: "last-week", title: "Last Week", defaultExpanded: false))
        bucketOrder.append(BucketSpec(id: "earlier", title: "Earlier", defaultExpanded: false))

        var buckets: [String: [WorkTask]] = [:]
        for task in items {
            let bucketID = bucketID(
                for: task,
                nowDay: nowDay,
                yesterdayDay: yesterdayDay,
                startOfWeek: startOfWeek,
                startOfLastWeek: startOfLastWeek,
                calendar: calendar,
                isoFormatters: [isoFractional, isoPlain]
            )
            buckets[bucketID, default: []].append(task)
        }

        return bucketOrder.compactMap { spec -> WorkBoardSection? in
            guard let tasks = buckets[spec.id], !tasks.isEmpty else { return nil }
            let sorted = tasks.sorted { $0.updatedAt > $1.updatedAt }
            return WorkBoardSection(
                id: "done-\(spec.id)",
                title: spec.title,
                items: sorted,
                isCollapsible: true,
                defaultExpanded: spec.defaultExpanded
            )
        }
    }

    private static func bucketID(
        for task: WorkTask,
        nowDay: Date,
        yesterdayDay: Date,
        startOfWeek: Date,
        startOfLastWeek: Date,
        calendar: Calendar,
        isoFormatters: [ISO8601DateFormatter]
    ) -> String {
        guard let parsed = parseUpdatedAt(task.updatedAt, isoFormatters: isoFormatters) else {
            return "earlier"
        }
        let day = calendar.startOfDay(for: parsed)
        if day >= nowDay {
            return "today"
        }
        if day >= yesterdayDay {
            return "yesterday"
        }
        if day >= startOfWeek {
            let delta = calendar.dateComponents([.day], from: day, to: nowDay).day ?? 0
            return "weekday-\(delta)"
        }
        if day >= startOfLastWeek {
            return "last-week"
        }
        return "earlier"
    }

    /// `boss chore list --json` currently emits `updated_at` in two
    /// shapes — Unix epoch seconds as a digit string for older rows
    /// and ISO 8601 for newer ones. The UI must handle both until the
    /// data-shape canonicalization chore lands; treat all-digit
    /// strings as Unix seconds, otherwise fall back to ISO parsing.
    static func parseUpdatedAt(
        _ string: String,
        isoFormatters: [ISO8601DateFormatter]
    ) -> Date? {
        let trimmed = string.trimmingCharacters(in: .whitespaces)
        if !trimmed.isEmpty,
           trimmed.allSatisfy({ $0.isASCII && $0.isNumber }),
           let seconds = TimeInterval(trimmed) {
            return Date(timeIntervalSince1970: seconds)
        }
        for formatter in isoFormatters {
            if let date = formatter.date(from: trimmed) {
                return date
            }
        }
        return nil
    }

    func isTaskVisible(_ task: WorkTask) -> Bool {
        workItems(in: effectiveBoardColumn(for: task)).contains(where: { $0.id == task.id })
    }

    private func mergeTaskRuntimes(
        _ runtimes: [WorkTaskRuntime],
        for productID: String,
        tasks: [WorkTask],
        chores: [WorkTask]
    ) {
        let productItemIDs = Set(tasks.map(\.id) + chores.map(\.id))
        taskRuntimesByID = taskRuntimesByID.filter { !productItemIDs.contains($0.key) }
        for runtime in runtimes {
            taskRuntimesByID[runtime.workItemID] = runtime
        }
    }

    func taskRuntime(for taskID: String) -> WorkTaskRuntime? {
        taskRuntimesByID[taskID]
    }

    /// Resolve the human-readable label for the rows currently gating
    /// `task` — i.e. its incomplete `blocks` prerequisites. Used by
    /// the kanban card to show "Blocked by <prereq title>" under the
    /// task name when the engine has parked the row in `blocked`. The
    /// caller is expected to gate on `task.status == "blocked"` so we
    /// don't compute this for cards that aren't rendering the badge.
    func blockedByLabel(for task: WorkTask) -> String? {
        let edges = dependenciesByProductID[task.productID] ?? []
        guard !edges.isEmpty else { return nil }
        let names: [String] = edges.compactMap { edge in
            guard edge.dependentID == task.id, edge.relation == "blocks" else {
                return nil
            }
            guard let name = workItemName(for: edge.prerequisiteID),
                  !isWorkItemSatisfied(edge.prerequisiteID)
            else {
                return nil
            }
            return name
        }
        guard !names.isEmpty else { return nil }
        return names.joined(separator: ", ")
    }

    /// All `blocks` prereqs for `task` joined against the work tree,
    /// rendered in card-detail and tooltip order. Includes already-
    /// satisfied edges so the popover can show the full picture (the
    /// chain badge tooltip and the auto-block predicate filter further
    /// for "incomplete" only).
    func dependencyPrereqs(for taskID: String) -> [WorkDependencyRow] {
        guard let productID = task(withID: taskID)?.productID
            ?? project(withID: taskID)?.productID
        else {
            return []
        }
        let edges = dependenciesByProductID[productID] ?? []
        return edges
            .filter { $0.dependentID == taskID && $0.relation == "blocks" }
            .map { workDependencyRow(forID: $0.prerequisiteID) }
    }

    /// All `blocks` dependents of `taskID`. Used by the card detail
    /// Dependencies subsection to show "what does this gate?".
    func dependencyDependents(for taskID: String) -> [WorkDependencyRow] {
        guard let productID = task(withID: taskID)?.productID
            ?? project(withID: taskID)?.productID
        else {
            return []
        }
        let edges = dependenciesByProductID[productID] ?? []
        return edges
            .filter { $0.prerequisiteID == taskID && $0.relation == "blocks" }
            .map { workDependencyRow(forID: $0.dependentID) }
    }

    /// Subset of `dependencyPrereqs` that are still gating the row —
    /// i.e. not yet in a satisfied status. Drives the chain badge's
    /// hover tooltip ("gated by …") and the auto-block predicate.
    func gatingPrereqs(for taskID: String) -> [WorkDependencyRow] {
        dependencyPrereqs(for: taskID).filter { !isWorkItemSatisfied($0.id) }
    }

    /// True iff the engine parked the row in `blocked` (rather than the
    /// user choosing it). The chain badge appears only for these rows
    /// per design Q7 — manual blocks already get the lane and would
    /// double up with the icon.
    func isAutoBlocked(_ task: WorkTask) -> Bool {
        task.status == "blocked"
            && task.lastStatusActor == "engine"
            && !gatingPrereqs(for: task.id).isEmpty
    }

    /// True iff the row currently has at least one unsatisfied gating
    /// prereq. Drag refusal keys on this rather than `lastStatusActor`
    /// because the engine refuses *any* manual move out of `blocked`
    /// while gated, regardless of who set the status last (Q4).
    func hasGatingPrereqs(_ task: WorkTask) -> Bool {
        !gatingPrereqs(for: task.id).isEmpty
    }

    private func workDependencyRow(forID id: String) -> WorkDependencyRow {
        if id.hasPrefix("proj_") {
            if let project = project(withID: id) {
                return WorkDependencyRow(
                    id: project.id,
                    title: project.name,
                    status: project.status,
                    kind: .project
                )
            }
        } else if let task = task(withID: id) {
            return WorkDependencyRow(
                id: task.id,
                title: task.name,
                status: task.status,
                kind: task.isChore ? .chore : .task
            )
        }
        return WorkDependencyRow(id: id, title: id, status: "unknown", kind: .unknown)
    }

    /// Inline drag-refusal banner shown next to the source card when a
    /// drag from Blocked → Doing is rejected because the row still has
    /// unsatisfied gating prereqs (design item 11). Single-slot — the
    /// previous notice is replaced when a new refusal fires.
    @Published private(set) var dragRefusalNotice: DragRefusalNotice?

    struct DragRefusalNotice: Equatable {
        let taskID: String
        let message: String
    }

    /// Variant of `moveTask` that returns whether the drop was
    /// accepted. Used by the kanban's `dropDestination` so the source
    /// lane can render an inline warning when the engine would refuse
    /// the underlying status change.
    func attemptMoveTask(_ taskID: String, to column: WorkBoardColumnKey) -> Bool {
        guard let task = task(withID: taskID) else { return false }
        let targetStatus = column.targetStatus

        // Dispatch-pending rows (status=todo, autostart=true) show in the
        // Doing column but their status is already "todo" — same as Backlog's
        // targetStatus. The normal status-equality guard would silently refuse
        // the drop. Instead, accept Doing→Backlog for these rows and flip
        // autostart=false; lane routing then moves the card to Backlog.
        let isDispatchPending = task.status == "todo" && task.autostart
        if isDispatchPending && column == .backlog {
            engine.sendUpdateWorkItem(id: task.id, patch: ["autostart": false])
            return true
        }

        guard task.status != targetStatus else { return false }

        if task.status == "blocked",
           targetStatus != "blocked",
           hasGatingPrereqs(task)
        {
            let count = gatingPrereqs(for: task.id).count
            let plural = count == 1 ? "prerequisite" : "prerequisites"
            dragRefusalNotice = DragRefusalNotice(
                taskID: task.id,
                message: "\(task.name) is gated by \(count) incomplete \(plural) — clear them or remove the edge first."
            )
            scheduleDragRefusalDismiss(for: task.id)
            return false
        }

        moveTask(taskID, to: column)
        return true
    }

    func clearDragRefusal() {
        dragRefusalNotice = nil
    }

    private func scheduleDragRefusalDismiss(for taskID: String) {
        Task { [weak self] in
            try? await Task.sleep(nanoseconds: 5_000_000_000)
            await MainActor.run { [weak self] in
                guard let self,
                      self.dragRefusalNotice?.taskID == taskID
                else { return }
                self.dragRefusalNotice = nil
            }
        }
    }

    private func workItemName(for id: String) -> String? {
        if id.hasPrefix("proj_") {
            return project(withID: id)?.name
        }
        return task(withID: id)?.name
    }

    /// Mirrors the engine's `status_satisfies` rule: a task/chore is
    /// satisfied at `done`; a project is satisfied at `done` or
    /// `archived`. Used to hide already-finished prereqs from the
    /// "Blocked by …" label on the off-chance an edge survives a
    /// status change momentarily.
    private func isWorkItemSatisfied(_ id: String) -> Bool {
        if id.hasPrefix("proj_") {
            guard let status = project(withID: id)?.status else { return false }
            return status == "done" || status == "archived"
        }
        guard let status = task(withID: id)?.status else { return false }
        return status == "done"
    }

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
