import AppKit
import os.log
import SwiftUI
import UpdateCore

private let workBoardColumnWidth: CGFloat = 280

// Debug logger for the investigation doc-link render path. Uses .debug() so
// it is silent in normal use; enable via Console.app subsystem filter or
// Xcode debug console. Surfaces work_item_id, kind, pr_url value, column,
// and whether PRURLLink will render — letting the operator identify which
// of the three known gap sites (delivery, render, stale build) is live.
private let kanbanDocLinkLog = Logger(
    subsystem: "dev.spinyfin.bossmacapp",
    category: "kanban-doc-link"
)
private let workBoardColumnWidthWide: CGFloat = 340
private let workBoardColumnWidthMax: CGFloat = 420
private let workBoardWideThreshold: CGFloat = 1400
private let workBoardUltraWideThreshold: CGFloat = 1800
private let workBoardColumnSpacing: CGFloat = 12
private let workBoardHorizontalPadding: CGFloat = 20
private let workBossPanelDefaultExpandedWidth: CGFloat = 380
private let workBossPanelMinWidth: CGFloat = 280
private let workBossPanelMaxWidth: CGFloat = 600
private let workBossPanelCollapsedWidth: CGFloat = 88
private let workBossPanelDividerHitWidth: CGFloat = 12

struct ContentView: View {
    @EnvironmentObject private var model: ChatViewModel
    @EnvironmentObject private var updateModel: UpdateModel
    #if canImport(GhosttyKit)
    @StateObject private var workersWorkspace = WorkersWorkspaceModel()
    @StateObject private var bossPane = BossPaneModel()
    #endif
    @State private var isSearchExpanded: Bool = false
    @State private var workColumnVisibility: NavigationSplitViewVisibility = .all
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        // Work and Agents are kept alive via opacity + hit-testing so SwiftUI
        // doesn't tear down the libghostty NSViews on tab switches (teardown
        // would force ghostty_surface_new and restart every claude session).
        // DesignsView is structurally conditional because it contains its own
        // NavigationSplitView: two NSVs mounted concurrently share the same
        // NSWindow toolbar namespace and AppKit deduplicates their toggle
        // items, causing position thrash and a missing Designs sidebar. Only
        // one NSV may live in the tree at a time. Designs remounts cheaply
        // (filesystem reads only) so structural conditional is safe here.
        ZStack {
            NavigationSplitView(columnVisibility: $workColumnVisibility) {
                sidebar
            } detail: {
                detail
            }
            // Remove the system sidebarToggle only on non-Work tabs. On the Work
            // tab, the system-provided toggle handles both expanded and collapsed
            // states natively, giving exactly one toggle button in either state
            // without a state-conditional custom button (the root cause of the
            // T479/T612 recurrence: suppressing one button in one collapse state
            // always left the other button visible in the opposite state).
            .toolbar(removing: model.navigationMode == .work ? nil : .sidebarToggle)
            .opacity(model.navigationMode == .work ? 1 : 0)
            .allowsHitTesting(model.navigationMode == .work)

            agentsView
                .opacity(model.navigationMode == .agents ? 1 : 0)
                .allowsHitTesting(model.navigationMode == .agents)

            if model.navigationMode == .designs {
                DesignsView(chat: model)
            }

            if model.navigationMode == .automations {
                AutomationsView(model: model)
                    .background(Color(nsColor: .windowBackgroundColor).ignoresSafeArea())
            }

        }
        .safeAreaInset(edge: .top, spacing: 0) {
            // Persistent chrome-level signal that the engine socket is
            // down. Only shown after we've connected at least once so
            // the banner doesn't flash on launch during the normal
            // initial-connect window. Replaces the previous behavior
            // where every reconnect attempt re-popped a "Work Error"
            // modal (#698) — transport errors are now routed away from
            // `workErrorMessage` in `ChatViewModel.handle`.
            VStack(spacing: 0) {
                if !model.isConnected && model.hasConnectedOnce {
                    EngineUnreachableBanner(
                        isRestarting: model.isRestartingEngine,
                        onRestart: { model.restartEngine() }
                    )
                    .transition(.move(edge: .top).combined(with: .opacity))
                }
                // Connection is up but the engine reports a degraded
                // condition (missing ANTHROPIC_API_KEY, dispatch paused,
                // syspolicyd wedged, etc.). Surface as a first-class
                // affordance so operators can't miss it (#699).
                if model.isConnected, !model.engineHealthIssues.isEmpty {
                    EngineHealthBanner(issues: model.engineHealthIssues)
                        .transition(.move(edge: .top).combined(with: .opacity))
                }
            }
        }
        .animation(.easeInOut(duration: 0.15), value: model.isConnected)
        .animation(.easeInOut(duration: 0.15), value: model.engineHealthIssues)
        #if canImport(GhosttyKit)
        .task {
            // Wire the SwiftPM-only pane allocator into ChatViewModel
            // so EngineRequest events from the engine route through to
            // WorkersWorkspaceModel. Bazel builds without GhosttyKit
            // leave the handlers nil; ChatViewModel responds with
            // EngineToAppError::Internal in that path.
            model.paneSpawnHandler = { [workspace = workersWorkspace] request in
                workspace.spawnWorkerPane(request)
            }
            model.paneReleaseHandler = { [workspace = workersWorkspace] slotId, killGrace in
                workspace.releaseWorkerPane(slotId: slotId, killGraceSeconds: killGrace)
            }
            model.paneSendHandler = { [workspace = workersWorkspace] slotId, text in
                workspace.sendToPane(slotId: slotId, text: text)
            }
            model.paneFocusHandler = { [workspace = workersWorkspace] slotId in
                workspace.focusWorkerPane(slotId: slotId)
            }
            model.paneInterruptHandler = { [workspace = workersWorkspace] slotId in
                workspace.interruptWorkerPane(slotId: slotId)
            }
            // Forward pool-config pushes from the engine so WorkersWorkspaceModel
            // always uses the engine's live pool sizes rather than independently-
            // maintained constants that drift when pool sizes change.
            model.panePoolConfigHandler = { [workspace = workersWorkspace] workerSlots, automationSlots, reviewSlots in
                workspace.configureSlots(workerCount: workerSlots, automationCount: automationSlots, reviewCount: reviewSlots)
            }
            // Install the Boss-pane shell-pid provider so the engine can
            // authenticate Boss-tier RPCs (e.g. `bossctl agents reap`).
            // The closure is re-evaluated on every call, so it picks up
            // the current surface pid after a Boss-pane restart.
            model.bossPaneShellPidProvider = { [boss = bossPane] in
                boss.session.shellPid
            }
            // Fire whenever the surface is (re-)attached — covers initial
            // creation and restarts after the coordinator session exits.
            bossPane.session.onSurfaceAttached = { [model] in
                model.bossPaneShellPidAvailable()
            }
            // Handle the race where the surface was attached before this
            // task ran (most common at startup).
            if bossPane.session.terminalReady {
                model.bossPaneShellPidAvailable()
            }
            // Forward worker-pane shell pids to the engine once surfaces
            // attach. WorkersWorkspaceModel fires onShellPidAvailable after
            // ghostty_surface_foreground_pid returns a valid pid so the
            // engine can wire process tracking for reviewer and other panes.
            workersWorkspace.onShellPidAvailable = { [model] runId, shellPid in
                model.workerPaneShellPidAvailable(runId: runId, shellPid: shellPid)
            }
        }
        #endif
        .frame(minWidth: 860, minHeight: 560)
        .navigationTitle(model.selectedProduct?.name ?? "Boss")
        .task {
            // Hand the SwiftUI `openWindow` action to the view model
            // so its design-doc dispatch can open the in-app renderer
            // window. The view model can't reach `@Environment` from
            // its own scope; injecting via a closure is how all the
            // other view-model boundaries (pane allocator above,
            // urlOpener) cross the same line.
            model.designRendererOpener = { [openWindow] content in
                openWindow(id: "design-renderer", value: content)
            }
            model.markdownViewerOpener = { [openWindow] content in
                openWindow(id: "markdown-viewer", value: content)
            }
            model.asyncMarkdownViewerOpener = { [openWindow] in
                openWindow(id: "async-markdown-viewer")
            }
            model.reviewTerminalOpener = { [openWindow] in
                openWindow(id: "review-terminal")
            }
            model.startIfNeeded()
        }
        .toolbar {
            ToolbarItem(placement: .navigation) {
                Picker("Mode", selection: Binding(
                    get: { model.navigationMode },
                    set: { model.setNavigationMode($0) }
                )) {
                    ForEach(NavigationMode.allCases) { mode in
                        Text(mode.rawValue).tag(mode)
                    }
                }
                .pickerStyle(.segmented)
                .frame(width: 360)
            }

            ToolbarItem {
                if model.navigationMode == .work {
                    Menu {
                        Button("New Product") {
                            model.presentCreateProduct()
                        }
                        .disabled(!model.isConnected)

                        Button("New Project") {
                            model.presentCreateProject()
                        }
                        .disabled(model.selectedProduct == nil || !model.isConnected)

                        Button("New Task") {
                            model.presentCreateTask()
                        }
                        .disabled(model.selectedProject == nil || !model.isConnected)

                        Button("New Chore") {
                            model.presentCreateChore()
                        }
                        .disabled(model.selectedProduct == nil || !model.isConnected)
                    } label: {
                        Label("New", systemImage: "plus")
                    }
                }
            }

            ToolbarItemGroup(placement: .primaryAction) {
                if model.navigationMode == .work {
                    WorkProjectFilterToolbarButton(model: model)
                    WorkGroupToolbarMenu(model: model)
                    WorkSearchToolbarItem(
                        model: model,
                        isExpanded: $isSearchExpanded
                    )
                }
            }

            ToolbarItem(placement: .primaryAction) {
                NotificationsToolbarButton(model: model)
            }

            ToolbarItem(placement: .primaryAction) {
                UpdateBadgeToolbarButton(updateModel: updateModel)
            }
        }
        .onChange(of: model.navigationMode) { _, _ in
            isSearchExpanded = false
        }
        .alert(
            "Work Error",
            isPresented: Binding(
                get: { model.workErrorMessage != nil },
                set: { newValue in
                    if !newValue {
                        model.workErrorMessage = nil
                    }
                }
            ),
            actions: {
                Button("OK", role: .cancel) {}
            },
            message: {
                Text(model.workErrorMessage ?? "")
            }
        )
        .sheet(item: $model.pendingWorkCreateRequest) { request in
            WorkCreateSheet(
                request: request,
                productDefaultRepoURL: productDefaultRepoURL(for: request),
                knownRepos: knownRepos(for: request),
                onCancel: { model.dismissWorkCreateRequest() },
                onCreate: { name, description, repoRemoteURL, goal, setAsDefault in
                    model.submitWorkCreateRequest(
                        request,
                        name: name,
                        description: description,
                        repoRemoteURL: repoRemoteURL,
                        goal: goal,
                        setAsProductDefault: setAsDefault
                    )
                }
            )
        }
        .sheet(item: $model.pendingWorkEditRequest) { request in
            WorkEditSheet(
                request: request,
                onCancel: { model.dismissWorkEditRequest() },
                onSave: { name, description, status, repoRemoteURL, goal, priority, prURL, workerBranchPrefix, docsRepo in
                    model.submitWorkEditRequest(
                        request,
                        name: name,
                        description: description,
                        status: status,
                        repoRemoteURL: repoRemoteURL,
                        goal: goal,
                        priority: priority,
                        prURL: prURL,
                        workerBranchPrefix: workerBranchPrefix,
                        docsRepo: docsRepo
                    )
                },
                onSetTracker: { kind, org, repo, projectNumber, reverseClose in
                    if case .product(let product) = request.item {
                        model.setProductExternalTracker(
                            productId: product.id,
                            kind: kind,
                            org: org,
                            repo: repo,
                            projectNumber: projectNumber,
                            reverseClose: reverseClose
                        )
                    }
                },
                onUnsetTracker: {
                    if case .product(let product) = request.item {
                        model.unsetProductExternalTracker(productId: product.id)
                    }
                }
            )
            // Re-inject the model so the nested GitHubAccountSection (inside
            // ExternalTrackerSection) can read it via @EnvironmentObject;
            // sheet content does not always inherit the presenter's
            // environment objects.
            .environmentObject(model)
        }
        .sheet(isPresented: Binding(
            get: { updateModel.showUpdateSheet },
            set: { updateModel.showUpdateSheet = $0 }
        )) {
            UpdateResultSheet()
                .environmentObject(updateModel)
        }
        .overlay(alignment: .topTrailing) {
            if let feedback = updateModel.manualCheckFeedback {
                UpdateStatusToast(feedback: feedback)
                    .padding(.top, 52)
                    .padding(.trailing, 16)
                    .transition(.opacity.combined(with: .offset(y: -8)))
            }
        }
        .animation(.easeInOut(duration: 0.2), value: updateModel.manualCheckFeedback)
    }

    private var sidebar: some View {
        workSidebar
            .navigationSplitViewColumnWidth(min: 220, ideal: 280, max: 360)
            .overlay(alignment: .trailing) {
                // Cursor feedback only; native NSSplitView splitter handles drag.
                Color.clear
                    .frame(width: 6)
                    .pointerStyle(.frameResize(position: .trailing))
            }
    }

    /// Look up the parent product's default repo URL for a pending
    /// create request, used by `WorkCreateSheet` to pick the repo
    /// field's render mode (design Q10). Product / project requests
    /// have no parent-product-default context that's relevant to the
    /// repo field, so we return `nil` there.
    private func productDefaultRepoURL(for request: WorkCreateRequest) -> String? {
        switch request.kind {
        case .product, .project:
            return nil
        case .task(let productID, _), .chore(let productID):
            return model.productDefaultRepoURL(productID)
        }
    }

    /// Empirical known-repo set for the parent product of a pending
    /// create request. Empty for product / project requests — neither
    /// form surfaces a recent-repos picker.
    private func knownRepos(for request: WorkCreateRequest) -> [String] {
        switch request.kind {
        case .product, .project:
            return []
        case .task(let productID, _), .chore(let productID):
            return model.knownReposForProduct(productID)
        }
    }

    private var detail: some View {
        workDetail
            .background(Color(nsColor: .windowBackgroundColor))
    }

    private var agentsView: some View {
        // Agents is the only top-level mode that isn't a NavigationSplitView,
        // so its content frame stops at the safe-area inset below the title
        // bar. The Work mode's sidebar uses the sidebar material that bleeds
        // up into that title bar region; with `.opacity(0)` the SwiftUI layer
        // is hidden but the title-bar strip directly above the sidebar
        // column is still visible chrome. Painting the agents background
        // through the safe area covers that strip so the Work sidebar's
        // top sliver doesn't show through when Agents is active.
        #if canImport(GhosttyKit)
        WorkersDetailView(
            workspace: workersWorkspace,
            liveStates: model.liveWorkerStates,
            liveStatusModel: model
        )
            .background(Color(nsColor: .windowBackgroundColor).ignoresSafeArea())
        #else
        VStack(alignment: .leading, spacing: 12) {
            Text("Agents mode requires GhosttyKit.")
                .font(.title3.weight(.semibold))
            Text("Run `tools/boss/app-macos/scripts/bootstrap-ghosttykit.sh` and rebuild with SwiftPM.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Spacer()
        }
        .padding(20)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(Color(nsColor: .windowBackgroundColor).ignoresSafeArea())
        #endif
    }

    private var workSidebar: some View {
        List {
            if !model.activeProducts.isEmpty {
                Section {
                    ZStack(alignment: .trailing) {
                        SidebarProductPicker(
                            selection: workProductSelection,
                            products: model.activeProducts
                        )
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.trailing, 28)

                        Button {
                            model.presentEditSelectedProduct()
                        } label: {
                            Image(systemName: "square.and.pencil")
                                .frame(width: 16, height: 16)
                        }
                        .buttonStyle(.borderless)
                        .padding(.trailing, -2)
                        .help("Edit Product")
                        .disabled(model.selectedProduct == nil || !model.isConnected)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .listRowInsets(EdgeInsets(top: 3, leading: -8, bottom: 3, trailing: 0))

                    let attentionItems = model.selectedProductOpenAttentionItems
                    if !attentionItems.isEmpty {
                        ExternalTrackerSyncBanner(items: attentionItems)
                            .listRowInsets(EdgeInsets(top: 0, leading: 0, bottom: 0, trailing: 0))
                            .listRowBackground(Color.clear)
                    }
                } header: {
                    workSidebarSectionTitle("Product")
                }
            }

            if model.selectedProduct != nil {
                Section {
                    WorkSidebarFilterRow(
                        title: "All Projects",
                        subtitle: nil,
                        systemImage: "square.stack.3d.up",
                        isSelected: !model.hasProjectFilters,
                        trailing: nil,
                        showsCheckbox: false
                    )
                    .listRowInsets(EdgeInsets(top: 3, leading: 8, bottom: 3, trailing: 8))
                    .listRowBackground(Color.clear)
                    .contentShape(Rectangle())
                    .onTapGesture {
                        model.clearProjectFilters()
                    }

                    let choresUnblocked = model.unblockedChoreCount
                    let choresBlocked = model.blockedChoreCount
                    WorkSidebarFilterRow(
                        title: "No Project (Chores)",
                        subtitle: nil,
                        systemImage: "tray",
                        isSelected: model.filterToChoresOnly,
                        trailing: nil,
                        showsCheckbox: false,
                        unblockedCount: choresUnblocked > 0 ? choresUnblocked : nil,
                        blockedCount: choresBlocked > 0 ? choresBlocked : nil
                    )
                    .listRowInsets(EdgeInsets(top: 3, leading: 8, bottom: 3, trailing: 8))
                    .listRowBackground(Color.clear)
                    .contentShape(Rectangle())
                    .onTapGesture {
                        model.setFilterToChoresOnly(!model.filterToChoresOnly)
                    }

                    ForEach(model.projectsForSelectedProduct) { project in
                        let isOn = model.selectedProjectFilterIDs.contains(project.id)
                        let isArchived = project.status == "archived"
                        let unblocked = model.unblockedTaskCount(forProjectID: project.id)
                        let blocked = model.blockedTaskCount(forProjectID: project.id)
                        let docPresentation = ProjectDesignDocAffordancePresentation.from(
                            state: model.designDocStateByProjectID[project.id] ?? .notSet
                        )
                        WorkSidebarFilterRow(
                            title: project.name,
                            subtitle: project.shortID.map { "P" + String($0) },
                            systemImage: isArchived ? "archivebox" : "folder",
                            isSelected: isOn,
                            trailing: nil,
                            showsCheckbox: true,
                            isCheckboxOn: isOn,
                            dimmed: isArchived,
                            unblockedCount: unblocked > 0 ? unblocked : nil,
                            blockedCount: blocked > 0 ? blocked : nil,
                            designDocPresentation: docPresentation,
                            onOpenDesignDoc: docPresentation != nil ? { model.openProjectDesignDoc(project) } : nil
                        )
                        .listRowInsets(EdgeInsets(top: 3, leading: 8, bottom: 3, trailing: 8))
                        .listRowBackground(Color.clear)
                        .contentShape(Rectangle())
                        .onTapGesture {
                            model.toggleProjectFilter(project.id)
                        }
                        .contextMenu {
                            if !isArchived {
                                Button("Archive") {
                                    model.archiveProject(id: project.id)
                                }
                            }
                        }
                    }
                } header: {
                    workSidebarSectionTitle("Projects")
                }

                Section {
                    Toggle("Include chores", isOn: Binding(
                        get: { model.includeChores },
                        set: { model.setIncludeChores($0) }
                    ))
                    .listRowInsets(EdgeInsets(top: 4, leading: 8, bottom: 4, trailing: 8))
                    .listRowBackground(Color.clear)

                    Toggle("Show blocked only", isOn: Binding(
                        get: { model.showBlockedOnly },
                        set: { model.setShowBlockedOnly($0) }
                    ))
                    .listRowInsets(EdgeInsets(top: 4, leading: 8, bottom: 4, trailing: 8))
                    .listRowBackground(Color.clear)

                    Toggle("Show archived projects", isOn: Binding(
                        get: { model.showArchivedProjects },
                        set: { model.setShowArchivedProjects($0) }
                    ))
                    .listRowInsets(EdgeInsets(top: 4, leading: 8, bottom: 4, trailing: 8))
                    .listRowBackground(Color.clear)
                } header: {
                    workSidebarSectionTitle("Options")
                }
            }
        }
        .listStyle(.sidebar)
        .safeAreaInset(edge: .bottom) {
            HStack {
                Button {
                    model.refreshWork()
                } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
                .buttonStyle(.borderless)
                Spacer()
                if !model.isConnected {
                    Label("Disconnected", systemImage: "circle.fill")
                        .foregroundStyle(.red)
                        .font(.caption)
                }
            }
            .padding(.horizontal, 12)
            .padding(.top, 8)
        }
    }

    private var workProductSelection: Binding<String?> {
        Binding(
            get: {
                model.selectedProduct?.id ?? model.activeProducts.first?.id
            },
            set: { newValue in
                guard let productID = newValue else { return }
                model.selectWorkProduct(productID)
            }
        )
    }

    private var workDetail: some View {
        HStack(spacing: 0) {
            workMainContent
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            workBossPanel
        }
    }

    private var workMainContent: some View {
        Group {
            if model.activeProducts.isEmpty {
                VStack(alignment: .leading, spacing: 10) {
                    Text("No work items yet")
                        .font(.title2.weight(.semibold))
                    Text("Create a product to start organizing projects, tasks, and chores.")
                        .foregroundStyle(.secondary)
                    Button("New Product") {
                        model.presentCreateProduct()
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .center)
                .padding(24)
            } else if model.selectedProduct != nil {
                VStack(spacing: 0) {
                    if let query = model.activeWorkSearchQuery {
                        WorkFilterBanner(query: query) {
                            model.workSearchText = ""
                            isSearchExpanded = false
                        }
                    }
                    workBoard()
                }
            } else {
                VStack(alignment: .leading, spacing: 10) {
                    Text("Select a product")
                        .font(.title3.weight(.semibold))
                    Text("Choose a product from the sidebar to open its board.")
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .center)
                .padding(24)
            }
        }
    }

    private var workBossPanel: some View {
        let isCollapsed = model.isBossPanelCollapsed
        let expandedWidth = model.bossPanelWidth

        return VStack(spacing: 0) {
            bossAgentHeader(isCollapsed: isCollapsed)

            ZStack(alignment: .leading) {
                // The boss terminal is always mounted, even while the
                // panel is collapsed. Two things would otherwise reset
                // the boss claude session:
                //
                //   1. A structural `if`/`else` that excludes
                //      BossPaneTerminalView in the collapsed branch
                //      deinits GhosttyTerminalHostView; its deinit
                //      calls ghostty_surface_free, killing the PTY
                //      child and so the boss claude process. Same
                //      failure mode the Agents↔Work toggle avoids in
                //      `body` above.
                //   2. Shrinking the surface to the 88pt collapsed
                //      strip width would SIGWINCH claude to ~10
                //      columns and reflow its TUI; the session
                //      survives but the visible buffer comes back
                //      mangled. Pinning the terminal's frame to the
                //      expanded width and clipping the outer panel
                //      keeps the surface size stable across collapse.
                #if canImport(GhosttyKit)
                BossPaneTerminalView(boss: bossPane)
                    .frame(width: expandedWidth)
                    .frame(maxHeight: .infinity)
                    .opacity(isCollapsed ? 0 : 1)
                    .allowsHitTesting(!isCollapsed)
                #else
                VStack(alignment: .leading, spacing: 8) {
                    Text("Boss pane requires GhosttyKit.")
                        .font(.callout.weight(.medium))
                    Text("Run `tools/boss/app-macos/scripts/bootstrap-ghosttykit.sh` and rebuild with SwiftPM.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    Spacer()
                }
                .padding(14)
                .frame(width: expandedWidth)
                .frame(maxHeight: .infinity, alignment: .topLeading)
                .opacity(isCollapsed ? 0 : 1)
                .allowsHitTesting(!isCollapsed)
                #endif

                if isCollapsed {
                    VStack {
                        Spacer(minLength: 0)
                        Text("Picard")
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(.secondary)
                            .rotationEffect(.degrees(-90))
                        Spacer(minLength: 0)
                    }
                    .frame(width: workBossPanelCollapsedWidth)
                    .frame(maxHeight: .infinity)
                }
            }
            .frame(maxHeight: .infinity)
            .clipped()
        }
        .frame(width: isCollapsed ? workBossPanelCollapsedWidth : expandedWidth)
        .frame(maxHeight: .infinity)
        .background(Color(nsColor: .windowBackgroundColor))
        .overlay(alignment: .leading) {
            if !isCollapsed {
                ResizeDivider(
                    currentWidth: model.bossPanelWidth,
                    minWidth: workBossPanelMinWidth,
                    maxWidth: workBossPanelMaxWidth,
                    onWidthChanged: { newWidth in
                        model.setBossPanelWidth(newWidth)
                    }
                )
                // Constrain the overlay to a narrow grab strip at
                // the leading edge of the Boss pane. Without this,
                // SwiftUI's overlay fills the whole pane and the
                // divider's tracking area covers everything: cursor
                // stays resize-left-right everywhere and clicks
                // intercept the libghostty surface so the Boss pane
                // never gains keyboard focus.
                //
                // The strip can't extend left of the Boss pane's
                // bounds — those clicks would land on the workMain
                // sibling instead of bubbling down to this overlay
                // (NSView hit testing is bounded by parent bounds).
                // 12pt wide on the Boss-pane side gives a much
                // easier-to-grip target than 6pt while still being
                // a small fraction of the panel.
                .frame(width: workBossPanelDividerHitWidth)
            } else {
                Rectangle()
                    .fill(Color(nsColor: .separatorColor))
                    .frame(width: 1)
            }
        }
        .animation(.snappy(duration: 0.18), value: model.isBossPanelCollapsed)
    }

    @ViewBuilder
    private func bossAgentHeader(isCollapsed: Bool) -> some View {
        HStack(alignment: .center, spacing: 10) {
            if let portrait = TrekIconAssets.image(.picard, size: .small) {
                Image(nsImage: portrait)
                    .resizable()
                    .interpolation(.high)
                    .aspectRatio(contentMode: .fit)
                    .frame(width: 22, height: 28)
                    .clipShape(RoundedRectangle(cornerRadius: 3, style: .continuous))
            } else {
                Image(systemName: "person.crop.circle.badge.checkmark")
                    .foregroundStyle(Color.accentColor)
                    .font(.system(size: 13, weight: .semibold))
                    .frame(width: 22, height: 28)
            }

            if !isCollapsed {
                Text("Picard")
                    .font(.subheadline.weight(.semibold))
                    .foregroundStyle(.primary)
                    .lineLimit(1)

                Spacer(minLength: 8)
            } else {
                Spacer(minLength: 0)
            }

            Button {
                model.toggleBossPanelCollapsed()
            } label: {
                Image(systemName: "sidebar.right")
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(.secondary)
                    .frame(width: 22, height: 22)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help(isCollapsed ? "Expand Boss panel" : "Collapse Boss panel")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 9)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.regularMaterial)
        .overlay(alignment: .bottom) {
            Rectangle()
                .fill(Color(nsColor: .separatorColor).opacity(0.6))
                .frame(height: 0.5)
        }
    }

    private func workBoard() -> some View {
        GeometryReader { geometry in
            let columnWidth: CGFloat = {
                if geometry.size.width >= workBoardUltraWideThreshold {
                    return workBoardColumnWidthMax
                } else if geometry.size.width >= workBoardWideThreshold {
                    return workBoardColumnWidthWide
                } else {
                    return workBoardColumnWidth
                }
            }()
            ScrollView(.horizontal) {
                HStack(alignment: .top, spacing: workBoardColumnSpacing) {
                    ForEach(WorkBoardColumnKey.allCases) { column in
                        workColumn(column, width: columnWidth)
                    }
                }
                .padding(.horizontal, workBoardHorizontalPadding)
                .padding(.top, workBoardHorizontalPadding)
                .frame(maxHeight: .infinity, alignment: .top)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    private func workColumn(_ column: WorkBoardColumnKey, width: CGFloat = workBoardColumnWidth) -> some View {
        let sections = model.workSections(in: column)
        let itemCount = sections.reduce(0) { $0 + $1.items.count }

        return VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text(column.title)
                    .font(.headline)
                Spacer()
                Text("\(itemCount)")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 4)
                    .background(Color(nsColor: .quaternaryLabelColor).opacity(0.12))
                    .clipShape(Capsule())
            }

            Divider()

            if itemCount == 0 {
                Text("No items")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, minHeight: 80, alignment: .topLeading)
                Spacer(minLength: 0)
            } else {
                ScrollViewReader { proxy in
                    ScrollView(.vertical) {
                        VStack(alignment: .leading, spacing: 12) {
                            ForEach(sections) { section in
                                workSectionView(section, column: column)
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .topLeading)
                    }
                    .frame(maxHeight: .infinity)
                    .onChange(of: model.revealScrollTarget) { _, target in
                        guard let target else { return }
                        let columnIDs = sections.flatMap { $0.items.map(\.id) }
                        guard columnIDs.contains(target) else { return }
                        withAnimation { proxy.scrollTo(target, anchor: .center) }
                    }
                }
            }
        }
        .padding(14)
        .frame(width: width, alignment: .topLeading)
        .frame(maxHeight: .infinity, alignment: .topLeading)
        .background(Color(nsColor: .controlBackgroundColor))
        .clipShape(RoundedRectangle(cornerRadius: 16, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 16, style: .continuous)
                .stroke(Color(nsColor: .separatorColor), lineWidth: 1)
        )
        .dropDestination(for: String.self) { items, _ in
            guard let taskID = items.first else { return false }
            return model.attemptMoveTask(taskID, to: column)
        }
    }

    @ViewBuilder
    private func workSectionView(_ section: WorkBoardSection, column: WorkBoardColumnKey) -> some View {
        if section.isCollapsible {
            CollapsibleWorkBoardSection(
                sectionID: section.id,
                title: section.title,
                count: section.items.count,
                defaultExpanded: section.defaultExpanded,
                shortIDLabel: section.projectID
                    .flatMap { model.project(withID: $0)?.shortID }
                    .map { "P" + String($0) }
            ) {
                workSectionItems(section.items, column: column)
            }
        } else {
            VStack(alignment: .leading, spacing: 10) {
                if model.workBoardGrouping == .project {
                    HStack(alignment: .firstTextBaseline, spacing: 6) {
                        Text(section.title)
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(.secondary)
                        if let projectID = section.projectID,
                           let project = model.project(withID: projectID),
                           let id = project.shortID {
                            Text("P" + String(id))
                                .font(.system(.caption2, design: .monospaced))
                                .foregroundStyle(.secondary)
                        }
                        Spacer(minLength: 0)
                        if let projectID = section.projectID,
                           let project = model.project(withID: projectID) {
                            ProjectDesignDocAffordance(model: model, project: project)
                        }
                    }
                }
                workSectionItems(section.items, column: column)
            }
        }
    }

    @ViewBuilder
    private func workSectionItems(_ items: [WorkTask], column: WorkBoardColumnKey) -> some View {
        let selectedID = model.selectedTask?.id
        let highlightID = model.revealHighlightID
        let frontierIDs = model.depFrontierHighlightIDs
        let revisionIDs = model.revisionHighlightIDs
        VStack(alignment: .leading, spacing: 10) {
            ForEach(items) { task in
                let isSelected = selectedID == task.id
                let isRevealed = highlightID == task.id
                let isFrontierHighlighted = frontierIDs.contains(task.id) || revisionIDs.contains(task.id)
                WorkBoardCardItem(
                    task: task,
                    projectName: model.cardProjectBadge(for: task),
                    column: column,
                    runtime: column == .doing ? model.taskRuntime(for: task.id) : nil,
                    isSelected: isSelected,
                    isRevealed: isRevealed,
                    isFrontierHighlighted: isFrontierHighlighted,
                    model: model,
                    liveStates: model.liveWorkerStates
                )
                .id(task.id)
            }
        }
    }

    @ViewBuilder
    private func workSidebarSectionTitle(_ title: String) -> some View {
        Text(title)
            .font(.caption.weight(.semibold))
            .foregroundStyle(.secondary)
            .textCase(.uppercase)
    }
}

private struct CollapsibleWorkBoardSection<Content: View>: View {
    let sectionID: String
    let title: String
    let count: Int
    let defaultExpanded: Bool
    var shortIDLabel: String? = nil
    @ViewBuilder let content: () -> Content

    @State private var userToggled: Bool = false

    private var isExpanded: Bool {
        userToggled ? !defaultExpanded : defaultExpanded
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Button {
                userToggled.toggle()
            } label: {
                HStack(spacing: 6) {
                    Image(systemName: isExpanded ? "chevron.down" : "chevron.right")
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.secondary)
                        .frame(width: 10)
                    Text("\(title) (\(count))")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                    if let label = shortIDLabel {
                        Text(label)
                            .font(.system(.caption2, design: .monospaced))
                            .foregroundStyle(.secondary)
                    }
                    Spacer()
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)

            if isExpanded {
                content()
            }
        }
        .id(sectionID)
    }
}

private struct WorkSidebarFilterRow: View {
    let title: String
    let subtitle: String?
    let systemImage: String
    let isSelected: Bool
    let trailing: String?
    var showsCheckbox: Bool = false
    var isCheckboxOn: Bool = false
    /// Render the row in a muted style — used for archived projects so
    /// they're visibly distinct from active ones when the user opts in
    /// to seeing them.
    var dimmed: Bool = false
    /// When non-nil, a green `▶ N` chip is shown for unblocked (todo)
    /// task count. Suppressed when nil (no unblocked tasks).
    var unblockedCount: Int? = nil
    /// When non-nil, a red `⏸ N` chip is shown for dependency-blocked
    /// task count. Suppressed when nil (no dependency-blocked tasks).
    var blockedCount: Int? = nil
    /// When non-nil, shows a design-doc affordance link under the badges.
    /// Suppressed when nil (project has no design doc pointer set).
    var designDocPresentation: ProjectDesignDocAffordancePresentation? = nil
    /// Called when the user clicks the design-doc affordance. Required
    /// when `designDocPresentation` is non-nil.
    var onOpenDesignDoc: (() -> Void)? = nil

    private var hasExtraRow: Bool {
        (subtitle != nil && !subtitle!.isEmpty) || designDocPresentation != nil
    }

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            if showsCheckbox {
                Image(systemName: isCheckboxOn ? "checkmark.square.fill" : "square")
                    .foregroundStyle(isCheckboxOn ? Color.accentColor : .secondary)
                    .font(.system(size: 14, weight: .medium))
                    .frame(width: 15, alignment: .center)
                    .padding(.top, 2)
                    .opacity(dimmed && !isCheckboxOn ? 0.6 : 1.0)
            } else {
                Image(systemName: systemImage)
                    .foregroundStyle(isSelected ? .primary : .secondary)
                    .font(.system(size: 14, weight: .medium))
                    .frame(width: 15, alignment: .center)
                    .padding(.top, 2)
            }
            VStack(alignment: .leading, spacing: subtitle != nil && !subtitle!.isEmpty ? 2 : 0) {
                HStack(alignment: .top, spacing: 8) {
                    if dimmed {
                        Image(systemName: systemImage)
                            .foregroundStyle(.secondary)
                            .font(.system(size: 12, weight: .medium))
                            .padding(.top, 3)
                            .help("Archived")
                    }
                    Text(title)
                        .font(.body.weight(isSelected ? .semibold : .regular))
                        .foregroundStyle(dimmed ? .secondary : .primary)
                        .lineLimit(2)
                        .truncationMode(.tail)
                        .fixedSize(horizontal: false, vertical: true)
                        .layoutPriority(1)
                        .help(title)

                    Spacer(minLength: 6)

                    if let trailing, !trailing.isEmpty {
                        WorkStatusBadge(text: trailing, emphasized: isSelected)
                            .fixedSize(horizontal: true, vertical: false)
                            .layoutPriority(2)
                            .opacity(dimmed ? 0.65 : 1.0)
                    }
                    if let blockedCount {
                        ProjectTaskCountChip(count: blockedCount, kind: .blocked)
                            .fixedSize(horizontal: true, vertical: false)
                            .layoutPriority(2)
                            .opacity(dimmed ? 0.65 : 1.0)
                    }
                    if let unblockedCount {
                        ProjectTaskCountChip(count: unblockedCount, kind: .unblocked)
                            .fixedSize(horizontal: true, vertical: false)
                            .layoutPriority(2)
                            .opacity(dimmed ? 0.65 : 1.0)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)

                if (subtitle != nil && !subtitle!.isEmpty) || designDocPresentation != nil {
                    HStack(alignment: .center, spacing: 6) {
                        if let subtitle, !subtitle.isEmpty {
                            Text(subtitle)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .lineLimit(1)
                        }
                        Spacer(minLength: 0)
                        if let presentation = designDocPresentation, let openDoc = onOpenDesignDoc {
                            Button(action: openDoc) {
                                Image(systemName: presentation.systemImage)
                                    .font(.caption)
                                    .foregroundStyle(presentation.tint)
                                    .accessibilityLabel(presentation.accessibilityLabel)
                            }
                            .buttonStyle(.plain)
                            .help(presentation.tooltip)
                        }
                    }
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.leading, 8)
        .padding(.trailing, 4)
        .padding(.vertical, hasExtraRow ? 7 : 6)
        .contentShape(Rectangle())
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

private struct WorkProjectFilterToolbarButton: View {
    @ObservedObject var model: ChatViewModel
    @State private var isShowingPopover = false

    var body: some View {
        Button {
            isShowingPopover.toggle()
        } label: {
            Image(systemName: "square.stack.3d.up")
                .overlay(alignment: .topTrailing) {
                    if model.hasProjectFilters {
                        Circle()
                            .fill(Color.accentColor)
                            .frame(width: 6, height: 6)
                            .offset(x: 3, y: -3)
                    }
                }
        }
        .help("Project filter")
        .popover(isPresented: $isShowingPopover, arrowEdge: .bottom) {
            ProjectFilterPopover(model: model)
        }
    }
}

private struct WorkGroupToolbarMenu: View {
    @ObservedObject var model: ChatViewModel

    var body: some View {
        Menu {
            ForEach(WorkBoardGrouping.allCases) { grouping in
                Button {
                    model.setWorkBoardGrouping(grouping)
                } label: {
                    if model.workBoardGrouping == grouping {
                        Label(grouping.title, systemImage: "checkmark")
                    } else {
                        Text(grouping.title)
                    }
                }
            }
        } label: {
            Image(systemName: "rectangle.3.group")
        }
        .help("Group by")
    }
}

private struct WorkSearchToolbarItem: View {
    @ObservedObject var model: ChatViewModel
    @Binding var isExpanded: Bool

    var body: some View {
        if isExpanded {
            SearchTextField(
                text: $model.workSearchText,
                onEscape: {
                    isExpanded = false
                    model.workSearchText = ""
                },
                onFocusLost: {
                    isExpanded = false
                }
            )
            .frame(width: 160)
        } else {
            Button {
                isExpanded = true
            } label: {
                Image(systemName: "magnifyingglass")
            }
            .help("Search (⌘F)")
            .keyboardShortcut("f", modifiers: .command)
        }
    }
}

private struct SearchTextField: View {
    @Binding var text: String
    var onEscape: () -> Void
    var onFocusLost: () -> Void
    @FocusState private var isFocused: Bool

    var body: some View {
        HStack(spacing: 4) {
            Image(systemName: "magnifyingglass")
                .foregroundStyle(.secondary)
                .font(.system(size: 11))
            TextField("Search", text: $text)
                .textFieldStyle(.plain)
                .focused($isFocused)
                .onKeyPress(.escape) {
                    onEscape()
                    return .handled
                }
        }
        .padding(.horizontal, 7)
        .padding(.vertical, 4)
        .background(.quaternary, in: Capsule())
        .onAppear { isFocused = true }
        .onChange(of: isFocused) { _, focused in
            if !focused { onFocusLost() }
        }
    }
}

private struct ProjectFilterPopover: View {
    @ObservedObject var model: ChatViewModel

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            let allSelected = !model.hasProjectFilters
            Button {
                model.clearProjectFilters()
            } label: {
                HStack(spacing: 8) {
                    Image(systemName: allSelected ? "checkmark.square.fill" : "square")
                        .foregroundStyle(allSelected ? Color.accentColor : .secondary)
                        .font(.system(size: 14, weight: .medium))
                        .frame(width: 15)
                    Text("All Projects")
                        .font(.body.weight(allSelected ? .semibold : .regular))
                    Spacer()
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)

            Divider()

            ForEach(model.projectsForSelectedProduct) { project in
                let isOn = model.selectedProjectFilterIDs.contains(project.id)
                Button {
                    model.toggleProjectFilter(project.id)
                } label: {
                    HStack(spacing: 8) {
                        Image(systemName: isOn ? "checkmark.square.fill" : "square")
                            .foregroundStyle(isOn ? Color.accentColor : .secondary)
                            .font(.system(size: 14, weight: .medium))
                            .frame(width: 15)
                        Text(project.name)
                            .font(.body.weight(isOn ? .semibold : .regular))
                            .lineLimit(1)
                            .truncationMode(.tail)
                        if let id = project.shortID {
                            Text("P" + String(id))
                                .font(.system(.caption2, design: .monospaced))
                                .foregroundStyle(.secondary)
                        }
                        Spacer()
                    }
                    .padding(.horizontal, 12)
                    .padding(.vertical, 8)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
        }
        .frame(minWidth: 200, maxWidth: 280)
        .padding(.vertical, 4)
    }
}

/// Compact warning banner shown in the sidebar below the product picker
/// when the external-tracker reconciler has unresolved attention items for
/// the selected product. Clicking the banner opens a popover with each
/// item's title and body. Disappears automatically once all items are
/// resolved (resolved_at set).
private struct ExternalTrackerSyncBanner: View {
    let items: [WorkAttentionItem]
    @State private var isPopoverPresented = false

    private var leadingPresentation: ExternalTrackerAttentionPresentation? {
        items.compactMap { ExternalTrackerAttentionPresentation.forItem($0) }.first
    }

    var body: some View {
        Button {
            isPopoverPresented.toggle()
        } label: {
            HStack(spacing: 6) {
                Image(systemName: leadingPresentation?.iconName ?? "exclamationmark.triangle")
                    .font(.caption)
                    .foregroundStyle(.orange)
                Text(items.count == 1
                    ? "Sync issue"
                    : "\(items.count) sync issues")
                    .font(.caption)
                    .foregroundStyle(.orange)
                Spacer()
                Image(systemName: "chevron.right")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
            .background(
                RoundedRectangle(cornerRadius: 6)
                    .fill(Color.orange.opacity(0.12))
                    .overlay(
                        RoundedRectangle(cornerRadius: 6)
                            .stroke(Color.orange.opacity(0.3), lineWidth: 1)
                    )
            )
        }
        .buttonStyle(.plain)
        .popover(isPresented: $isPopoverPresented, arrowEdge: .trailing) {
            ExternalTrackerSyncPopover(items: items)
        }
    }
}

private struct ExternalTrackerSyncPopover: View {
    let items: [WorkAttentionItem]

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("External Tracker Sync Issues")
                .font(.headline)
                .padding(.horizontal, 16)
                .padding(.top, 14)
                .padding(.bottom, 10)

            Divider()

            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    ForEach(items) { item in
                        if let presentation = ExternalTrackerAttentionPresentation.forItem(item) {
                            ExternalTrackerAttentionRow(presentation: presentation, item: item)
                        } else {
                            ExternalTrackerGenericAttentionRow(item: item)
                        }
                    }
                }
                .padding(16)
            }
            .frame(minWidth: 320, maxWidth: 400, minHeight: 80, maxHeight: 400)
        }
    }
}

private struct ExternalTrackerAttentionRow: View {
    let presentation: ExternalTrackerAttentionPresentation
    let item: WorkAttentionItem

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .top, spacing: 8) {
                Image(systemName: presentation.iconName)
                    .foregroundStyle(.orange)
                    .frame(width: 16)
                Text(item.title)
                    .font(.subheadline.weight(.medium))
                    .fixedSize(horizontal: false, vertical: true)
            }
            Text(item.bodyMarkdown)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.leading, 24)
        }
    }
}

private struct ExternalTrackerGenericAttentionRow: View {
    let item: WorkAttentionItem

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .top, spacing: 8) {
                Image(systemName: "exclamationmark.triangle")
                    .foregroundStyle(.orange)
                    .frame(width: 16)
                Text(item.title)
                    .font(.subheadline.weight(.medium))
                    .fixedSize(horizontal: false, vertical: true)
            }
            Text(item.bodyMarkdown)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.leading, 24)
        }
    }
}

struct SidebarProductPicker: View {
    @Binding var selection: String?
    let products: [WorkProduct]

    var body: some View {
        Picker("Product", selection: $selection) {
            ForEach(products) { product in
                Text(product.name).tag(product.id as String?)
            }
        }
        .labelsHidden()
        .pickerStyle(.menu)
    }
}

/// Wrapper for a single kanban card. Observes `LiveWorkerStateStore`
/// so live-state pushes invalidate the card without touching
/// `ContentView` or `ChatViewModel`. Doing-column cards re-resolve
/// their live state on every store publish; other columns ignore the
/// store entirely.
private struct WorkBoardCardItem: View {
    let task: WorkTask
    let projectName: String?
    let column: WorkBoardColumnKey
    let runtime: WorkTaskRuntime?
    let isSelected: Bool
    var isRevealed: Bool = false
    /// True when this card is part of the actionable prerequisite
    /// frontier for a currently-hovered Dependency badge. Adds an
    /// amber border overlay so the reader can see "what needs to happen
    /// next" without opening the popover.
    var isFrontierHighlighted: Bool = false
    @ObservedObject var model: ChatViewModel
    @ObservedObject var liveStates: LiveWorkerStateStore
    @Environment(\.openWindow) private var openWindow
    @State private var showingDeleteConfirmation = false

    var body: some View {
        let liveState: WorkerLiveState? = {
            guard column == .doing,
                  let executionID = runtime?.executionID
            else { return nil }
            return liveStates.byRunID[executionID]
        }()

        // A dispatch-pending card has status=todo+autostart=true; it
        // landed in Doing because the engine intends to run it but no
        // slot is free yet. We give it its own activity state and a
        // "waiting for a slot" subtitle distinct from an active worker.
        let isDispatchPending = task.status == "todo" && task.autostart

        // A conflict-resolution card is status=blocked+merge_conflict with
        // an active resolution attempt. It routes to Doing for the duration
        // of the worker run; we surface a distinct "resolving conflicts"
        // indicator rather than the generic agent-activity dot.
        let isResolvingConflicts = column == .doing
            && task.status == "blocked"
            && task.blockedReason == "merge_conflict"

        // A CI-remediation card is status=blocked+ci_failure with an active
        // remediation attempt. Symmetric to the merge-conflict path above.
        let isRemediatingCI = column == .doing
            && task.status == "blocked"
            && task.blockedReason == "ci_failure"

        let isAIReviewing = column == .doing && task.aiReviewing && task.status == "active"

        let activityState: AgentActivityState? = column == .doing
            ? .forDoingCard(
                runtime: runtime,
                liveState: liveState,
                isDispatchPending: isDispatchPending,
                isResolvingConflicts: isResolvingConflicts,
                isRemediatingCI: isRemediatingCI,
                isAIReviewing: isAIReviewing)
            : nil

        let liveStatusForCard: String? = {
            guard column == .doing else { return nil }
            if isDispatchPending { return "Waiting for a slot" }
            if isResolvingConflicts { return nil }
            if isRemediatingCI { return nil }
            if isAIReviewing { return nil }
            return liveState?.liveStatus
        }()

        let blockedBy: String? = {
            if task.status == "blocked" {
                return model.blockedByLabel(for: task)
            }
            if task.blockedReason == "dependency" {
                let rows = model.dependencyPrereqs(for: task.id)
                guard !rows.isEmpty else { return nil }
                return rows.map(\.title).joined(separator: ", ")
            }
            return nil
        }()

        let gatingPrereqs = model.gatingPrereqs(for: task.id)
        let isAutoBlocked = model.isAutoBlocked(task)
        let dragRefusal: String? = (model.dragRefusalNotice?.taskID == task.id)
            ? model.dragRefusalNotice?.message
            : nil
        let repoChip = model.repoChip(for: task)
        let designDocProject: WorkProject? = task.kind == "design"
            ? task.projectID.flatMap { model.project(withID: $0) }
            : nil
        let designDocState: ProjectDesignDocState? = designDocProject
            .map { model.designDocStateByProjectID[$0.id] ?? .notSet }
        let externalRefLink = ExternalRefLinkPresentation.forTask(task)
        let inReviewRevisions: [WorkTask] = column == .review
            ? model.inReviewRevisions(forParentTaskID: task.id)
            : column == .done
                ? model.doneRevisions(forParentTaskID: task.id)
                : []
        let parentShortID: Int? = task.kind == "revision"
            ? task.parentTaskId.flatMap { model.workTask(withID: $0)?.shortID }
            : nil

        VStack(alignment: .leading, spacing: 6) {
            Button {
                model.selectWorkCard(isSelected ? nil : task.id)
            } label: {
                WorkBoardCardView(
                    task: task,
                    projectName: projectName,
                    isSelected: isSelected,
                    activityState: activityState,
                    assignedSlotId: column == .doing ? liveState?.slotId : nil,
                    liveStatus: liveStatusForCard,
                    liveStatusActivity: isDispatchPending ? nil : (column == .doing ? liveState?.activity : nil),
                    liveStatusLastEventAt: isDispatchPending ? nil : (column == .doing ? liveState?.lastEventAt : nil),
                    blockedBy: blockedBy,
                    isAutoBlocked: isAutoBlocked,
                    gatingPrereqs: gatingPrereqs,
                    repoChip: repoChip,
                    showsConflictClearedBadge: model.showsConflictClearedBadge(forPR: task.prURL),
                    showsCIAutoFixedBadge: model.showsCIAutoFixedBadge(forPR: task.prURL),
                    ciFailureBadge: model.ciFailureBadge(forPR: task.prURL),
                    isResolvingConflicts: isResolvingConflicts,
                    isRemediatingCI: isRemediatingCI,
                    isFrontierHighlighted: isFrontierHighlighted,
                    designDocState: designDocState,
                    onOpenDesignDoc: designDocProject.map { proj in { model.openProjectDesignDoc(proj) } },
                    ciRequiredState: column == .review ? (task.ciRequiredState ?? "in_progress") : nil,
                    ciRequiredDetail: column == .review ? task.ciRequiredDetail : nil,
                    reviewRequiredState: column == .review ? task.reviewRequiredState : nil,
                    reviewRequiredDetail: column == .review ? task.reviewRequiredDetail : nil,
                    mergeQueueState: column == .review ? task.mergeQueueState : nil,
                    externalRefLink: externalRefLink,
                    ambiguousRepoNames: model.ambiguousVisibleRepoNames,
                    inReviewRevisions: inReviewRevisions,
                    parentShortID: parentShortID,
                    onDepBadgeHover: { hovering in
                        model.setDepBadgeHover(hovering ? task.id : nil)
                    },
                    onRevisionBadgeHover: { hovering in
                        model.setRevisionBadgeHover(hovering ? task.id : nil)
                    },
                    onOpenReviewTerminal: ((column == .review || column == .done) && task.prURL != nil && !(task.prURL?.isEmpty ?? true))
                        ? { model.openReviewTerminal(for: task) }
                        : nil,
                    onMergeWhenReady: (column == .review &&
                                       task.status == "in_review" &&
                                       task.prURL != nil &&
                                       !(task.prURL?.isEmpty ?? true) &&
                                       task.mergeQueueState != "queued")
                        ? { model.mergeWhenReady(for: task) }
                        : nil
                )
            }
            .buttonStyle(.plain)
            .overlay(
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .strokeBorder(
                        Color.accentColor.opacity(isRevealed ? 0.85 : 0),
                        lineWidth: 3
                    )
                    .animation(.easeInOut(duration: 0.25), value: isRevealed)
            )
            .overlay(
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .strokeBorder(
                        Color.green.opacity(isFrontierHighlighted ? 0.7 : 0),
                        lineWidth: 2
                    )
                    .animation(.easeInOut(duration: 0.15), value: isFrontierHighlighted)
            )
            .contextMenu {
                if let id = task.shortID {
                    Button("Copy ID") {
                        let pb = NSPasteboard.general
                        pb.clearContents()
                        pb.setString("T" + String(id), forType: .string)
                    }
                }
                Button("View transcripts…") {
                    openWindow(id: "transcript-viewer", value: TranscriptViewerRef(taskId: task.id))
                }
                Divider()
                Button("Delete", role: .destructive) {
                    showingDeleteConfirmation = true
                }
            }
            .popover(
                isPresented: Binding(
                    get: { isSelected },
                    set: { isPresented in
                        if !isPresented, isSelected {
                            model.selectWorkCard(nil)
                        }
                    }
                ),
                arrowEdge: .trailing
            ) {
                WorkCardPopoverView(model: model, task: task)
            }

            if let dragRefusal {
                WorkDragRefusalBanner(message: dragRefusal) {
                    model.clearDragRefusal()
                }
            }
        }
        .onAppear { logDocLinkState("appeared") }
        .onChange(of: task.prURL) { _, _ in logDocLinkState("prURL-changed") }
        .confirmationDialog(
            "Delete \"\(task.name)\"?",
            isPresented: $showingDeleteConfirmation,
            titleVisibility: .visible
        ) {
            Button("Delete", role: .destructive) {
                model.deleteWorkItem(id: task.id)
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This is a soft-delete and can be recovered with: boss task restore")
        }
    }

    // Emits a debug log entry capturing the full doc-link render state for
    // this card. Gated at .debug() so it is silent in normal builds; surface
    // via Console.app (filter subsystem "dev.spinyfin.bossmacapp", category
    // "kanban-doc-link") or the Xcode debug console.
    //
    // Captured fields:
    //   event    — what triggered the log ("appeared" or "prURL-changed")
    //   id       — work_item_id (T-number correlates with engine logs)
    //   kind     — task kind ("investigation", "design", …)
    //   column   — board column the card routes to ("review", "doing", …)
    //   prURL    — the exact pr_url value the app received from the engine
    //              ("<nil>" = field absent/null on the wire; "empty" = "")
    //   link     — whether PRURLLink will render ("shown" or "skipped")
    //   skipReason — when link == "skipped", why (nil_or_empty vs none)
    private func logDocLinkState(_ event: String) {
        let prURLDesc: String
        let linkShown: Bool
        let skipReason: String

        if let u = task.prURL {
            prURLDesc = u.isEmpty ? "empty" : u
            linkShown = !u.isEmpty
            skipReason = u.isEmpty ? "empty_string" : "none"
        } else {
            prURLDesc = "<nil>"
            linkShown = false
            skipReason = "nil"
        }

        kanbanDocLinkLog.debug(
            """
            \(event, privacy: .public) \
            id=\(task.id, privacy: .public) \
            kind=\(task.kind, privacy: .public) \
            column=\(column.rawValue, privacy: .public) \
            prURL=\(prURLDesc, privacy: .public) \
            link=\(linkShown ? "shown" : "skipped", privacy: .public) \
            skipReason=\(skipReason, privacy: .public)
            """
        )
    }
}

private struct WorkDragRefusalBanner: View {
    let message: String
    let onDismiss: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 6) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
                .font(.caption)
                .padding(.top, 1)
            Text(message)
                .font(.caption)
                .foregroundStyle(.primary)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 4)
            Button(action: onDismiss) {
                Image(systemName: "xmark.circle.fill")
                    .foregroundStyle(.secondary)
                    .font(.caption)
            }
            .buttonStyle(.plain)
            .help("Dismiss")
            .accessibilityLabel("Dismiss drag refusal")
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .background(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .fill(Color.orange.opacity(0.12))
                .overlay(
                    RoundedRectangle(cornerRadius: 8, style: .continuous)
                        .strokeBorder(Color.orange.opacity(0.4), lineWidth: 1)
                )
        )
        .accessibilityElement(children: .combine)
    }
}

/// Persistent banner shown across the top of the kanban whenever a search
/// filter is active. Non-matching cards are hidden while a search is in
/// effect, and without a standing indicator a stale query reads as an
/// empty or complete board — a card looks deleted when it is merely
/// filtered out (issue #1248). The banner states the view is filtered,
/// echoes the active query, and offers a one-click Clear affordance.
private struct WorkFilterBanner: View {
    let query: String
    let onClear: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "line.3.horizontal.decrease.circle.fill")
                .foregroundStyle(Color.accentColor)
                .font(.callout)
            (
                Text("Filtered view — showing matches for ")
                    .foregroundStyle(.secondary)
                    + Text("“\(query)”")
                    .foregroundStyle(.primary)
                    .fontWeight(.semibold)
            )
            .font(.callout)
            .lineLimit(1)
            .truncationMode(.middle)
            Spacer(minLength: 8)
            Button(action: onClear) {
                Label("Clear filter", systemImage: "xmark.circle.fill")
                    .font(.callout.weight(.medium))
            }
            .buttonStyle(.plain)
            .foregroundStyle(Color.accentColor)
            .help("Clear the search filter and show all cards")
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity)
        .background(Color.accentColor.opacity(0.12))
        .overlay(alignment: .bottom) {
            Rectangle()
                .fill(Color.accentColor.opacity(0.35))
                .frame(height: 1)
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel("Board is filtered by search query \(query). Activate to clear the filter.")
    }
}

/// A wrapping horizontal stack: lays subviews left-to-right and wraps to a
/// new line as soon as the next subview would overflow the proposed width.
///
/// Used for the kanban card's metadata/badge cluster. A plain `HStack` of
/// `.fixedSize` chips (effort, CI status, repo, work-item id, agent/action
/// chips) cannot compress and has no wrap behaviour, so a full badge set on
/// a card with a long title overflows past the lane's right edge and gets
/// clipped — the recurring regression in #1172. Flowing the cluster instead
/// constrains every chip to the lane width: the row grows downward rather
/// than running off the side.
struct FlowLayout: Layout {
    /// Horizontal gap between chips on the same line.
    var horizontalSpacing: CGFloat = 6
    /// Vertical gap between wrapped lines.
    var verticalSpacing: CGFloat = 3

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout Void) -> CGSize {
        let maxWidth = proposal.width ?? .infinity
        let rows = computeRows(maxWidth: maxWidth, subviews: subviews)
        let width = rows.map(\.width).max() ?? 0
        let height = rows.reduce(0) { $0 + $1.height }
            + verticalSpacing * CGFloat(max(0, rows.count - 1))
        return CGSize(width: min(width, maxWidth), height: height)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout Void) {
        let rows = computeRows(maxWidth: bounds.width, subviews: subviews)
        var y = bounds.minY
        for row in rows {
            var x = bounds.minX
            for index in row.indices {
                let size = subviews[index].sizeThatFits(.unspecified)
                subviews[index].place(
                    at: CGPoint(x: x, y: y),
                    anchor: .topLeading,
                    proposal: ProposedViewSize(size)
                )
                x += size.width + horizontalSpacing
            }
            y += row.height + verticalSpacing
        }
    }

    private struct Row {
        var indices: [Int] = []
        var width: CGFloat = 0
        var height: CGFloat = 0
    }

    private func computeRows(maxWidth: CGFloat, subviews: Subviews) -> [Row] {
        var rows: [Row] = []
        var current = Row()
        for index in subviews.indices {
            let size = subviews[index].sizeThatFits(.unspecified)
            let lead = current.indices.isEmpty ? 0 : horizontalSpacing
            // Wrap when the chip would overflow — but never strand a chip on an
            // empty line, so the first chip of a row always fits even if it is
            // itself wider than the lane (it will clip, but that is degenerate).
            if !current.indices.isEmpty && current.width + lead + size.width > maxWidth {
                rows.append(current)
                current = Row()
            }
            let gap = current.indices.isEmpty ? 0 : horizontalSpacing
            current.width += gap + size.width
            current.height = max(current.height, size.height)
            current.indices.append(index)
        }
        if !current.indices.isEmpty {
            rows.append(current)
        }
        return rows
    }
}

struct WorkBoardCardView: View {
    let task: WorkTask
    let projectName: String?
    let isSelected: Bool
    let activityState: AgentActivityState?
    /// Slot id of the worker currently bound to this card, when the
    /// card lives in the Doing lane. Drives the small crew portrait
    /// in the title row so a glance at the board tells you which
    /// crew member is on which task.
    let assignedSlotId: Int?
    /// Free-text one-sentence "what is the worker doing right now"
    /// fed by the engine's live-status summarizer. Rendered as a
    /// subtitle row between the title row and the footer when the
    /// card is in the Doing lane and the string is non-empty.
    /// `nil` collapses the row entirely so idle/blank states don't
    /// leave awkward whitespace.
    var liveStatus: String? = nil
    /// Activity of the worker behind `liveStatus`. `WaitingForInput`
    /// now surfaces a `WorkerWaitingIndicator` icon next to the
    /// subtitle (rather than tinting the text accent-blue, which was
    /// ambiguous and an accessibility problem); `Errored` reads in
    /// red, `Idle` dims further than `.secondary`. The default `nil`
    /// is treated as the plain `.secondary` colour.
    var liveStatusActivity: WorkerActivity? = nil
    /// ISO-8601 `last_event_at` of the worker behind `liveStatus`,
    /// passed straight through from `LiveWorkerState`. Feeds the
    /// "No response for …" elapsed time in the waiting indicator's
    /// tooltip. `nil` when there is no live worker or no event yet.
    var liveStatusLastEventAt: String? = nil
    /// Comma-joined names of the prereqs currently gating this card.
    /// Non-nil only on `blocked` rows — the kanban surfaces these in
    /// the Backlog column with a lock + "Blocked by …" subtitle so the
    /// reader can tell at a glance which Backlog items are gated and
    /// by what.
    let blockedBy: String?
    /// True when the row is engine-blocked (auto-block) rather than a
    /// human choice. Drives the chain badge in the footer per design
    /// Q7 — manual blocks already get the lane and would double up.
    var isAutoBlocked: Bool = false
    /// Resolved prereq rows used by the chain badge's hover tooltip.
    /// Empty for cards that aren't gated; populated regardless of
    /// `isAutoBlocked` because the popover Dependencies subsection
    /// reuses this list to render hyperlinks.
    var gatingPrereqs: [WorkDependencyRow] = []
    /// Per-card repo chip presentation, populated only when the
    /// kanban is in multi-repo mode (any card override or mixed
    /// resolved URLs across the visible board). `nil` in single-repo
    /// mode, where the chip lives on the product header instead — see
    /// `WorkBoardRepoMode` for the mode rule.
    var repoChip: RepoChipPresentation? = nil
    /// True when this card's PR was the target of a successful
    /// conflict-resolution attempt inside the freshness window
    /// (Phase 5 #15 of the merge-conflict design). Renders the
    /// `"🔧 conflict cleared"` chip in the footer; ages out after 24h
    /// via [[ChatViewModel.showsConflictClearedBadge(forPR:)]].
    var showsConflictClearedBadge: Bool = false
    /// True when this card's PR has a successful CI auto-fix inside
    /// the 24h freshness window. Renders the `"✅ ci auto-fixed"` chip
    /// per design Q11 / Phase 11 #37.
    var showsCIAutoFixedBadge: Bool = false
    /// In-flight / exhausted CI-failure chip for the PR, or `nil` when
    /// no CI auto-fix is currently tracked. Renders `🟧 ci failing
    /// (used/budget)` or `🛑 ci failing (exhausted)` per design Q11.
    var ciFailureBadge: CiFailureBadge? = nil
    /// True when this card is in the Doing column because a merge-
    /// resolution worker is actively running against it. Suppresses the
    /// blocked-row orange chrome and renders the `"resolving conflicts"`
    /// indicator instead so the user can tell at a glance what the
    /// active work is.
    var isResolvingConflicts: Bool = false
    /// True when this card is in the Doing column because a CI-remediation
    /// worker is actively running against it. Symmetric to
    /// [[isResolvingConflicts]]; suppresses orange chrome and renders the
    /// `"resolving CI failure"` badge instead.
    var isRemediatingCI: Bool = false
    /// True when this card is a prerequisite frontier card for a
    /// currently-hovered Dependency badge. Drives the green card background.
    var isFrontierHighlighted: Bool = false
    /// Resolved design-doc state for the parent project. Non-nil only
    /// for `kind=design` tasks whose parent project has populated
    /// `design_doc_*` columns. `nil` hides the affordance entirely.
    var designDocState: ProjectDesignDocState? = nil
    /// Invoked when the user taps the design-doc affordance. Only
    /// called when `designDocState` is non-nil and produces a
    /// non-nil `ProjectDesignDocAffordancePresentation`.
    var onOpenDesignDoc: (() -> Void)? = nil
    /// Aggregate required-CI state for the PR indicator. Mirrors
    /// `WorkTask.ciRequiredState`; supplied by the parent only when the
    /// card is in the Review lane and `task.prURL` is non-nil.
    var ciRequiredState: String? = nil
    /// JSON-encoded failing check detail for the CI tooltip.
    var ciRequiredDetail: String? = nil
    /// Required-review state for the review indicator. Mirrors
    /// `WorkTask.reviewRequiredState`; supplied by the parent under the
    /// same conditions as `ciRequiredState`.
    var reviewRequiredState: String? = nil
    /// JSON-encoded reviewer list for the review tooltip.
    var reviewRequiredDetail: String? = nil
    /// Merge-queue state for the merging indicator. `"queued"` when the PR
    /// is in GitHub's merge queue; `nil` otherwise. When set, replaces the
    /// CI indicator so the card clearly shows the PR is actively being shipped.
    var mergeQueueState: String? = nil
    /// Upstream-link affordance derived from `task.externalRef`. `nil`
    /// when the task has no external binding — the affordance is hidden
    /// entirely in that state. Bound refs show an accent-colored `↗ #N`
    /// link; stale refs (binding cleared upstream) show it dimmed with a
    /// strikethrough so the history is preserved but the staleness is
    /// communicated at a glance.
    var externalRefLink: ExternalRefLinkPresentation? = nil
    /// Repo names whose bare-`repo#n` PR label would be ambiguous on
    /// the current board — see
    /// [[ChatViewModel.ambiguousVisibleRepoNames]]. Threaded into
    /// `PRURLLink` so a card's PR link can drop the org prefix when
    /// its repo is unique among visible cards.
    var ambiguousRepoNames: Set<String> = []
    /// Revisions to display as rollup lines on this card's footer. Populated
    /// in the Review lane (in-review revisions) and the Done lane (done
    /// revisions). Empty for Backlog/Doing cards and parent tasks with no
    /// nested revisions. Ordered by `revisionSeq`.
    var inReviewRevisions: [WorkTask] = []
    /// Short ID of the parent task, used to render "revises T<n>" on
    /// revision cards in Backlog/Doing. `nil` for non-revision tasks
    /// and revision tasks whose parent can't be resolved.
    var parentShortID: Int? = nil
    /// Called with `true` when the pointer enters a Dependency badge
    /// (the text badge or the chain link icon); `false` on exit.
    /// `nil` when the card doesn't need to report badge hover (e.g.
    /// in the Designs viewer).
    var onDepBadgeHover: ((Bool) -> Void)? = nil
    /// Called with `true` when the pointer enters the "In revision" badge;
    /// `false` on exit. Same hover-highlight protocol as `onDepBadgeHover`.
    var onRevisionBadgeHover: ((Bool) -> Void)? = nil
    /// Invoked when the user taps the terminal icon on a Review-column
    /// card. `nil` hides the button — callers only pass a closure when
    /// `column == .review && task.prURL != nil`.
    var onOpenReviewTerminal: (() -> Void)? = nil
    /// Invoked after the user confirms the "Merge When Ready" button on a
    /// Review-column card. `nil` hides the button — callers only pass a
    /// closure when the card is in the Review lane, has a PR URL, and the
    /// PR is not already in the merge queue (`mergeQueueState != "queued"`).
    var onMergeWhenReady: (() -> Void)? = nil

    @State private var isHovered: Bool = false
    @State private var showMergeConfirmation: Bool = false

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if task.kind == "revision", let seq = task.revisionSeq {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    RevisionBadge(seq: seq)
                    if let origin = EngineRevisionOrigin(createdVia: task.createdVia) {
                        EngineRevisionBadge(origin: origin)
                    }
                    if let parentID = parentShortID {
                        Text("revises T" + String(parentID))
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    Spacer(minLength: 0)
                }
            }
            HStack(alignment: .top, spacing: 6) {
                if let activityState {
                    AgentActivityDot(state: activityState)
                        .padding(.top, 5)
                }
                if let slotId = assignedSlotId,
                   let character = TrekCharacter.forSlot(slotId),
                   let nsImage = TrekIconAssets.image(character, size: .small) {
                    Image(nsImage: nsImage)
                        .resizable()
                        .interpolation(.high)
                        .aspectRatio(contentMode: .fit)
                        .frame(width: 20, height: 26)
                        .clipShape(RoundedRectangle(cornerRadius: 3, style: .continuous))
                        .help("\(character.displayName) (slot \(slotId))")
                }
                VStack(alignment: .leading, spacing: 2) {
                    HStack(alignment: .firstTextBaseline, spacing: 4) {
                        if task.status == "blocked" && !isResolvingConflicts && !isRemediatingCI {
                            Image(systemName: "lock.fill")
                                .font(.caption)
                                .foregroundStyle(.orange)
                                .accessibilityLabel("Blocked")
                        }
                        Text(task.name)
                            .font(.body.weight(.medium))
                            .foregroundStyle(.primary)
                            .multilineTextAlignment(.leading)
                            // Revision descriptions can be multi-paragraph; cap
                            // the card body to 2 lines so the card stays compact.
                            // The full text is accessible via the detail popover.
                            .lineLimit(task.kind == "revision" ? 2 : nil)
                            .truncationMode(.tail)
                    }
                    if let blockedBy, !blockedBy.isEmpty {
                        let prefix = task.status == "blocked" ? "Blocked by" : "Waiting on:"
                        Text("\(prefix) \(blockedBy)")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(2)
                            .help("\(prefix) \(blockedBy)")
                    }
                }
                // Pin the title column to the remaining lane width so the
                // title text wraps within the card instead of overflowing past
                // the right edge on long, low-break-opportunity names (#1172).
                .frame(maxWidth: .infinity, alignment: .leading)
            }

            if let liveStatus, !liveStatus.isEmpty {
                HStack(alignment: .firstTextBaseline, spacing: 4) {
                    WorkerWaitingIndicator(
                        activity: liveStatusActivity,
                        lastEventAt: liveStatusLastEventAt
                    )
                    Text(liveStatus)
                        .font(.caption)
                        .foregroundStyle(liveStatusColor)
                        .lineLimit(2)
                        .truncationMode(.tail)
                        .help(liveStatus)
                        .accessibilityLabel("Live status: \(liveStatus)")
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }

            if hasFooterContent {
                // Wrap the whole metadata cluster so a full badge set — effort,
                // CI status, repo, work-item id, and the trailing action chips —
                // flows onto additional lines within the lane width instead of
                // overflowing past the card's right edge and clipping (#1172).
                FlowLayout(horizontalSpacing: 6, verticalSpacing: 4) {
                    let parsedPriority = WorkPriority.parse(task.priority)
                    if parsedPriority == .high {
                        PriorityChip(priority: parsedPriority)
                    }
                    if let effortLevel = task.effortLevel,
                       !effortLevel.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                        EffortChip(effortLevel: effortLevel)
                    }
                    if let projectName, !projectName.isEmpty {
                        WorkStatusBadge(text: projectName)
                    }
                    if task.aiReviewing && task.status == "active" {
                        ReviewingAIBadge()
                    }
                    if isResolvingConflicts {
                        ResolvingConflictsBadge()
                    } else if isRemediatingCI {
                        ResolvingCIFailureBadge()
                    } else if let blockedText = WorkBlockedBadge.badgeText(for: task) {
                        let isDependencyBadge = blockedText == WorkBlockedBadge.label(forReason: "dependency")
                        WorkStatusBadge(text: blockedText)
                            .onHover { hovering in
                                if isDependencyBadge {
                                    onDepBadgeHover?(hovering)
                                }
                            }
                    }
                    if isAutoBlocked {
                        Image(systemName: "link")
                            .font(.caption2.weight(.semibold))
                            .foregroundStyle(.orange)
                            .help(autoBlockTooltip)
                            .accessibilityLabel("Auto-blocked by dependencies")
                            .accessibilityValue(autoBlockTooltip)
                            .onHover { hovering in
                                onDepBadgeHover?(hovering)
                            }
                    }
                    if conflictClearedBadgeVisible {
                        ConflictClearedBadge()
                    }
                    if showsCIAutoFixedBadge && ciFailureBadge == nil {
                        CIAutoFixedBadge()
                    }
                    if let ciFailureBadge, !isRemediatingCI {
                        CIFailureChip(badge: ciFailureBadge)
                    }
                    if let repoChip {
                        RepoChipView(presentation: repoChip)
                    }
                    if task.sourceAutomationId != nil {
                        Image(systemName: "wand.and.stars")
                            .font(.caption2.weight(.semibold))
                            .foregroundStyle(.purple)
                            .help("Created by automation")
                            .accessibilityLabel("Created by automation")
                    }
                    if let extRef = externalRefLink {
                        ExternalRefLinkView(presentation: extRef)
                    }
                    if task.kind == "design",
                       let state = designDocState,
                       let presentation = ProjectDesignDocAffordancePresentation.from(state: state) {
                        Button {
                            onOpenDesignDoc?()
                        } label: {
                            Image(systemName: presentation.systemImage)
                                .font(.caption)
                                .foregroundStyle(presentation.tint)
                                .accessibilityLabel(presentation.accessibilityLabel)
                        }
                        .buttonStyle(.plain)
                        .help(presentation.tooltip)
                    }
                    if let openTerminal = onOpenReviewTerminal {
                        Button {
                            openTerminal()
                        } label: {
                            Image(systemName: "terminal")
                                .font(.caption)
                                .foregroundStyle(Color.secondary)
                                .accessibilityLabel("Open terminal on PR branch")
                        }
                        .buttonStyle(.plain)
                        .help("Open terminal on PR branch")
                    }
                    if onMergeWhenReady != nil {
                        Button {
                            showMergeConfirmation = true
                        } label: {
                            Image(systemName: "arrow.triangle.merge")
                                .font(.caption)
                                .foregroundStyle(Color.secondary)
                                .accessibilityLabel("Merge when ready")
                        }
                        .buttonStyle(.plain)
                        .help("Merge When Ready: enqueue this PR for merging once all required checks pass")
                        .confirmationDialog(
                            "Merge When Ready",
                            isPresented: $showMergeConfirmation,
                            titleVisibility: .visible
                        ) {
                            Button("Confirm Merge When Ready") {
                                onMergeWhenReady?()
                            }
                            Button("Cancel", role: .cancel) {}
                        } message: {
                            Text("This will queue the PR for merging once all required checks pass. This action cannot be undone.")
                        }
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }

            if let prURL = task.prURL, !prURL.isEmpty {
                HStack(alignment: .center, spacing: 6) {
                    if mergeQueueState == "queued" {
                        PrMergingIndicator()
                    } else if let ciState = ciRequiredState {
                        PrCiIndicator(state: ciState, detail: ciRequiredDetail)
                    }
                    PRURLLink(
                        urlString: prURL,
                        font: .caption,
                        ambiguousRepoNames: ambiguousRepoNames
                    )
                    if task.hasInProgressRevision {
                        PrInRevisionIndicator()
                            .onHover { hovering in
                                onRevisionBadgeHover?(hovering)
                            }
                    }
                    Spacer(minLength: 0)
                    if let id = task.shortID {
                        Text("T" + String(id))
                            .font(.system(.caption2, design: .monospaced))
                            .foregroundStyle(.secondary)
                            .accessibilityLabel("T" + String(id))
                            .lineLimit(1)
                            .fixedSize(horizontal: true, vertical: false)
                    }
                }
            }

            if task.prURL != nil, let reviewState = reviewRequiredState {
                HStack(spacing: 6) {
                    PrReviewIndicator(state: reviewState, detail: reviewRequiredDetail)
                    Spacer(minLength: 0)
                }
            }

            if task.kind == "revision", let prURL = task.revisionParentPrUrl, !prURL.isEmpty {
                HStack(alignment: .center, spacing: 6) {
                    PRURLLink(
                        urlString: prURL,
                        font: .caption,
                        ambiguousRepoNames: ambiguousRepoNames
                    )
                    Spacer(minLength: 0)
                }
            }

            if task.prURL == nil || task.prURL!.isEmpty, let id = task.shortID {
                HStack {
                    Spacer(minLength: 0)
                    Text("T" + String(id))
                        .font(.system(.caption2, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .accessibilityLabel("T" + String(id))
                        .lineLimit(1)
                        .fixedSize(horizontal: true, vertical: false)
                }
            }

            if !inReviewRevisions.isEmpty {
                Divider()
                    .padding(.vertical, 2)
                ForEach(inReviewRevisions) { revision in
                    RevisionRollupLine(revision: revision)
                }
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .fill(cardBackground)
                .brightness(isHovered && !isSelected ? 0.04 : 0)
                .overlay(
                    RoundedRectangle(cornerRadius: 12, style: .continuous)
                        .strokeBorder(borderColor, lineWidth: isSelected ? 2 : 1)
                )
        )
        .draggable(task.id)
        .onHover { hovering in
            withAnimation(.easeInOut(duration: 0.15)) {
                isHovered = hovering
            }
        }
    }

    /// The footer renders the priority chip on every card so a glance
    /// at the board immediately separates `[HIGH]` work from the rest
    /// without authors having to prefix names. The other footer
    /// elements (project tag, blocked tag) appear conditionally.
    private var hasFooterContent: Bool {
        true
    }

    /// True when the "conflict cleared" badge may render: the cleared flag
    /// is set AND no active "Merge Conflict" badge is showing. Enforces
    /// the T795 mutual-exclusion invariant — the two states must never
    /// co-render. Delegates to [[WorkBlockedBadge.conflictClearedVisible]].
    private var conflictClearedBadgeVisible: Bool {
        WorkBlockedBadge.conflictClearedVisible(
            forTask: task,
            cleared: showsConflictClearedBadge,
            isResolvingConflicts: isResolvingConflicts
        )
    }

    /// Tooltip body for the chain badge. Mirrors the CLI `show`
    /// output's prereq list so a hover tells the reader the same
    /// thing without opening the popover.
    var autoBlockTooltip: String {
        guard !gatingPrereqs.isEmpty else {
            return "Auto-blocked by dependencies"
        }
        let summary = gatingPrereqs
            .map { "\($0.title) (\($0.status.replacingOccurrences(of: "_", with: " ")))" }
            .joined(separator: ", ")
        return "Gated by: \(summary)"
    }

    /// Tint for the live-status subtitle row. Red for errored runs, a
    /// dimmer grey when the worker is idle, and the normal `.secondary`
    /// grey otherwise. The `waitingForInput` case is intentionally
    /// *not* tinted: it now carries its meaning via the explicit
    /// `WorkerWaitingIndicator` icon + tooltip instead of an ambiguous
    /// accent-blue subtitle (hue alone is an accessibility problem).
    private var liveStatusColor: Color {
        switch liveStatusActivity {
        case .errored:
            return .red
        case .idle:
            return Color(nsColor: .tertiaryLabelColor)
        default:
            return .secondary
        }
    }

    private var cardBackground: Color {
        if isSelected {
            return Color.accentColor.opacity(0.08)
        }
        if isFrontierHighlighted {
            return Color.green.opacity(0.07)
        }
        if !isResolvingConflicts && !isRemediatingCI && task.status == "blocked" {
            return Color.orange.opacity(0.08)
        }
        return Color(nsColor: .windowBackgroundColor)
    }

    private var borderColor: Color {
        if isSelected {
            return .accentColor
        }
        if !isResolvingConflicts && !isRemediatingCI && task.status == "blocked" {
            return .orange
        }
        return Color(nsColor: .separatorColor)
    }
}

/// The `⟳ R<n>` chip rendered on revision cards in Backlog/Doing.
/// Uses the accent color so the chip reads as an affordance rather than
/// metadata text, and clearly signals "this is a revision" at a glance.
private struct RevisionBadge: View {
    let seq: Int

    var body: some View {
        HStack(spacing: 3) {
            Text("⟳")
                .font(.caption.weight(.semibold))
            Text("R\(seq)")
                .font(.system(.caption, design: .monospaced).weight(.semibold))
        }
        .foregroundStyle(.white)
        .padding(.horizontal, 6)
        .padding(.vertical, 2)
        .background(Color.accentColor.opacity(0.75), in: Capsule())
        .accessibilityLabel("Revision \(seq)")
    }
}

/// Discriminates the engine-triggered origin of a revision task from its
/// `created_via` field. `nil` when the revision is operator- or comment-driven.
/// Design: `tools/boss/docs/designs/unify-pr-remediation-on-revisions.md` Q2.
private enum EngineRevisionOrigin {
    case mergeConflict
    case ciFix

    init?(createdVia: String) {
        if createdVia.hasPrefix("merge-conflict:") {
            self = .mergeConflict
        } else if createdVia.hasPrefix("ci-fix:") {
            self = .ciFix
        } else {
            return nil
        }
    }

    var label: String {
        switch self {
        case .mergeConflict: return "conflict fix"
        case .ciFix: return "CI fix"
        }
    }

    var helpText: String {
        switch self {
        case .mergeConflict: return "Engine-triggered revision: auto-generated to resolve a merge conflict."
        case .ciFix: return "Engine-triggered revision: auto-generated to fix a CI failure."
        }
    }

    var accessibilityLabel: String {
        switch self {
        case .mergeConflict: return "Engine-triggered conflict fix"
        case .ciFix: return "Engine-triggered CI fix"
        }
    }
}

/// Subtle chrome indicating an engine-triggered revision (merge-conflict or
/// CI-fix origin). Shown inline with [[RevisionBadge]] on revision cards
/// whose `created_via` matches one of the engine-trigger prefixes.
private struct EngineRevisionBadge: View {
    let origin: EngineRevisionOrigin

    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "gear")
                .font(.caption2)
            Text(origin.label)
                .font(.caption.weight(.medium))
        }
        .foregroundStyle(.secondary)
        .padding(.horizontal, 5)
        .padding(.vertical, 2)
        .background(Color.secondary.opacity(0.12), in: Capsule())
        .help(origin.helpText)
        .accessibilityLabel(origin.accessibilityLabel)
    }
}

/// One rollup line in a Review-lane parent card for an in-review revision.
/// Shows `⟳ R<n>  <description truncated>  ↗` linking to the parent PR.
private struct RevisionRollupLine: View {
    let revision: WorkTask

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            if let seq = revision.revisionSeq {
                Text("⟳ R\(seq)")
                    .font(.system(.caption2, design: .monospaced).weight(.semibold))
                    .foregroundStyle(Color.accentColor)
            }
            Text(revision.name)
                .font(.caption2)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.tail)
            Spacer(minLength: 0)
            if let prURL = revision.revisionParentPrUrl,
               let url = URL(string: prURL) {
                Link(destination: url) {
                    Image(systemName: "arrow.up.right")
                        .font(.caption2)
                        .foregroundStyle(Color.accentColor)
                }
                .buttonStyle(.plain)
                .help("Revision \(revision.revisionSeq.map { "R\($0)" } ?? ""): \(revision.name)")
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel({
            let seqLabel = revision.revisionSeq.map { "Revision \($0)" } ?? "Revision"
            return "\(seqLabel): \(revision.name)"
        }())
    }
}

/// Per-project "open the design doc" affordance for the kanban
/// project-section header. Icon variant is keyed off
/// [[ProjectDesignDocState]] (hidden / plain doc icon / warning glyph),
/// click handler is the engine-resolved open dispatch on
/// [[ChatViewModel.openProjectDesignDoc(_:)]]. The view stays empty
/// when no state has been resolved yet so cards don't flash a stale
/// affordance while the first `ResolveProjectDesignDoc` is in flight.
struct ProjectDesignDocAffordance: View {
    @ObservedObject var model: ChatViewModel
    let project: WorkProject

    var body: some View {
        if let presentation = ProjectDesignDocAffordancePresentation.from(
            state: model.designDocStateByProjectID[project.id] ?? .notSet
        ) {
            Button {
                model.openProjectDesignDoc(project)
            } label: {
                Image(systemName: presentation.systemImage)
                    .font(.caption)
                    .foregroundStyle(presentation.tint)
                    .accessibilityLabel(presentation.accessibilityLabel)
            }
            .buttonStyle(.plain)
            .help(presentation.tooltip)
        }
    }
}

/// Pure-data presentation chosen for a `ProjectDesignDocState`. Lives
/// outside the view so tests can assert "this state renders this
/// icon" without spinning up a SwiftUI host — the kanban view is a
/// thin reflection of these fields.
struct ProjectDesignDocAffordancePresentation: Equatable {
    let systemImage: String
    let tooltip: String
    let accessibilityLabel: String
    let kind: Kind

    enum Kind: Equatable {
        case resolved
        case broken
    }

    var tint: Color {
        switch kind {
        case .resolved:
            return .secondary
        case .broken:
            return .orange
        }
    }

    /// Map a `ProjectDesignDocState` to its kanban presentation. Returns
    /// `nil` for `.notSet` so the kanban hides the affordance entirely
    /// — the design doc spec (Q3) wants no icon when the pointer is
    /// unset, distinct from the warning glyph used for broken pointers.
    static func from(state: ProjectDesignDocState) -> ProjectDesignDocAffordancePresentation? {
        switch state {
        case .notSet:
            return nil
        case .resolved(let resolved, _, _, _):
            let repoBase = repoBasename(from: resolved.repoRemoteURL)
            let tooltip = "\(repoBase):\(resolved.path)"
            return ProjectDesignDocAffordancePresentation(
                systemImage: "doc.text",
                tooltip: tooltip,
                accessibilityLabel: "Open design doc",
                kind: .resolved
            )
        case .broken(let reason):
            return ProjectDesignDocAffordancePresentation(
                systemImage: "exclamationmark.triangle",
                tooltip: "Design doc pointer is broken: \(reason)",
                accessibilityLabel: "Design doc pointer is broken",
                kind: .broken
            )
        }
    }

    /// Pull the `owner/repo` slug out of a GitHub URL for the hover
    /// tooltip. Falls back to the raw URL when the path isn't
    /// recognisable so we never render an empty `:path`. Handles
    /// both `https://github.com/foo/bar.git` and SCP-style
    /// `git@github.com:foo/bar.git` — `URL(string:)` accepts the
    /// SCP form on macOS but treats `git@github.com` as the scheme
    /// and leaves `path` empty, so the scheme check below routes
    /// scheme-less inputs through the colon-split branch.
    static func repoSlug(from repoURL: String) -> String {
        repoBasename(from: repoURL)
    }

    private static func repoBasename(from repoURL: String) -> String {
        if let url = URL(string: repoURL), url.host != nil {
            let parts = url.path
                .split(separator: "/", omittingEmptySubsequences: true)
                .map(String.init)
            if parts.count >= 2 {
                let owner = parts[0]
                let repo = parts[1].hasSuffix(".git")
                    ? String(parts[1].dropLast(4))
                    : parts[1]
                return "\(owner)/\(repo)"
            }
        }
        if let scpRange = repoURL.range(of: ":") {
            let path = String(repoURL[scpRange.upperBound...])
            let trimmed = path.hasSuffix(".git") ? String(path.dropLast(4)) : path
            return trimmed
        }
        return repoURL
    }
}

private struct AgentActivityDot: View {
    let state: AgentActivityState

    var body: some View {
        Group {
            if case .dispatchPending = state {
                Image(systemName: "hourglass")
                    .font(.system(size: 9, weight: .medium))
                    .foregroundStyle(Color(nsColor: .tertiaryLabelColor))
                    .frame(width: 7, height: 7)
            } else {
                Circle()
                    .fill(fillColor)
                    .frame(width: 7, height: 7)
            }
        }
        .help(state.tooltip)
        .accessibilityLabel(state.tooltip)
    }

    private var fillColor: Color {
        switch state {
        case .active:
            return .green
        case .waiting:
            return .yellow
        case .errored:
            return .red
        case .none:
            return Color(nsColor: .tertiaryLabelColor)
        case .dispatchPending:
            return Color(nsColor: .tertiaryLabelColor)
        }
    }
}

struct WorkCardPopoverView: View {
    @ObservedObject var model: ChatViewModel
    let task: WorkTask

    @Environment(\.openWindow) private var openWindow

    /// Drives presentation of the Repo Change… picker sheet. Bound to
    /// the popover so the sheet inherits the popover's window context;
    /// closing the sheet returns focus to the popover the user came
    /// from rather than dropping back to the kanban underneath.
    @State private var presentingRepoPicker: Bool = false

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(alignment: .top, spacing: 12) {
                VStack(alignment: .leading, spacing: 6) {
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Text(task.name)
                            .font(.title3.weight(.semibold))
                        if let id = task.shortID {
                            Text("T" + String(id))
                                .font(.system(.caption, design: .monospaced))
                                .foregroundStyle(.secondary)
                                .accessibilityLabel("T" + String(id))
                        }
                    }
                    Text(task.kindLabel)
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                }
                Spacer(minLength: 12)
                Button("Edit") {
                    model.selectWorkCard(task.id)
                    model.presentEditSelectedWorkItem()
                }
            }

            if !task.description.isEmpty {
                descriptionSummary
            }

            VStack(alignment: .leading, spacing: 10) {
                if let projectName = model.projectName(for: task.projectID) {
                    metadataRow("Project", value: projectName)
                }
                metadataRow(
                    "Status",
                    value: task.status.replacingOccurrences(of: "_", with: " ").capitalized
                )
                if task.status == "blocked", let reason = task.blockedReason {
                    metadataRow(
                        "Blocked reason",
                        value: reason.replacingOccurrences(of: "_", with: " ").capitalized
                    )
                }
                priorityRow
                repoRow
                if let ordinal = task.ordinal, !task.isChore {
                    metadataRow("Phase", value: "\(ordinal)")
                }
                metadataPRRow(prURL: task.prURL)
                sourceChipRow
                if task.sourceAutomationId != nil {
                    automationRow
                }
            }

            WorkDependenciesSection(model: model, taskID: task.id)

            executionsSection

            VStack(alignment: .leading, spacing: 8) {
                Text("Move")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                HStack {
                    ForEach(WorkBoardColumnKey.allCases) { column in
                        Button(column.title) {
                            model.selectWorkCard(task.id)
                            model.moveTask(task.id, to: column)
                        }
                        .disabled(task.boardColumn == column && task.status != "blocked")
                    }
                }
            }

            HStack {
                if task.status == "active" || task.status == "blocked" {
                    Button(task.status == "blocked" ? "Unblock" : "Mark Blocked") {
                        model.selectWorkCard(task.id)
                        model.toggleBlocked(for: task.id)
                    }
                }
                if !task.isChore {
                    Button("Move Up") {
                        model.selectWorkCard(task.id)
                        model.moveSelectedTask(offset: -1)
                    }
                    Button("Move Down") {
                        model.selectWorkCard(task.id)
                        model.moveSelectedTask(offset: 1)
                    }
                }
                Spacer()
                Button("Delete", role: .destructive) {
                    model.selectWorkCard(task.id)
                    model.deleteSelectedWorkItem()
                }
            }
        }
        .padding(20)
        .frame(width: 360, alignment: .leading)
        .onAppear {
            model.loadExecutions(taskId: task.id)
        }
        .sheet(isPresented: $presentingRepoPicker) {
            RepoOverridePicker(
                presentation: model.repoOverridePresentation(for: task),
                recentURLs: model.recentRepoURLs(forProduct: task.productID),
                onCancel: { presentingRepoPicker = false },
                onSelect: { url in
                    model.setRepoOverride(for: task.id, to: url)
                    presentingRepoPicker = false
                },
                onClear: {
                    model.setRepoOverride(for: task.id, to: nil)
                    presentingRepoPicker = false
                }
            )
        }
    }

    /// "Repo:" row inside the popover. Mirrors the CLI `boss <kind>
    /// show` Repo line — resolved URL on the first line, provenance
    /// label below — and trails the row with a `Change…` button that
    /// opens the override picker. Matches the CLI's three-state
    /// vocabulary: override / inherited from product / none-can't-
    /// dispatch.
    @ViewBuilder
    private var repoRow: some View {
        let presentation = model.repoOverridePresentation(for: task)
        VStack(alignment: .leading, spacing: 2) {
            Text("Repo")
                .font(.caption)
                .foregroundStyle(.secondary)
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                VStack(alignment: .leading, spacing: 1) {
                    if let url = presentation.resolvedURL {
                        Text(url)
                            .font(.body)
                            .lineLimit(1)
                            .truncationMode(.middle)
                            .help(url)
                        Text(presentation.provenanceLabel)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    } else {
                        Text(presentation.provenanceLabel)
                            .font(.body)
                            .foregroundStyle(.secondary)
                    }
                }
                Spacer(minLength: 8)
                Button("Change…") {
                    presentingRepoPicker = true
                }
                .accessibilityIdentifier("work-card-repo-change")
            }
        }
        .accessibilityIdentifier("work-card-repo-row")
    }

    /// Truncated rendering of the task description so a long body
    /// can't push the trailing metadata (Project, Status, …) off
    /// screen. Caps the visible text to roughly the first six lines
    /// and offers a "Read full description" affordance when the body
    /// has more content or markdown structure worth seeing rendered.
    @ViewBuilder
    private var descriptionSummary: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(descriptionSummaryText)
                .font(.body)
                .lineLimit(6)
                .truncationMode(.tail)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)

            if shouldOfferFullDescription {
                Button {
                    openWindow(
                        id: "markdown-viewer",
                        value: MarkdownViewerContent(title: task.name, markdown: task.description)
                    )
                } label: {
                    Label("Read full description", systemImage: "doc.text.magnifyingglass")
                        .font(.callout)
                }
                .buttonStyle(.link)
                .accessibilityIdentifier("work-card-read-full-description")
            }
        }
    }

    /// Plain-text preview used in the popover body. We surface the
    /// first paragraph because longer descriptions are usually a
    /// markdown document (`# heading` lines, fenced code, bullet
    /// lists) — that content reads poorly as raw text and is better
    /// served by the full markdown viewer the affordance opens.
    private var descriptionSummaryText: String {
        let trimmed = task.description.trimmingCharacters(in: .whitespacesAndNewlines)
        let paragraphs = trimmed.components(separatedBy: "\n\n")
        let firstParagraph = paragraphs.first ?? trimmed
        return firstParagraph.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// True when the description has content the truncated preview
    /// can't show (additional paragraphs, more than ~6 lines, or
    /// markdown features like headings or fenced code that only
    /// render meaningfully in the viewer).
    private var shouldOfferFullDescription: Bool {
        let trimmed = task.description.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty { return false }
        if trimmed != descriptionSummaryText { return true }
        if trimmed.components(separatedBy: "\n").count > 6 { return true }
        if trimmed.count > 280 { return true }
        if trimmed.contains("```") { return true }
        if trimmed.contains("\n#") || trimmed.hasPrefix("#") { return true }
        return false
    }

    @ViewBuilder
    private var executionsSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("Executions")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer()
                Button("View transcripts…") {
                    openWindow(id: "transcript-viewer", value: TranscriptViewerRef(taskId: task.id))
                }
                .buttonStyle(.link)
                .font(.caption)
                .accessibilityIdentifier("work-card-view-transcripts")
            }
            if let executions = model.executionsByTaskID[task.id] {
                if executions.isEmpty {
                    Text("No executions yet.")
                        .font(.caption)
                        .foregroundStyle(.tertiary)
                } else {
                    VStack(alignment: .leading, spacing: 2) {
                        ForEach(executions) { exec in
                            Button {
                                openWindow(
                                    id: "transcript-viewer",
                                    value: TranscriptViewerRef(taskId: task.id, preselectExecutionId: exec.id)
                                )
                            } label: {
                                ExecutionRow(exec: exec)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                            }
                            .buttonStyle(.plain)
                        }
                    }
                }
            } else {
                ProgressView()
                    .controlSize(.small)
            }
        }
    }

    @ViewBuilder
    private func metadataRow(_ label: String, value: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(label)
                .font(.caption)
                .foregroundStyle(.secondary)
            Text(value)
                .font(.body)
        }
    }

    /// Priority row with an inline picker. Editing here fires a
    /// targeted update so authors can re-prioritise a card without
    /// going through the full edit sheet.
    @ViewBuilder
    private var priorityRow: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("Priority")
                .font(.caption)
                .foregroundStyle(.secondary)
            Picker(
                "",
                selection: Binding(
                    get: { WorkPriority.parse(task.priority) },
                    set: { newValue in
                        if newValue.rawValue != task.priority {
                            model.setPriority(for: task.id, to: newValue)
                        }
                    }
                )
            ) {
                ForEach(WorkPriority.allCases) { priority in
                    Text(priority.label).tag(priority)
                }
            }
            .labelsHidden()
            .pickerStyle(.menu)
            .fixedSize()
        }
    }

    /// Surface that filed this row, rendered as a small chip below the
    /// PR row. Visible on every card; the chip text is the raw
    /// `created_via` value (`cli`, `mac_app`, `engine_auto`, …) so a
    /// future undocumented source shows up verbatim rather than
    /// silently looking like one of the known values.
    @ViewBuilder
    private var sourceChipRow: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("Source")
                .font(.caption)
                .foregroundStyle(.secondary)
            Text(task.createdVia)
                .font(.caption)
                .padding(.horizontal, 8)
                .padding(.vertical, 2)
                .background(
                    Capsule().fill(Color.secondary.opacity(0.15))
                )
        }
    }

    private var automationRow: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("Automation")
                .font(.caption)
                .foregroundStyle(.secondary)
            HStack(spacing: 4) {
                Image(systemName: "wand.and.stars")
                    .font(.caption)
                    .foregroundStyle(.purple)
                Text("Created by automation")
                    .font(.caption)
            }
        }
    }

    @ViewBuilder
    private func metadataPRRow(prURL: String?) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("PR")
                .font(.caption)
                .foregroundStyle(.secondary)
            if let prURL, !prURL.isEmpty {
                PRURLLink(urlString: prURL, font: .body)
            } else {
                Text("Not set")
                    .font(.body)
            }
        }
    }
}

/// Picker sheet for the work-item detail Repo: row's `Change…`
/// affordance (per Follow-up chore #12 of
/// `multi-repo-work-modeling.md`). Reuses the create form's
/// recent-repos source — the same per-product distinct-URL set the
/// view model exposes — and adds two row types the create form
/// doesn't need:
///
/// - **Custom URL…** lets the user pin an override the recent set
///   doesn't yet contain (the empirical set bootstraps from the
///   first explicit `--repo`, so brand-new URLs always start
///   custom).
/// - **Clear (inherit from product)** drops the override and falls
///   back to product inheritance. Hidden when there's nothing to
///   clear (current state is already inherited / unresolved).
struct RepoOverridePicker: View {
    let presentation: RepoOverridePresentation
    let recentURLs: [String]
    let onCancel: () -> Void
    let onSelect: (String) -> Void
    let onClear: () -> Void

    @State private var customURL: String = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Change repo")
                .font(.title3.weight(.semibold))

            VStack(alignment: .leading, spacing: 4) {
                Text("Current")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Text(presentation.cliLine)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }

            if !recentURLs.isEmpty {
                VStack(alignment: .leading, spacing: 6) {
                    Text("Recent repos")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    VStack(alignment: .leading, spacing: 4) {
                        ForEach(recentURLs, id: \.self) { url in
                            Button(action: { onSelect(url) }) {
                                HStack(spacing: 6) {
                                    Image(systemName: "folder")
                                        .foregroundStyle(.secondary)
                                    Text(shortRepoName(for: url))
                                        .font(.body)
                                    Text(url)
                                        .font(.caption)
                                        .foregroundStyle(.secondary)
                                        .lineLimit(1)
                                        .truncationMode(.middle)
                                    Spacer(minLength: 4)
                                }
                                .contentShape(Rectangle())
                            }
                            .buttonStyle(.plain)
                            .accessibilityIdentifier("repo-picker-recent-\(url)")
                        }
                    }
                }
            }

            VStack(alignment: .leading, spacing: 6) {
                Text("Custom URL")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                HStack(spacing: 8) {
                    TextField(
                        "https://github.com/owner/repo.git",
                        text: $customURL
                    )
                    .textFieldStyle(.roundedBorder)
                    .accessibilityIdentifier("repo-picker-custom-url")
                    Button("Use") {
                        onSelect(customURL)
                    }
                    .disabled(
                        customURL.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                    )
                    .accessibilityIdentifier("repo-picker-custom-use")
                }
            }

            if canClear {
                Button(action: onClear) {
                    Label("Clear (inherit from product)", systemImage: "arrow.uturn.backward")
                }
                .buttonStyle(.link)
                .accessibilityIdentifier("repo-picker-clear")
            }

            HStack {
                Spacer()
                Button("Cancel", action: onCancel)
                    .keyboardShortcut(.cancelAction)
            }
        }
        .padding(20)
        .frame(width: 480, alignment: .leading)
        .accessibilityIdentifier("repo-override-picker")
    }

    /// Whether the "Clear (inherit from product)" affordance has any
    /// effect. The override only exists in the `.taskOverride` state;
    /// `.productDefault` and `.none` are already inheriting (or have
    /// nothing to inherit), so clearing would be a no-op and the
    /// button stays hidden to avoid implying an action.
    private var canClear: Bool {
        presentation.provenance == .taskOverride
    }
}

/// Dependencies subsection rendered inside the card detail popover.
/// Mirrors the CLI `boss <kind> show` output: incoming edges
/// (prerequisites) and outgoing edges (dependents) as two short
/// lists, each row hyperlinked to the corresponding card. Collapses
/// to nothing when both lists are empty so the popover doesn't grow
/// taller for cards with no dependencies (design item 12).
struct WorkDependenciesSection: View {
    @ObservedObject var model: ChatViewModel
    let taskID: String

    var body: some View {
        let prereqs = model.dependencyPrereqs(for: taskID)
        let dependents = model.dependencyDependents(for: taskID)

        if prereqs.isEmpty && dependents.isEmpty {
            EmptyView()
        } else {
            VStack(alignment: .leading, spacing: 10) {
                Text("Dependencies")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .textCase(.uppercase)

                if !prereqs.isEmpty {
                    dependencyList(title: "Prerequisites", rows: prereqs)
                }
                if !dependents.isEmpty {
                    dependencyList(title: "Dependents", rows: dependents)
                }
            }
            .accessibilityIdentifier("work-dependencies-section")
        }
    }

    @ViewBuilder
    private func dependencyList(title: String, rows: [WorkDependencyRow]) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(.caption)
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 2) {
                ForEach(rows) { row in
                    WorkDependencyRowView(row: row) {
                        model.selectWorkCard(row.id)
                    }
                }
            }
        }
    }
}

private struct WorkDependencyRowView: View {
    let row: WorkDependencyRow
    let onSelect: () -> Void

    var body: some View {
        Button(action: onSelect) {
            HStack(spacing: 6) {
                Image(systemName: kindSymbol)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .frame(width: 14)
                Text(row.title)
                    .font(.body)
                    .foregroundStyle(linkColor)
                    .underline(isLinkable)
                    .lineLimit(1)
                    .truncationMode(.tail)
                Spacer(minLength: 6)
                Text(statusLabel)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(
                        Capsule(style: .continuous)
                            .fill(Color(nsColor: .quaternaryLabelColor).opacity(0.18))
                    )
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(!isLinkable)
        .help(row.title)
    }

    private var isLinkable: Bool {
        row.kind != .unknown
    }

    private var linkColor: Color {
        isLinkable ? Color.accentColor : .primary
    }

    private var kindSymbol: String {
        switch row.kind {
        case .task:
            return "checkmark.circle"
        case .chore:
            return "wrench.and.screwdriver"
        case .project:
            return "folder"
        case .unknown:
            return "questionmark.circle"
        }
    }

    private var statusLabel: String {
        row.status.replacingOccurrences(of: "_", with: " ").capitalized
    }
}

private struct PRURLLink: View {
    let urlString: String
    let font: Font
    /// Board-local disambiguation key from
    /// [[ChatViewModel.ambiguousVisibleRepoNames]]. When set, the label
    /// shortens to `repo#n` for repos not in the set and falls back to
    /// `org/repo#n` for repos that *are*. Pass `nil` to force the full
    /// `org/repo#n` form unconditionally — that's what the detail
    /// popover does, since the popover is the "tooltip-like" surface
    /// the design calls out as always-full.
    var ambiguousRepoNames: Set<String>? = nil

    var body: some View {
        let label = pullRequestLinkLabel(
            for: urlString,
            ambiguousRepoNames: ambiguousRepoNames
        ) ?? urlString
        if let url = URL(string: urlString), url.scheme != nil {
            Link(destination: url) {
                Text(label)
                    .font(font)
                    .foregroundStyle(Color.accentColor)
                    .underline()
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            .buttonStyle(.plain)
            .pointerStyle(.link)
            .help(tooltip)
        } else {
            Text(label)
                .font(font)
                .foregroundStyle(.secondary)
                .lineLimit(1)
        }
    }

    /// Tooltip surfaces the unambiguous `org/repo#n` form (or, if the
    /// URL isn't a recognisable GitHub PR, the raw URL) so a user who
    /// hovered to verify gets the disambiguating context the shortened
    /// label may have dropped.
    private var tooltip: String {
        if let full = pullRequestLinkLabel(for: urlString, ambiguousRepoNames: nil) {
            return "\(full)\n\(urlString)"
        }
        return urlString
    }
}

private struct WorkCreateSheet: View {
    let request: WorkCreateRequest
    /// Parent product's default repo URL when the request is for a
    /// task or chore. Drives the chore/task form's repo render mode
    /// per design Q10: hidden-with-disclosure when set, shown-required
    /// when nil.
    let productDefaultRepoURL: String?
    /// Empirical known-repo set for the parent product. Powers the
    /// "Recent repos" picker. Empty when the form is for a product or
    /// project.
    let knownRepos: [String]
    let onCancel: () -> Void
    /// Callback args: `(name, description, repoRemoteURL, goal,
    /// setAsProductDefault)`. The last flag is meaningful only on
    /// task/chore submissions made against a product without a
    /// default repo where the user typed a fresh URL.
    let onCreate: (String, String, String, String, Bool) -> Void

    @State private var name = ""
    @State private var description = ""
    @State private var goal = ""
    @State private var repoFormState: WorkCreateRepoFormState

    init(
        request: WorkCreateRequest,
        productDefaultRepoURL: String?,
        knownRepos: [String],
        onCancel: @escaping () -> Void,
        onCreate: @escaping (String, String, String, String, Bool) -> Void
    ) {
        self.request = request
        self.productDefaultRepoURL = productDefaultRepoURL
        self.knownRepos = knownRepos
        self.onCancel = onCancel
        self.onCreate = onCreate
        _repoFormState = State(
            initialValue: WorkCreateRepoFormState(
                productRepoURL: productDefaultRepoURL,
                knownRepos: knownRepos
            )
        )
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(title)
                .font(.title3.weight(.semibold))

            TextField("Name", text: $name)

            switch request.kind {
            case .product:
                TextField("Description", text: $description)
                VStack(alignment: .leading, spacing: 4) {
                    // Product-create repo field is independent of the
                    // chore/task form state — same wire field, but the
                    // form mode + recent-repos picker only make sense
                    // *under* an existing product.
                    TextField(
                        ProductRepoFieldCopy.placeholder,
                        text: productCreateRepoBinding
                    )
                    Text(ProductRepoFieldCopy.helperText)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            case .project:
                TextField("Description", text: $description)
                TextField("Goal", text: $goal)
            case .task, .chore:
                TextField("Description", text: $description)
                workItemRepoField
            }

            HStack {
                Spacer()
                Button("Cancel", action: onCancel)
                Button("Create") {
                    onCreate(
                        name,
                        description,
                        submittedRepoURL,
                        goal,
                        repoFormState.shouldSetAsProductDefault
                    )
                }
                .keyboardShortcut(.defaultAction)
                .disabled(isSubmitDisabled)
            }
        }
        .padding(20)
        .frame(width: 460)
    }

    /// Repo field for chore/task creation. Renders the disclosure
    /// form in product-has-default mode and the required form in
    /// product-has-no-default mode, with the "Recent repos" picker
    /// and "Set as product default" affordance gated as the design
    /// describes.
    @ViewBuilder
    private var workItemRepoField: some View {
        switch repoFormState.mode {
        case .productHasDefault(let defaultURL):
            DisclosureGroup(
                WorkItemRepoFieldCopy.overrideDisclosureLabel,
                isExpanded: $repoFormState.overrideEnabled
            ) {
                VStack(alignment: .leading, spacing: 6) {
                    Text("Inherits from product: \(defaultURL)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    recentReposPicker
                    TextField(
                        WorkItemRepoFieldCopy.overridePlaceholder,
                        text: $repoFormState.enteredURL
                    )
                    Text(WorkItemRepoFieldCopy.overrideHelperText)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .padding(.top, 4)
            }
        case .productHasNoDefault:
            VStack(alignment: .leading, spacing: 6) {
                recentReposPicker
                TextField(
                    WorkItemRepoFieldCopy.requiredPlaceholder,
                    text: $repoFormState.enteredURL
                )
                Text(WorkItemRepoFieldCopy.requiredHelperText)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                if repoFormState.showSetAsProductDefaultCheckbox {
                    Toggle(
                        WorkItemRepoFieldCopy.setAsProductDefaultLabel,
                        isOn: $repoFormState.setAsProductDefault
                    )
                    .font(.caption)
                }
            }
        }
    }

    /// "Recent repos" picker — surfaces the product's empirical
    /// known-repo set. The first option is a no-op placeholder that
    /// the picker shows when the user hasn't picked anything yet;
    /// selecting any other entry copies its URL into the text field.
    @ViewBuilder
    private var recentReposPicker: some View {
        if !knownRepos.isEmpty {
            Picker(
                WorkItemRepoFieldCopy.recentReposLabel,
                selection: pickerSelectionBinding
            ) {
                Text("Choose…").tag(Optional<String>.none)
                ForEach(knownRepos, id: \.self) { url in
                    Text("\(shortRepoName(for: url)) — \(url)")
                        .tag(Optional<String>.some(url))
                }
            }
            .pickerStyle(.menu)
        }
    }

    /// Two-way binding between the recent-repos `Picker` and the text
    /// field. Reading reports the URL when it exactly matches a known
    /// entry; writing copies the chosen URL into the entered text.
    private var pickerSelectionBinding: Binding<String?> {
        Binding(
            get: {
                let trimmed = repoFormState.enteredURL
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                return knownRepos.contains(trimmed) ? trimmed : nil
            },
            set: { newValue in
                guard let newValue else { return }
                repoFormState.enteredURL = newValue
                repoFormState.setAsProductDefault = false
            }
        )
    }

    /// Binding for the product-create repo field. Product creation
    /// doesn't share the chore/task form state — the field is a
    /// vanilla text input — so we keep the value alongside the rest
    /// of the chore/task form state in the same `enteredURL` slot
    /// (the two cases are mutually exclusive by request kind, so the
    /// reuse is safe and avoids a parallel `@State`).
    private var productCreateRepoBinding: Binding<String> {
        $repoFormState.enteredURL
    }

    /// The URL string to forward to `onCreate`. For chore/task in
    /// `.productHasDefault` mode with the override disclosure closed,
    /// the value is the empty string — submission falls through to
    /// the product default, matching the engine's
    /// "absent field → inherit" semantics.
    private var submittedRepoURL: String {
        switch request.kind {
        case .product:
            return repoFormState.enteredURL
        case .project:
            return ""
        case .task, .chore:
            return repoFormState.submittedURL ?? ""
        }
    }

    /// Encodes the submission gate. Name is always required; the
    /// repo field adds a second gate for chore/task creation under a
    /// product with no default.
    private var isSubmitDisabled: Bool {
        if name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            return true
        }
        switch request.kind {
        case .product, .project:
            return false
        case .task, .chore:
            return repoFormState.isSubmissionBlocked
        }
    }

    private var title: String {
        switch request.kind {
        case .product:
            return "New Product"
        case .project:
            return "New Project"
        case .task:
            return "New Task"
        case .chore:
            return "New Chore"
        }
    }
}

/// Shared layout metrics for the redesigned Edit Product dialog (#982).
///
/// One source of truth for the dialog's grid so the Product, External
/// Tracker, and GitHub-account sections all inset to the same left margin
/// and share one label column. The label column is sized for the longest
/// label in the form ("Worker branch prefix" / "Owner / Organization") so
/// no label clips or runs past the dialog edge the way the first
/// `Form`-based pass did.
private enum ProductDialogMetrics {
    /// Dialog width. Wide enough that a 160pt label column still leaves a
    /// comfortable field column; a deliberate, modest bump over #982's 480.
    static let width: CGFloat = 520
    /// Outer inset for the title, section stack, and footer button bar.
    /// Full-bleed dividers ignore it so the header/content/footer read as
    /// distinct bands.
    static let horizontalPadding: CGFloat = 24
    /// Fixed width of the leading label column. Fits the longest label at
    /// the body font with margin to spare, which is the whole point of the
    /// redesign — every row's field starts at the same x.
    static let labelColumnWidth: CGFloat = 160
    /// Gap between the label column and the field column.
    static let labelFieldGap: CGFloat = 12
    /// Vertical gap between rows inside one section.
    static let rowSpacing: CGFloat = 12
    /// Vertical gap between a section header and its first row.
    static let headerRowGap: CGFloat = 10
    /// Vertical gap between sections.
    static let sectionGap: CGFloat = 22
}

/// `LabeledContentStyle` that lays every form row out on the shared grid:
/// a fixed-width, leading-aligned label column followed by a field column
/// that fills the remaining width. Applied once to the whole form so all
/// rows — across all three sections — line up. A label-less row
/// (`LabeledContent { ... } label: { EmptyView() }`) still reserves the
/// column, which is how the Reverse-close toggle and Unset button align to
/// the field column instead of floating.
private struct ProductFixedLabelStyle: LabeledContentStyle {
    func makeBody(configuration: Configuration) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: ProductDialogMetrics.labelFieldGap) {
            configuration.label
                .frame(width: ProductDialogMetrics.labelColumnWidth, alignment: .leading)
            configuration.content
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

/// A titled group in the Edit Product dialog: a real, left-aligned section
/// header (the first pass rendered these as centered, stray-looking labels)
/// over a consistently-spaced stack of rows. All three sections use this so
/// their headers and content share one alignment grid.
private struct ProductFormSection<Content: View>: View {
    private let title: String
    private let content: Content

    init(_ title: String, @ViewBuilder content: () -> Content) {
        self.title = title
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: ProductDialogMetrics.headerRowGap) {
            Text(title)
                .font(.headline)
                .frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityAddTraits(.isHeader)
            VStack(alignment: .leading, spacing: ProductDialogMetrics.rowSpacing) {
                content
            }
        }
    }
}

private struct WorkEditSheet: View {
    let request: WorkEditRequest
    let onCancel: () -> Void
    let onSave: (String, String, String, String, String, String, String, String, String) -> Void
    let onSetTracker: ((String, String, String, Int, Bool) -> Void)?
    let onUnsetTracker: (() -> Void)?

    @State private var name: String
    @State private var description: String
    @State private var status: String
    @State private var repoRemoteURL: String
    @State private var goal: String
    @State private var priority: String
    @State private var prURL: String
    @State private var workerBranchPrefix: String
    @State private var docsRepo: String

    // External tracker state (product only)
    @State private var trackerKind: String
    @State private var trackerOrg: String
    @State private var trackerRepo: String
    @State private var trackerProjectNumber: String
    @State private var trackerReverseClose: Bool
    // True if the product had a tracker bound when the sheet opened.
    private let initialTrackerBound: Bool

    init(
        request: WorkEditRequest,
        onCancel: @escaping () -> Void,
        onSave: @escaping (String, String, String, String, String, String, String, String, String) -> Void,
        onSetTracker: ((String, String, String, Int, Bool) -> Void)? = nil,
        onUnsetTracker: (() -> Void)? = nil
    ) {
        self.request = request
        self.onCancel = onCancel
        self.onSave = onSave
        self.onSetTracker = onSetTracker
        self.onUnsetTracker = onUnsetTracker

        switch request.item {
        case .product(let product):
            _name = State(initialValue: product.name)
            _description = State(initialValue: product.description)
            _status = State(initialValue: product.status)
            _repoRemoteURL = State(initialValue: product.repoRemoteURL ?? "")
            _goal = State(initialValue: "")
            _priority = State(initialValue: "")
            _prURL = State(initialValue: "")
            _workerBranchPrefix = State(initialValue: product.workerBranchPrefix ?? "")
            _docsRepo = State(initialValue: product.docsRepo ?? "")

            if let kind = product.externalTrackerKind,
               let configJSON = product.externalTrackerConfig,
               let configData = configJSON.data(using: .utf8),
               let config = try? JSONSerialization.jsonObject(with: configData) as? [String: Any] {
                _trackerKind = State(initialValue: kind)
                _trackerOrg = State(initialValue: config["org"] as? String ?? "")
                _trackerRepo = State(initialValue: config["repo"] as? String ?? "")
                let projectNum = config["project_number"]
                if let n = projectNum as? Int {
                    _trackerProjectNumber = State(initialValue: String(n))
                } else if let n = projectNum as? Double {
                    _trackerProjectNumber = State(initialValue: String(Int(n)))
                } else {
                    _trackerProjectNumber = State(initialValue: "")
                }
                _trackerReverseClose = State(initialValue: config["reverse_close"] as? Bool ?? false)
                initialTrackerBound = true
            } else {
                _trackerKind = State(initialValue: "github")
                _trackerOrg = State(initialValue: "")
                _trackerRepo = State(initialValue: "")
                _trackerProjectNumber = State(initialValue: "")
                _trackerReverseClose = State(initialValue: false)
                initialTrackerBound = false
            }

        case .project(let project):
            _name = State(initialValue: project.name)
            _description = State(initialValue: project.description)
            _status = State(initialValue: project.status)
            _repoRemoteURL = State(initialValue: "")
            _goal = State(initialValue: project.goal)
            _priority = State(initialValue: project.priority)
            _prURL = State(initialValue: "")
            _workerBranchPrefix = State(initialValue: "")
            _docsRepo = State(initialValue: "")
            _trackerKind = State(initialValue: "github")
            _trackerOrg = State(initialValue: "")
            _trackerRepo = State(initialValue: "")
            _trackerProjectNumber = State(initialValue: "")
            _trackerReverseClose = State(initialValue: false)
            initialTrackerBound = false
        case .task(let task), .chore(let task):
            _name = State(initialValue: task.name)
            _description = State(initialValue: task.description)
            _status = State(initialValue: task.status)
            _repoRemoteURL = State(initialValue: "")
            _goal = State(initialValue: "")
            _priority = State(initialValue: task.priority)
            _prURL = State(initialValue: task.prURL ?? "")
            _workerBranchPrefix = State(initialValue: "")
            _docsRepo = State(initialValue: "")
            _trackerKind = State(initialValue: "github")
            _trackerOrg = State(initialValue: "")
            _trackerRepo = State(initialValue: "")
            _trackerProjectNumber = State(initialValue: "")
            _trackerReverseClose = State(initialValue: false)
            initialTrackerBound = false
        }
    }

    var body: some View {
        if case .product = request.item {
            productBody
        } else {
            sharedBody
        }
    }

    @ViewBuilder
    private var productBody: some View {
        VStack(alignment: .leading, spacing: 0) {
            // Header band.
            Text("Edit Product")
                .font(.title3.weight(.semibold))
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, ProductDialogMetrics.horizontalPadding)
                .padding(.top, ProductDialogMetrics.horizontalPadding)
                .padding(.bottom, 12)

            Divider()

            // Content band: three sections on one shared label/field grid.
            VStack(alignment: .leading, spacing: ProductDialogMetrics.sectionGap) {
                ProductFormSection("Product") {
                    LabeledContent("Name") {
                        TextField("", text: $name, prompt: Text("Product name"))
                    }
                    LabeledContent("Description") {
                        TextField("", text: $description, prompt: Text("Optional"))
                    }
                    LabeledContent("Status") {
                        Picker("Status", selection: $status) {
                            ForEach(["active", "paused", "archived"], id: \.self) { s in
                                Text(s.capitalized).tag(s)
                            }
                        }
                        .labelsHidden()
                        .frame(maxWidth: 200, alignment: .leading)
                    }
                    LabeledContent("Repository URL") {
                        TextField(
                            "", text: $repoRemoteURL,
                            prompt: Text("https://github.com/org/repo")
                        )
                    }
                    LabeledContent("Worker branch prefix") {
                        VStack(alignment: .leading, spacing: 4) {
                            TextField("", text: $workerBranchPrefix, prompt: Text("e.g. bduff/"))
                            Text(
                                "Optional. Workers push to <prefix>exec_<id>. " +
                                "Leave blank to use the default prefix boss/. " +
                                "Trailing / is conventional."
                            )
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                        }
                    }
                    LabeledContent("Docs repo") {
                        VStack(alignment: .leading, spacing: 4) {
                            TextField(
                                "", text: $docsRepo,
                                prompt: Text("owner/repo")
                            )
                            Text(
                                "Optional. Investigation and design writeups open PRs here. " +
                                "Leave blank to use the user-level BOSS_USER_DOCS_REPO default."
                            )
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                        }
                    }
                }

                ProductFormSection("External Tracker") {
                    LabeledContent("Kind") {
                        Picker("Kind", selection: $trackerKind) {
                            Text("GitHub").tag("github")
                        }
                        .labelsHidden()
                        .frame(maxWidth: 200, alignment: .leading)
                    }
                    if trackerKind == "github" {
                        LabeledContent("Owner / Organization") {
                            TextField("", text: $trackerOrg, prompt: Text("e.g. spinyfin"))
                        }
                        LabeledContent("Repository") {
                            TextField("", text: $trackerRepo, prompt: Text("e.g. mono"))
                        }
                        LabeledContent("Project number") {
                            TextField("", text: $trackerProjectNumber, prompt: Text("e.g. 7"))
                        }
                        // Label-less row: toggle + Unset align to the field
                        // column rather than floating under the fields.
                        LabeledContent {
                            VStack(alignment: .leading, spacing: 8) {
                                Toggle("Reverse-close", isOn: $trackerReverseClose)
                                    .help(
                                        "When a work item is marked done without a merged PR, " +
                                        "close the upstream GitHub issue."
                                    )
                                if initialTrackerBound {
                                    Button("Unset", role: .destructive) {
                                        onUnsetTracker?()
                                    }
                                }
                            }
                        } label: {
                            EmptyView()
                        }
                    }
                }

                ProductFormSection("GitHub account") {
                    GitHubAccountSection()
                }
            }
            .labeledContentStyle(ProductFixedLabelStyle())
            .padding(.horizontal, ProductDialogMetrics.horizontalPadding)
            .padding(.vertical, ProductDialogMetrics.sectionGap)

            Divider()

            // Footer band: dialog-level actions.
            HStack {
                Spacer()
                Button("Cancel", action: onCancel)
                Button("Save") {
                    onSave(
                        name, description, status, repoRemoteURL, "", "", "", workerBranchPrefix,
                        docsRepo
                    )
                    if trackerFormValid,
                       let num = Int(trackerProjectNumber.trimmingCharacters(in: .whitespacesAndNewlines)) {
                        onSetTracker?(trackerKind, trackerOrg, trackerRepo, num, trackerReverseClose)
                    }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(
                    name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ||
                    trackerFieldsEntered && !trackerFormValid
                )
            }
            .padding(.horizontal, ProductDialogMetrics.horizontalPadding)
            .padding(.vertical, 16)
        }
        .frame(width: ProductDialogMetrics.width)
    }

    @ViewBuilder
    private var sharedBody: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(title)
                .font(.title3.weight(.semibold))

            TextField("Name", text: $name)
            TextField("Description", text: $description)

            switch request.item {
            case .project:
                Picker("Status", selection: $status) {
                    ForEach(["planned", "active", "blocked", "done", "archived"], id: \.self) { status in
                        Text(status.capitalized).tag(status)
                    }
                }
                Picker("Priority", selection: $priority) {
                    ForEach(["low", "medium", "high"], id: \.self) { priority in
                        Text(priority.capitalized).tag(priority)
                    }
                }
                TextField("Goal", text: $goal)
            case .task, .chore:
                Picker("Status", selection: $status) {
                    ForEach(["todo", "active", "blocked", "in_review", "done"], id: \.self) { status in
                        Text(status.replacingOccurrences(of: "_", with: " ").capitalized).tag(status)
                    }
                }
                Picker("Priority", selection: $priority) {
                    ForEach(["low", "medium", "high"], id: \.self) { priority in
                        Text(priority.capitalized).tag(priority)
                    }
                }
                TextField("PR URL", text: $prURL)
            case .product:
                EmptyView()
            }

            HStack {
                Spacer()
                Button("Cancel", action: onCancel)
                Button("Save") {
                    onSave(name, description, status, repoRemoteURL, goal, priority, prURL, workerBranchPrefix, docsRepo)
                }
                .keyboardShortcut(.defaultAction)
                .disabled(name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(20)
        .frame(width: 440)
    }

    private var trackerFieldsEntered: Bool {
        let org = trackerOrg.trimmingCharacters(in: .whitespacesAndNewlines)
        let repo = trackerRepo.trimmingCharacters(in: .whitespacesAndNewlines)
        let project = trackerProjectNumber.trimmingCharacters(in: .whitespacesAndNewlines)
        return !org.isEmpty || !repo.isEmpty || !project.isEmpty
    }

    private var trackerFormValid: Bool {
        guard trackerKind == "github" else { return false }
        let org = trackerOrg.trimmingCharacters(in: .whitespacesAndNewlines)
        let repo = trackerRepo.trimmingCharacters(in: .whitespacesAndNewlines)
        let project = trackerProjectNumber.trimmingCharacters(in: .whitespacesAndNewlines)
        return !org.isEmpty && !repo.isEmpty && Int(project) != nil
    }

    private var title: String {
        switch request.item {
        case .product:
            return "Edit Product"
        case .project:
            return "Edit Project"
        case .task:
            return "Edit Task"
        case .chore:
            return "Edit Chore"
        }
    }
}


/// "GitHub account" subsection of the external-tracker settings — drives
/// and renders the engine-owned OAuth device flow (OAuth device-flow design
/// §4/§7/§8). All flow logic lives in the engine; the display mapping lives
/// in `GitHubAuthPresentation`. This view is a thin renderer over
/// `model.gitHubAuthState` plus button wiring to the `gitHubAuth*` bridges.
///
/// The auth state is global (one github.com token shared across all
/// GitHub-bound products), so this subsection shows the same state in every
/// product's settings sheet.
private struct GitHubAccountSection: View {
    @EnvironmentObject private var model: ChatViewModel

    private var presentation: GitHubAuthPresentation {
        GitHubAuthPresentation.forState(model.gitHubAuthState)
    }

    var body: some View {
        // The enclosing `ProductFormSection("GitHub account")` now renders the
        // header and supplies the section's spacing, so this view is just the
        // account status/flow content.
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                if presentation.isBusy {
                    ProgressView()
                        .controlSize(.small)
                } else {
                    Image(systemName: presentation.statusIcon)
                        .foregroundStyle(.secondary)
                }
                Text(presentation.statusLine)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            if let prompt = presentation.pendingPrompt {
                pendingPromptView(prompt)
            }

            ForEach(Array(presentation.banners.enumerated()), id: \.offset) { _, banner in
                bannerView(banner)
            }

            if !presentation.actions.isEmpty {
                HStack(spacing: 8) {
                    ForEach(presentation.actions, id: \.self) { action in
                        actionButton(action)
                    }
                    Spacer()
                }
            }
        }
    }

    @ViewBuilder
    private func actionButton(_ action: GitHubAuthPresentation.Action) -> some View {
        switch action {
        case .connect:
            Button(presentation.connectIsRestart ? "Start over" : "Connect") {
                model.gitHubAuthConnect()
            }
        case .cancel:
            Button("Cancel") {
                model.gitHubAuthCancel()
            }
        case .disconnect:
            Button("Disconnect", role: .destructive) {
                model.gitHubAuthDisconnect()
            }
        case .reauthorize:
            Button("Re-authorize") {
                model.gitHubAuthReauthorize()
            }
        }
    }

    @ViewBuilder
    private func pendingPromptView(_ prompt: GitHubAuthPresentation.PendingPrompt) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Text("Code")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                Text(prompt.userCode)
                    .font(.system(.title3, design: .monospaced).weight(.semibold))
                    .textSelection(.enabled)
            }
            HStack(spacing: 8) {
                if let url = URL(string: prompt.openURL) {
                    Link("Open in browser", destination: url)
                }
                Text(prompt.verificationURL)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
            Text("Enter the code at the verification URL to authorize Boss for issue sync.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(8)
        .background(Color(nsColor: .controlBackgroundColor))
        .clipShape(RoundedRectangle(cornerRadius: 6))
    }

    @ViewBuilder
    private func bannerView(_ banner: GitHubAuthPresentation.Banner) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .top, spacing: 6) {
                Image(systemName: bannerIcon(banner.kind))
                    .foregroundStyle(bannerColor(banner.kind))
                Text(banner.message)
                    .font(.caption)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if banner.actionURL != nil || banner.offersRecheck {
                HStack(spacing: 8) {
                    if let urlString = banner.actionURL,
                       let label = banner.actionLabel,
                       let url = URL(string: urlString) {
                        Link(label, destination: url)
                    }
                    if banner.offersRecheck {
                        Button("Re-check") {
                            model.gitHubAuthRecheck()
                        }
                    }
                }
                .font(.caption)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(8)
        .background(bannerColor(banner.kind).opacity(0.12))
        .clipShape(RoundedRectangle(cornerRadius: 6))
    }

    private func bannerIcon(_ kind: GitHubAuthPresentation.Banner.Kind) -> String {
        switch kind {
        case .needsOrgApproval: return "building.2"
        case .needsSso: return "lock.shield"
        case .unknownOrg: return "questionmark.circle"
        case .limitedScopes: return "exclamationmark.triangle"
        case .expired: return "clock.badge.exclamationmark"
        case .denied: return "hand.raised"
        case .error: return "exclamationmark.octagon"
        }
    }

    private func bannerColor(_ kind: GitHubAuthPresentation.Banner.Kind) -> Color {
        switch kind {
        case .needsOrgApproval, .needsSso, .unknownOrg, .limitedScopes, .expired:
            return .orange
        case .denied, .error:
            return .red
        }
    }
}

/// Capsule chip surfacing a repo's short name on a kanban card or
/// product header. Hover tooltip carries the full URL plus the
/// provenance string ("Inherited from product" vs "Repo set on this
/// card") so the reader can tell where the URL came from without
/// digging into the popover. Pure view — all the mode/provenance
/// logic lives on `RepoChipPresentation` so tests don't need a
/// SwiftUI host.
///
/// The chip renders in a neutral style matching `WorkStatusBadge`
/// (used by `Blocked` and the project tag). Earlier the override
/// variant carried an accent-blue tint, but the color had no stable
/// meaning to readers — and with per-card chips now only appearing
/// on rows that carry their own repo, the "this row is special" signal
/// is already conveyed by the chip's mere presence on the card.
struct RepoChipView: View {
    let presentation: RepoChipPresentation
    var emphasized: Bool = false

    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "folder")
                .font(.caption2)
            Text(presentation.shortName)
                .font(.caption.weight(.semibold))
                .lineLimit(1)
                .truncationMode(.tail)
        }
        .fixedSize(horizontal: true, vertical: false)
        .foregroundStyle(Color(nsColor: .labelColor))
        .padding(.horizontal, 7)
        .padding(.vertical, 3)
        .background(Color(nsColor: .controlBackgroundColor))
        .clipShape(Capsule())
        .overlay(
            Capsule().strokeBorder(
                Color(nsColor: .separatorColor),
                lineWidth: 0.5
            )
        )
        .help(presentation.tooltip)
        .accessibilityLabel(presentation.accessibilityLabel)
    }
}

/// Upstream-link affordance rendered in the kanban card footer when
/// a work item carries an `externalRef`. Three visual states:
///
/// - **Bound** (`isStale == false`): accent-colored `↗ #N` link, opens the
///   upstream URL in the default browser.
/// - **Stale** (`isStale == true`): secondary-colored with strikethrough,
///   still clickable; tooltip explains the binding was cleared.
/// - **Absent** (`ExternalRefLinkPresentation.forTask` returns `nil`): no
///   view rendered at all (callers gate on nil).
private struct ExternalRefLinkView: View {
    let presentation: ExternalRefLinkPresentation

    var body: some View {
        if let url = URL(string: presentation.url), url.scheme != nil {
            Link(destination: url) {
                labelText
            }
            .buttonStyle(.plain)
            .pointerStyle(.link)
            .help(presentation.tooltip)
        } else {
            labelText
                .help(presentation.tooltip)
        }
    }

    private var labelText: some View {
        Text(presentation.label)
            .font(.system(.caption2, design: .monospaced))
            .foregroundStyle(presentation.isStale ? Color.secondary : Color.accentColor)
            .strikethrough(presentation.isStale)
            .accessibilityLabel(presentation.isStale
                ? "Upstream issue (stale): \(presentation.label)"
                : "Open upstream issue: \(presentation.label)")
            .lineLimit(1)
            .fixedSize(horizontal: true, vertical: false)
    }
}

/// "🔧 conflict cleared" PR-card chip. Phase 5 #15 of the merge-
/// conflict design. Rendered on parent cards whose PR was the target
/// of a successful conflict-resolution attempt in the last 24h
/// (the freshness window lives on
/// [[ChatViewModel.badgeFreshnessWindow]]). The tooltip names the
/// action so a glance tells a reader *what* the engine cleared, not
/// just that something happened.
private struct ConflictClearedBadge: View {
    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "checkmark.circle.fill")
                .font(.caption2.weight(.semibold))
            Text("conflict cleared")
                .font(.caption.weight(.semibold))
                .lineLimit(1)
        }
        .fixedSize(horizontal: true, vertical: false)
        .foregroundStyle(Color.green)
        .help("The engine cleared a merge conflict on this PR within the last 24 hours.")
        .accessibilityLabel("Conflict cleared by the engine")
    }
}

/// "✅ ci auto-fixed" PR-card chip. Phase 11 #37 / design Q11.
/// Parallels [[ConflictClearedBadge]] — green, 24-hour freshness
/// window — for cards whose PR was the target of a successful CI
/// auto-fix attempt.
private struct CIAutoFixedBadge: View {
    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "checkmark.circle.fill")
                .font(.caption2.weight(.semibold))
            Text("ci auto-fixed")
                .font(.caption.weight(.semibold))
                .lineLimit(1)
        }
        .fixedSize(horizontal: true, vertical: false)
        .foregroundStyle(Color.green)
        .help("The engine auto-fixed a CI failure on this PR within the last 24 hours.")
        .accessibilityLabel("CI auto-fixed by the engine")
    }
}

/// In-flight / exhausted CI-failure chip. Design Q11 calls for two
/// visual states:
///  - 🟧 `ci failing (used/budget)` while the engine still has budget
///    and a worker is/was in flight.
///  - 🛑 `ci failing (exhausted)` once the engine has given up; the
///    user is the next actor (`boss engine ci retry`).
private struct CIFailureChip: View {
    let badge: CiFailureBadge

    private var label: String {
        switch badge.state {
        case .inFlight:
            if badge.budget > 0 {
                return "ci failing (\(badge.attemptsUsed)/\(badge.budget))"
            }
            return "ci failing"
        case .exhausted:
            return "ci failing (exhausted)"
        }
    }

    private var color: Color {
        switch badge.state {
        case .inFlight: return .orange
        case .exhausted: return .red
        }
    }

    private var icon: String {
        switch badge.state {
        case .inFlight: return "exclamationmark.triangle.fill"
        case .exhausted: return "octagon.fill"
        }
    }

    private var tooltip: String {
        switch badge.state {
        case .inFlight:
            return "The engine is auto-fixing a CI failure on this PR. \(badge.attemptsUsed) of \(badge.budget) attempts used."
        case .exhausted:
            return "The engine has exhausted its CI auto-fix budget for this PR. Run `boss engine ci retry <work-item>` to try again."
        }
    }

    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: icon)
                .font(.caption2.weight(.semibold))
            Text(label)
                .font(.caption.weight(.semibold))
                .lineLimit(1)
        }
        .fixedSize(horizontal: true, vertical: false)
        .foregroundStyle(color)
        .help(tooltip)
        .accessibilityLabel(tooltip)
    }
}

/// CI status indicator shown on Review-lane cards. Four visual states:
/// in-progress (yellow clock), success (green checkmark), fail (red X),
/// and unknown / nil (also rendered as in-progress). The unknown state
/// means the first poll is still pending — showing in-progress is truthful
/// ("we haven't checked yet") and keeps the icon slot occupied so it
/// doesn't pop in later.
private struct PrCiIndicator: View {
    let state: String
    var detail: String? = nil

    var body: some View {
        if let icon = systemImage {
            Image(systemName: icon)
                .font(.caption2.weight(.semibold))
                .foregroundStyle(tint)
                .help(tooltipText)
                .accessibilityLabel(tooltipText)
        }
    }

    private var systemImage: String? {
        switch state {
        case "success": return "checkmark.circle.fill"
        case "fail":    return "xmark.circle.fill"
        default:        return "clock.fill"
        }
    }

    private var tint: Color {
        switch state {
        case "success": return .green
        case "fail":    return .red
        default:        return .yellow
        }
    }

    private var tooltipText: String {
        switch state {
        case "success":
            return "All required CI checks passed"
        case "fail":
            if let detail, let checks = parseCheckNames(from: detail), !checks.isEmpty {
                return "Required CI check(s) failed: \(checks.joined(separator: ", "))"
            }
            return "Required CI check(s) failed"
        default:
            return "Required CI checks in progress"
        }
    }

    private func parseCheckNames(from json: String) -> [String]? {
        guard let data = json.data(using: .utf8),
              let arr = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]]
        else { return nil }
        return arr.compactMap { $0["name"] as? String }
    }
}

/// Merge-queue indicator for Review-lane cards. Shown when the PR is
/// currently in GitHub's merge queue — replaces the CI icon so the user
/// can immediately distinguish cards that are actively being shipped from
/// cards waiting for CI or human action.
private struct PrMergingIndicator: View {
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "arrow.triangle.merge")
                .font(.caption2.weight(.semibold))
            Text("merging")
                .font(.caption.weight(.semibold))
                .lineLimit(1)
        }
        .foregroundStyle(Color.white)
        .padding(.horizontal, 6)
        .padding(.vertical, 3)
        .background(backgroundColor)
        .clipShape(Capsule())
        .fixedSize()
        .help("PR is in the merge queue and actively being shipped.")
        .accessibilityLabel("In merge queue — merging")
    }

    private var backgroundColor: Color {
        switch colorScheme {
        case .light:
            return Color(red: 165/255, green: 107/255, blue: 0/255)
        case .dark:
            return Color(red: 158/255, green: 106/255, blue: 3/255)
        @unknown default:
            return Color(red: 165/255, green: 107/255, blue: 0/255)
        }
    }
}

/// Warning indicator shown on the PR card of a chain root when at least one
/// descendant revision is still `todo` or `active`. Signals that new commits
/// are incoming and the PR should not be merged yet.
private struct PrInRevisionIndicator: View {
    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "exclamationmark.triangle.fill")
                .font(.caption2.weight(.semibold))
            Text("in revision")
                .font(.caption.weight(.semibold))
                .lineLimit(1)
        }
        .foregroundStyle(Color.white)
        .padding(.horizontal, 6)
        .padding(.vertical, 3)
        .background(Color.orange)
        .clipShape(Capsule())
        .fixedSize()
        .help("A revision is in progress — do not merge this PR yet")
        .accessibilityLabel("In revision — do not merge")
    }
}

/// Review-gating indicator for Review-lane cards. Four states:
/// required (empty checklist — awaiting review), approved (green
/// checkmark — all required reviews in), changes_requested (exclamation
/// — at least one reviewer requested changes), unknown (hidden).
private struct PrReviewIndicator: View {
    let state: String
    var detail: String? = nil

    var body: some View {
        if let icon = systemImage {
            Image(systemName: icon)
                .font(.caption2.weight(.semibold))
                .foregroundStyle(tint)
                .help(tooltipText)
                .accessibilityLabel(tooltipText)
        }
    }

    private var systemImage: String? {
        switch state {
        case "required":           return "checklist"
        case "approved":           return "checkmark.seal.fill"
        case "changes_requested":  return "exclamationmark.circle.fill"
        default:                   return nil
        }
    }

    private var tint: Color {
        switch state {
        case "required":           return .secondary
        case "approved":           return .green
        case "changes_requested":  return .orange
        default:                   return .secondary
        }
    }

    private var tooltipText: String {
        let reviewers = reviewerNames(from: detail)
        switch state {
        case "required":
            return "Awaiting required review"
        case "approved":
            if reviewers.isEmpty { return "Approved" }
            return "Approved by \(reviewers.joined(separator: ", "))"
        case "changes_requested":
            if reviewers.isEmpty { return "Changes requested" }
            return "Changes requested by \(reviewers.joined(separator: ", "))"
        default:
            return "Review state unknown"
        }
    }

    private func reviewerNames(from json: String?) -> [String] {
        guard let json,
              let data = json.data(using: .utf8),
              let arr = try? JSONSerialization.jsonObject(with: data) as? [String]
        else { return [] }
        return arr
    }
}

/// "resolving conflicts" PR-card chip. Rendered on cards that have been
/// routed to the Doing column because a merge-resolution worker is
/// actively running against them. Signals to the user that the active
/// work is conflict resolution rather than the original task scope.
private struct ResolvingConflictsBadge: View {
    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "arrow.triangle.2.circlepath")
                .font(.caption2)
            Text("resolving conflicts")
                .font(.caption.weight(.semibold))
                .foregroundStyle(Color.orange)
                .lineLimit(1)
                .truncationMode(.tail)
        }
        // The icon keeps its intrinsic size, but the label is allowed to
        // truncate so a wide badge yields footer width to the fixed-size
        // repo chip and short-id rather than pushing them off the card's
        // right edge. The full text stays reachable via the tooltip and
        // accessibility label. `.layoutPriority(-1)` makes this badge the
        // first element the footer HStack squeezes when space is tight.
        .padding(.horizontal, 6)
        .padding(.vertical, 3)
        .background(Color.orange.opacity(0.12))
        .clipShape(Capsule())
        .layoutPriority(-1)
        .help("A worker is actively resolving a merge conflict on this PR.")
        .accessibilityLabel("Resolving merge conflict")
    }
}

/// "resolving CI failure" PR-card chip. Rendered on cards routed to the
/// Doing column because a CI-remediation worker is actively running
/// against them. Symmetric to [[ResolvingConflictsBadge]] — same visual
/// vocabulary, same orange tint, different icon and label.
private struct ResolvingCIFailureBadge: View {
    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "arrow.triangle.2.circlepath")
                .font(.caption2)
            Text("resolving CI failure")
                .font(.caption.weight(.semibold))
                .foregroundStyle(Color.orange)
                .lineLimit(1)
                .truncationMode(.tail)
        }
        // See [[ResolvingConflictsBadge]]: the label truncates so this
        // wider badge can't clip the trailing repo chip / short-id off the
        // card's right edge. Full text remains in the tooltip and a11y
        // label, and `.layoutPriority(-1)` makes it yield space first.
        .padding(.horizontal, 6)
        .padding(.vertical, 3)
        .background(Color.orange.opacity(0.12))
        .clipShape(Capsule())
        .layoutPriority(-1)
        .help("A worker is actively resolving a CI failure on this PR.")
        .accessibilityLabel("Resolving CI failure")
    }
}

/// "AI reviewing" card chip. Rendered on Doing-column cards held in `active`
/// while a `pr_review` reviewer execution is in flight (P992). The badge
/// distinguishes a card that is intentionally waiting for the AI review pass
/// from one that appears stuck with no explanation.
private struct ReviewingAIBadge: View {
    var body: some View {
        HStack(spacing: 3) {
            Image(systemName: "brain")
                .font(.caption2)
            Text("AI reviewing")
                .font(.caption.weight(.semibold))
                .foregroundStyle(Color.accentColor)
                .lineLimit(1)
                .truncationMode(.tail)
        }
        .padding(.horizontal, 6)
        .padding(.vertical, 3)
        .background(Color.accentColor.opacity(0.10))
        .clipShape(Capsule())
        .layoutPriority(-1)
        .help("An AI reviewer pass is running on this PR. The card will move to Review once the pass completes (typically within a minute).")
        .accessibilityLabel("AI reviewing PR")
    }
}

private struct WorkStatusBadge: View {
    let text: String
    var emphasized: Bool = false

    var body: some View {
        Text(text)
            .font(.caption.weight(.semibold))
            .foregroundStyle(foregroundColor)
            .lineLimit(1)
            .truncationMode(.tail)
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .background(backgroundColor)
            .clipShape(Capsule())
            .help(text)
    }

    private var foregroundColor: Color {
        if emphasized {
            return .accentColor
        }
        return Color(nsColor: .labelColor)
    }

    private var backgroundColor: Color {
        if emphasized {
            return Color.white.opacity(0.96)
        }
        return Color(nsColor: .controlBackgroundColor)
    }
}

/// Compact count chip for the navigator project row. Shows the number
/// of unblocked (green `▶ N`) or dependency-blocked (red `⏸ N`) tasks
/// for a project. Color + symbol ensures the chip is meaningful for
/// color-blind users. Visual weight deliberately subordinate to the
/// project name — matches the `T<n>` / `P<n>` chip treatment.
private struct ProjectTaskCountChip: View {
    enum Kind {
        case unblocked
        case blocked
    }

    let count: Int
    let kind: Kind

    var body: some View {
        Text(label)
            .font(.caption.weight(.semibold))
            .foregroundStyle(foregroundColor)
            .lineLimit(1)
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(backgroundColor)
            .clipShape(Capsule())
            .help(helpText)
    }

    private var label: String {
        switch kind {
        case .unblocked: return "▶ \(count)"
        case .blocked: return "⏸ \(count)"
        }
    }

    private var helpText: String {
        switch kind {
        case .unblocked: return "\(count) unblocked task\(count == 1 ? "" : "s") ready to dispatch"
        case .blocked: return "\(count) task\(count == 1 ? "" : "s") gated by a dependency"
        }
    }

    private var foregroundColor: Color {
        switch kind {
        case .unblocked: return Color(nsColor: .systemGreen)
        case .blocked: return Color(nsColor: .systemRed)
        }
    }

    private var backgroundColor: Color {
        switch kind {
        case .unblocked: return Color(nsColor: .systemGreen).opacity(0.12)
        case .blocked: return Color(nsColor: .systemRed).opacity(0.12)
        }
    }
}

/// Color-coded chip for the kanban card footer. Reads as `H`/`M`/`L`
/// to keep the chip narrow at typical column widths; the full label
/// surfaces in the tooltip and detail popover. We render every
/// priority (medium included) rather than hiding the default so the
/// field is always visible — invisible defaults are exactly what
/// pushed authors to stuff `[MEDIUM]` into the name in the first
/// place.
private struct PriorityChip: View {
    let priority: WorkPriority

    var body: some View {
        Text(letter)
            .font(.caption.weight(.bold))
            .foregroundStyle(foregroundColor)
            .frame(minWidth: 18)
            .padding(.horizontal, 6)
            .padding(.vertical, 3)
            .background(backgroundColor)
            .clipShape(Capsule())
            .help("Priority: \(priority.label)")
            .accessibilityLabel("Priority \(priority.label)")
    }

    private var letter: String {
        switch priority {
        case .high: return "H"
        case .medium: return "M"
        case .low: return "L"
        }
    }

    private var backgroundColor: Color {
        switch priority {
        case .high: return Color.red.opacity(0.18)
        case .medium: return Color.gray.opacity(0.18)
        case .low: return Color.blue.opacity(0.14)
        }
    }

    private var foregroundColor: Color {
        switch priority {
        case .high: return .red
        case .medium: return Color(nsColor: .secondaryLabelColor)
        case .low: return .blue
        }
    }
}

/// Effort-level chip rendered on kanban cards. Only shown when the
/// task carries a non-nil effort_level — unset rows must not masquerade
/// as medium.
private struct EffortChip: View {
    let effortLevel: String

    var body: some View {
        Text(letter)
            .font(.caption.weight(.bold))
            .foregroundStyle(foregroundColor)
            .padding(.horizontal, 6)
            .padding(.vertical, 3)
            .background(backgroundColor)
            .clipShape(Capsule())
            .help("Effort: \(label)")
            .accessibilityLabel("Effort \(label)")
    }

    private var letter: String {
        switch effortLevel {
        case "trivial": return "XS"
        case "small": return "S"
        case "medium": return "M"
        case "large": return "L"
        case "max": return "XL"
        default: return effortLevel.prefix(1).uppercased()
        }
    }

    private var label: String {
        switch effortLevel {
        case "trivial": return "Trivial"
        case "small": return "Small"
        case "medium": return "Medium"
        case "large": return "Large"
        case "max": return "Max"
        default: return effortLevel.capitalized
        }
    }

    private var backgroundColor: Color {
        switch effortLevel {
        case "trivial": return Color.blue.opacity(0.12)
        case "small": return Color.green.opacity(0.14)
        case "medium": return Color.gray.opacity(0.18)
        case "large": return Color.orange.opacity(0.18)
        case "max": return Color.red.opacity(0.14)
        default: return Color.gray.opacity(0.18)
        }
    }

    private var foregroundColor: Color {
        switch effortLevel {
        case "trivial": return .blue
        case "small": return Color(nsColor: .systemGreen)
        case "medium": return Color(nsColor: .secondaryLabelColor)
        case "large": return .orange
        case "max": return .red
        default: return Color(nsColor: .secondaryLabelColor)
        }
    }
}

private struct ResizeDivider: NSViewRepresentable {
    let currentWidth: CGFloat
    let minWidth: CGFloat
    let maxWidth: CGFloat
    let onWidthChanged: (CGFloat) -> Void

    func makeNSView(context: Context) -> ResizeDividerView {
        let view = ResizeDividerView()
        view.minWidth = minWidth
        view.maxWidth = maxWidth
        view.currentWidth = currentWidth
        view.onWidthChanged = onWidthChanged
        return view
    }

    func updateNSView(_ nsView: ResizeDividerView, context: Context) {
        nsView.minWidth = minWidth
        nsView.maxWidth = maxWidth
        nsView.currentWidth = currentWidth
        nsView.onWidthChanged = onWidthChanged
    }
}

private class ResizeDividerView: NSView {
    var minWidth: CGFloat = 280
    var maxWidth: CGFloat = 600
    /// The Boss panel's current width, mirrored from the SwiftUI
    /// model. The drag math anchors on this at mouseDown — see the
    /// note in `mouseDown`.
    var currentWidth: CGFloat = 0
    var onWidthChanged: ((CGFloat) -> Void)?

    private var dragStartWidth: CGFloat = 0
    private var dragStartMouseX: CGFloat = 0
    private var isHovering = false
    private var isDragging = false

    /// X offset of the visible 1pt separator line within the view's
    /// bounds. The strip is anchored at the Boss pane's leading edge,
    /// so x = 0 is the boundary and the rest of the strip extends
    /// into the Boss pane as an invisible grip area.
    private let visibleLineX: CGFloat = 0

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        for area in trackingAreas {
            removeTrackingArea(area)
        }
        // `.cursorUpdate` is what actually drives the resize cursor.
        // The SwiftUI overlay hosts this NSView inside the detail pane
        // of a NavigationSplitView (NSSplitView under the hood), and
        // that container intercepts the AppKit cursor-rect machinery —
        // `resetCursorRects` / `addCursorRect` is not called reliably
        // for descendant views, so the cursor never flips on hover.
        // Routing cursor swaps through the tracking area's
        // `cursorUpdate(_:)` event bypasses that and works regardless
        // of the parent container.
        let area = NSTrackingArea(
            rect: bounds,
            options: [.mouseEnteredAndExited, .cursorUpdate, .activeInKeyWindow, .inVisibleRect],
            owner: self,
            userInfo: nil
        )
        addTrackingArea(area)
    }

    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)
        // Visible 1pt separator line at the boundary between the
        // worker grid and the Boss pane. The rest of the view bounds
        // is invisible grab strip — cursor + drag hit area, but not
        // painted.
        let lineX = visibleLineX
        NSColor.separatorColor.setFill()
        NSRect(x: lineX, y: 0, width: 1, height: bounds.height).fill()

        // Hover/active feedback: thicken and tint the line so the
        // user can see that the divider is grabbable / being dragged.
        // Drawn slightly inside the strip so it stays within bounds.
        if isDragging || isHovering {
            let alpha: CGFloat = isDragging ? 0.85 : 0.45
            NSColor.controlAccentColor.withAlphaComponent(alpha).setFill()
            NSRect(x: lineX, y: 0, width: 2, height: bounds.height).fill()
        }
    }

    /// Fires while the cursor is inside the tracking area. Setting
    /// the cursor here (instead of via `addCursorRect`) sidesteps the
    /// NSSplitView ancestor that would otherwise swallow cursor-rect
    /// management for descendant SwiftUI-hosted views. AppKit clears
    /// the cursor automatically when the pointer leaves the tracking
    /// area, so there's no stale-resize-cursor risk.
    override func cursorUpdate(with event: NSEvent) {
        NSCursor.resizeLeftRight.set()
    }

    override func mouseEntered(with event: NSEvent) {
        isHovering = true
        // Belt-and-suspenders: `cursorUpdate` is the primary path, but
        // setting on entry guarantees the swap fires on the first
        // hover even if the tracking area's initial `cursorUpdate`
        // hasn't been dispatched yet.
        NSCursor.resizeLeftRight.set()
        needsDisplay = true
    }

    override func mouseExited(with event: NSEvent) {
        isHovering = false
        // Restore the arrow on exit. Without this, the resize cursor
        // can linger on app-focus changes or when leaving via a route
        // that doesn't trigger another view's `cursorUpdate`.
        NSCursor.arrow.set()
        needsDisplay = true
    }

    override func mouseDown(with event: NSEvent) {
        // Anchor on the panel's width as reported by the SwiftUI model
        // rather than `superview.bounds.width` — the superview here is
        // the SwiftUI host of the (narrow) divider strip itself, not
        // the Boss panel. Using its width as the anchor produces a
        // tiny initial value (≈ the strip width) and clamps every
        // drag straight to `minWidth`, which is exactly the bug this
        // change fixes.
        dragStartWidth = currentWidth
        dragStartMouseX = event.locationInWindow.x
        isDragging = true
        needsDisplay = true
    }

    override func mouseDragged(with event: NSEvent) {
        let deltaX = event.locationInWindow.x - dragStartMouseX
        // The Boss panel sits on the trailing side of the window, so
        // dragging the divider right (positive deltaX) shrinks it.
        let newWidth = max(minWidth, min(maxWidth, dragStartWidth - deltaX))
        onWidthChanged?(newWidth)
    }

    override func mouseUp(with event: NSEvent) {
        isDragging = false
        needsDisplay = true
    }
}

/// Persistent, full-width strip pinned to the top of the window when
/// the engine socket can't be reached. Replaces the prior "Work Error"
/// modal that re-popped every dismissal during a reconnect storm (see
/// `ChatViewModel.handle` for the matching transport-error suppression
/// path).
///
/// Carries a "Restart engine" affordance so a stale or hung engine
/// process can be recovered without a shell `pkill` (issue #697). The
/// button drives `ChatViewModel.restartEngine()`, which terminates the
/// engine via the token-auth shutdown RPC (falling back to SIGTERM/
/// SIGKILL when the socket is dead) and relaunches it; the reconnect
/// loop picks the new socket up automatically.
private struct EngineUnreachableBanner: View {
    let isRestarting: Bool
    let onRestart: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.white)
            Text(headlineText)
                .font(.callout.weight(.semibold))
                .foregroundStyle(.white)
            Spacer(minLength: 0)
            Button(action: onRestart) {
                Text(isRestarting ? "Restarting…" : "Restart engine")
                    .font(.callout.weight(.semibold))
            }
            .buttonStyle(.bordered)
            .controlSize(.small)
            .tint(.white)
            .disabled(isRestarting)
            .help("Terminate the unresponsive engine and start a fresh one.")
            .accessibilityHint("Terminates the unresponsive engine and starts a fresh one.")
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity)
        .background(Color.red.opacity(0.85))
        .accessibilityElement(children: .contain)
    }

    private var headlineText: String {
        isRestarting
            ? "Restarting Boss engine…"
            : "Boss engine is unreachable — reconnecting…"
    }
}

/// Chrome-level banner surfacing engine-health issues: missing
/// `ANTHROPIC_API_KEY`, dispatch paused, `syspolicyd` wedged, and any
/// future issue the engine emits. Introduced after #699. The first
/// issue's title renders inline; the chevron expands all issues with
/// their remediation bodies.
private struct EngineHealthBanner: View {
    let issues: [EngineHealthIssue]
    @State private var isExpanded: Bool = false

    /// Highest severity in the issue list — drives banner color so a
    /// single `error` row escalates an otherwise-warning banner.
    private var effectiveSeverity: String {
        issues.contains(where: { $0.severity == "error" }) ? "error" : "warning"
    }

    private var background: Color {
        effectiveSeverity == "error"
            ? Color.red.opacity(0.85)
            : Color.orange.opacity(0.85)
    }

    private var iconName: String {
        effectiveSeverity == "error"
            ? "exclamationmark.octagon.fill"
            : "exclamationmark.triangle.fill"
    }

    var body: some View {
        VStack(spacing: 0) {
            Button(action: { withAnimation(.easeInOut(duration: 0.12)) { isExpanded.toggle() } }) {
                HStack(spacing: 8) {
                    Image(systemName: iconName)
                        .foregroundStyle(.white)
                    Text(headlineText)
                        .font(.callout.weight(.semibold))
                        .foregroundStyle(.white)
                        .lineLimit(1)
                        .truncationMode(.tail)
                    Spacer(minLength: 0)
                    Image(systemName: isExpanded ? "chevron.up" : "chevron.down")
                        .foregroundStyle(.white)
                        .font(.caption.weight(.semibold))
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 8)
                .frame(maxWidth: .infinity)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help(issues.first?.body ?? "")

            if isExpanded {
                VStack(alignment: .leading, spacing: 6) {
                    ForEach(issues) { issue in
                        VStack(alignment: .leading, spacing: 2) {
                            Text(issue.title)
                                .font(.callout.weight(.semibold))
                                .foregroundStyle(.white)
                            Text(issue.body)
                                .font(.caption)
                                .foregroundStyle(.white.opacity(0.92))
                                .fixedSize(horizontal: false, vertical: true)
                        }
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 14)
                .padding(.bottom, 10)
            }
        }
        .background(background)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(accessibilityLabel)
    }

    private var headlineText: String {
        if issues.count == 1 {
            return issues[0].title
        }
        let first = issues[0].title
        return "\(first) (\(issues.count - 1) more)"
    }

    private var accessibilityLabel: String {
        issues.map { "\($0.title). \($0.body)" }.joined(separator: " ")
    }
}

// MARK: - Update badge

/// Trailing toolbar button that appears when an update is available in Notify or Automatic mode.
/// Visibility is driven by `UpdateModel`; clicking opens a popover with version info and actions.
/// Notifications bell in the primary toolbar (attentions.md — App UI). Shows
/// a count badge for the selected product's open attention groups and opens
/// the singleton Attentions window. Mirrors the `.badge(openGroupCount)`
/// pattern with an overlay (`.badge` only applies inside List/TabView).
private struct NotificationsToolbarButton: View {
    @ObservedObject var model: ChatViewModel
    @Environment(\.openWindow) private var openWindow

    private var count: Int { model.openAttentionGroupCount }

    var body: some View {
        Button {
            openWindow(id: "attentions")
        } label: {
            Image(systemName: count > 0 ? "bell.badge" : "bell")
                .overlay(alignment: .topTrailing) {
                    if count > 0 {
                        Text(count > 99 ? "99+" : "\(count)")
                            .font(.system(size: 9, weight: .bold))
                            .foregroundStyle(.white)
                            .padding(.horizontal, 4)
                            .padding(.vertical, 1)
                            .background(Capsule().fill(Color.red))
                            .offset(x: 9, y: -7)
                            .fixedSize()
                    }
                }
        }
        .help(count > 0
              ? "\(count) notification\(count == 1 ? "" : "s") need your attention"
              : "Notifications")
    }
}

private struct UpdateBadgeToolbarButton: View {
    @ObservedObject var updateModel: UpdateModel
    @State private var isPopoverPresented = false

    var body: some View {
        if let update = visibleUpdate {
            Button {
                isPopoverPresented.toggle()
            } label: {
                Image(systemName: "arrow.down.circle.fill")
                    .foregroundStyle(Color.accentColor)
            }
            .help("Update available: Boss \(update.version)")
            .popover(isPresented: $isPopoverPresented, arrowEdge: .bottom) {
                UpdateBadgePopover(update: update, updateModel: updateModel) {
                    isPopoverPresented = false
                }
            }
        }
    }

    private var visibleUpdate: AvailableUpdate? {
        guard updateModel.mode != .manual,
              case .available(let update) = updateModel.lastCheckResult,
              update.version.description != updateModel.skippedVersion
        else { return nil }
        return update
    }
}

private struct UpdateBadgePopover: View {
    let update: AvailableUpdate
    @ObservedObject var updateModel: UpdateModel
    let onDismiss: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .center, spacing: 8) {
                Image(systemName: "arrow.down.circle.fill")
                    .foregroundStyle(Color.accentColor)
                    .font(.title3)
                VStack(alignment: .leading, spacing: 2) {
                    Text("Update Available")
                        .font(.headline)
                    Text("Boss \(update.version)")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                }
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 16)
            .padding(.top, 14)
            .padding(.bottom, 10)

            Divider()

            if !update.changelog.isEmpty || !update.releaseNotes.isEmpty {
                ScrollView {
                    ReleaseNotesContent(changelog: update.changelog, fallbackNotes: update.releaseNotes)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(16)
                }
                .frame(minHeight: 120, maxHeight: 320)

                Divider()
            }

            if let note = downloadStatusNote {
                Text(note)
                    .font(.caption)
                    .foregroundStyle(downloadFailed ? .orange : .secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.horizontal, 16)
                    .padding(.bottom, 8)
            }

            HStack(spacing: 8) {
                Button("Skip This Version") {
                    updateModel.skipCurrentVersion()
                    onDismiss()
                }
                .buttonStyle(.plain)
                .foregroundStyle(.secondary)
                .font(.callout)

                Spacer(minLength: 0)

                Button("Later") {
                    onDismiss()
                }
                .keyboardShortcut(.cancelAction)

                primaryActionButton
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 10)
        }
        .frame(minWidth: 300, maxWidth: 360)
    }

    /// Trailing call-to-action mirroring ``UpdateResultSheet``: dev builds keep the
    /// manual browser download; release builds stage the bundle in-app and then offer
    /// "Install & Relaunch".
    @ViewBuilder
    private var primaryActionButton: some View {
        if updateModel.isDevBuild {
            Button("Download") {
                NSWorkspace.shared.open(releasePageURL ?? update.assetURL)
                onDismiss()
            }
            .keyboardShortcut(.defaultAction)
        } else {
            switch updateModel.downloadState {
            case .downloading(let v, _) where v == update.version:
                Button {
                } label: {
                    HStack(spacing: 6) {
                        ProgressView().controlSize(.small)
                        Text("Downloading…")
                    }
                }
                .disabled(true)

            case .readyToInstall(let v) where v == update.version:
                Button("Install & Relaunch") {
                    if UpdateLifecycle.installStagedAndRelaunch() {
                        NSApplication.shared.terminate(nil)
                    } else {
                        NSWorkspace.shared.open(releasePageURL ?? update.assetURL)
                    }
                    onDismiss()
                }
                .keyboardShortcut(.defaultAction)

            case .failed(let v, _) where v == update.version:
                Button("Retry Download") {
                    updateModel.downloadAvailableUpdate()
                }
                .keyboardShortcut(.defaultAction)

            default:
                Button("Download") {
                    updateModel.downloadAvailableUpdate()
                }
                .keyboardShortcut(.defaultAction)
            }
        }
    }

    private var downloadStatusNote: String? {
        switch updateModel.downloadState {
        case .downloading(let v, let fraction) where v == update.version:
            let pct = Int((fraction * 100).rounded())
            return pct > 0 ? "Downloading… \(pct)%" : "Downloading…"
        case .readyToInstall(let v) where v == update.version:
            return "Downloaded and verified. Install & Relaunch to apply."
        case .failed(let v, let reason) where v == update.version:
            return "Download failed: \(reason)"
        default:
            return nil
        }
    }

    private var downloadFailed: Bool {
        if case .failed(let v, _) = updateModel.downloadState, v == update.version { return true }
        return false
    }

    private var releasePageURL: URL? {
        URL(string: "https://github.com/spinyfin/mono/releases/tag/\(update.tagName)")
    }
}
