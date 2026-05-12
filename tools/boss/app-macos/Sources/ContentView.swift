import AppKit
import SwiftUI

private let workBoardColumnWidth: CGFloat = 280
private let workBoardColumnSpacing: CGFloat = 12
private let workBoardHorizontalPadding: CGFloat = 20
private let workBossPanelDefaultExpandedWidth: CGFloat = 380
private let workBossPanelMinWidth: CGFloat = 280
private let workBossPanelMaxWidth: CGFloat = 600
private let workBossPanelCollapsedWidth: CGFloat = 88
private let workBossPanelDividerHitWidth: CGFloat = 12

struct ContentView: View {
    @StateObject private var model = ChatViewModel()
    #if canImport(GhosttyKit)
    @StateObject private var workersWorkspace = WorkersWorkspaceModel()
    @StateObject private var bossPane = BossPaneModel()
    #endif

    var body: some View {
        // Both modes are rendered simultaneously and toggled via opacity +
        // hit-testing. SwiftUI's structural `if`/`else` would tear down the
        // libghostty NSViews on every Agents↔Work switch, which would force
        // `ghostty_surface_new` and restart every claude session.
        ZStack {
            NavigationSplitView {
                sidebar
            } detail: {
                detail
            }
            .opacity(model.navigationMode == .work ? 1 : 0)
            .allowsHitTesting(model.navigationMode == .work)

            agentsView
                .opacity(model.navigationMode == .agents ? 1 : 0)
                .allowsHitTesting(model.navigationMode == .agents)

            DesignsView(chat: model)
                .opacity(model.navigationMode == .designs ? 1 : 0)
                .allowsHitTesting(model.navigationMode == .designs)
        }
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
        }
        #endif
        .frame(minWidth: 860, minHeight: 560)
        .task {
            model.startIfNeeded()
        }
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Picker("Mode", selection: Binding(
                    get: { model.navigationMode },
                    set: { model.setNavigationMode($0) }
                )) {
                    ForEach(NavigationMode.allCases) { mode in
                        Text(mode.rawValue).tag(mode)
                    }
                }
                .pickerStyle(.segmented)
                .frame(width: 260)
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
                onCancel: { model.dismissWorkCreateRequest() },
                onCreate: { name, description, repoRemoteURL, goal in
                    model.submitWorkCreateRequest(
                        request,
                        name: name,
                        description: description,
                        repoRemoteURL: repoRemoteURL,
                        goal: goal
                    )
                }
            )
        }
        .sheet(item: $model.pendingWorkEditRequest) { request in
            WorkEditSheet(
                request: request,
                onCancel: { model.dismissWorkEditRequest() },
                onSave: { name, description, status, repoRemoteURL, goal, priority, prURL in
                    model.submitWorkEditRequest(
                        request,
                        name: name,
                        description: description,
                        status: status,
                        repoRemoteURL: repoRemoteURL,
                        goal: goal,
                        priority: priority,
                        prURL: prURL
                    )
                }
            )
        }
    }

    private var sidebar: some View {
        workSidebar
            .navigationSplitViewColumnWidth(min: 220, ideal: 280, max: 360)
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

                    ForEach(model.projectsForSelectedProduct) { project in
                        let isOn = model.selectedProjectFilterIDs.contains(project.id)
                        let isArchived = project.status == "archived"
                        WorkSidebarFilterRow(
                            title: project.name,
                            subtitle: nil,
                            systemImage: isArchived ? "archivebox" : "folder",
                            isSelected: isOn,
                            trailing: project.status.capitalized,
                            showsCheckbox: true,
                            isCheckboxOn: isOn,
                            dimmed: isArchived
                        )
                        .listRowInsets(EdgeInsets(top: 3, leading: 8, bottom: 3, trailing: 8))
                        .listRowBackground(Color.clear)
                        .contentShape(Rectangle())
                        .onTapGesture {
                            model.toggleProjectFilter(project.id)
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
        .searchable(text: $model.workSearchText, placement: .sidebar, prompt: "Filter board")
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
            .padding(.vertical, 8)
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
            } else if let product = model.selectedProduct {
                workBoard(product: product)
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
            ZStack {
                Circle()
                    .fill(Color.accentColor.opacity(0.14))
                if let portrait = TrekIconAssets.image(.picard, size: .small) {
                    // The sprite is taller than it is wide (head + torso),
                    // so an aspect-fill into a square frame overflows
                    // vertically. Anchor the overflow to the top edge so
                    // the head is preserved and only the lower body — which
                    // the Circle mask would hide anyway — gets clipped.
                    // Without `.top`, SwiftUI centers the overflow and
                    // slices the bald crown off Picard.
                    Image(nsImage: portrait)
                        .resizable()
                        .interpolation(.high)
                        .aspectRatio(contentMode: .fill)
                        .frame(width: 26, height: 26, alignment: .top)
                        .clipShape(Circle())
                } else {
                    Image(systemName: "person.crop.circle.badge.checkmark")
                        .foregroundStyle(Color.accentColor)
                        .font(.system(size: 13, weight: .semibold))
                }
            }
            .frame(width: 26, height: 26)

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

    private func workBoard(product: WorkProduct) -> some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(alignment: .top) {
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    Text(product.name)
                        .font(.title2.weight(.semibold))
                    if case .singleRepo(let url) = model.workBoardRepoMode {
                        RepoChipView(
                            presentation: RepoChipPresentation.forProductHeader(
                                productRepoURL: url
                            )
                        )
                    }
                    Text(model.projectFilterDescription)
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.tail)
                }
                Spacer()
                Picker(
                    "Group",
                    selection: Binding(
                        get: { model.workBoardGrouping },
                        set: { model.setWorkBoardGrouping($0) }
                    )
                ) {
                    ForEach(WorkBoardGrouping.allCases) { grouping in
                        Text(grouping.title).tag(grouping)
                    }
                }
                .pickerStyle(.segmented)
                .frame(width: 220)
            }
            .padding(.horizontal, workBoardHorizontalPadding)
            .padding(.top, 20)

            NativeWorkBoardScrollView(
                columns: WorkBoardColumnKey.allCases.map { column in
                    NativeWorkBoardColumn(
                        id: column.id,
                        view: AnyView(workColumn(column))
                    )
                }
            )
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }

    private func workColumn(_ column: WorkBoardColumnKey) -> some View {
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
                ScrollView(.vertical) {
                    VStack(alignment: .leading, spacing: 12) {
                        ForEach(sections) { section in
                            workSectionView(section, column: column)
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .topLeading)
                }
                .frame(maxHeight: .infinity)
            }
        }
        .padding(14)
        .frame(width: workBoardColumnWidth, alignment: .topLeading)
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
                defaultExpanded: section.defaultExpanded
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
        VStack(alignment: .leading, spacing: 10) {
            ForEach(items) { task in
                WorkBoardCardItem(
                    task: task,
                    projectName: model.cardProjectBadge(for: task),
                    column: column,
                    runtime: column == .doing ? model.taskRuntime(for: task.id) : nil,
                    isSelected: model.selectedTask?.id == task.id,
                    model: model,
                    liveStates: model.liveWorkerStates
                )
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
            VStack(alignment: .leading, spacing: subtitle == nil ? 0 : 2) {
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
                }
                .frame(maxWidth: .infinity, alignment: .leading)

                if let subtitle, !subtitle.isEmpty {
                    Text(subtitle)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.leading, 8)
        .padding(.trailing, 4)
        .padding(.vertical, subtitle == nil ? 6 : 7)
        .contentShape(Rectangle())
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

struct SidebarProductPicker: NSViewRepresentable {
    @Binding var selection: String?
    let products: [WorkProduct]

    func makeCoordinator() -> Coordinator {
        Coordinator(selection: $selection)
    }

    func makeNSView(context: Context) -> NSPopUpButton {
        let button = NSPopUpButton(frame: .zero, pullsDown: false)
        button.bezelStyle = .rounded
        button.target = context.coordinator
        button.action = #selector(Coordinator.selectionDidChange(_:))
        button.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        return button
    }

    func updateNSView(_ nsView: NSPopUpButton, context: Context) {
        context.coordinator.selection = $selection
        context.coordinator.productIDs = products.map(\.id)

        nsView.removeAllItems()
        nsView.addItems(withTitles: products.map(\.name))

        for (index, productID) in context.coordinator.productIDs.enumerated() {
            nsView.item(at: index)?.representedObject = productID
        }

        let selectedID = selection ?? context.coordinator.productIDs.first
        if let selectedID,
           let index = context.coordinator.productIDs.firstIndex(of: selectedID) {
            nsView.selectItem(at: index)
        }
    }

    final class Coordinator: NSObject {
        var selection: Binding<String?>
        var productIDs: [String] = []

        init(selection: Binding<String?>) {
            self.selection = selection
        }

        @objc func selectionDidChange(_ sender: NSPopUpButton) {
            let index = sender.indexOfSelectedItem
            guard productIDs.indices.contains(index) else { return }
            let selectedID = productIDs[index]
            if selection.wrappedValue != selectedID {
                selection.wrappedValue = selectedID
            }
        }
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
    @ObservedObject var model: ChatViewModel
    @ObservedObject var liveStates: LiveWorkerStateStore

    var body: some View {
        let liveState: WorkerLiveState? = {
            guard column == .doing,
                  let executionID = runtime?.executionID
            else { return nil }
            return liveStates.byRunID[executionID]
        }()

        let blockedBy: String? = task.status == "blocked"
            ? model.blockedByLabel(for: task)
            : nil

        let gatingPrereqs = model.gatingPrereqs(for: task.id)
        let isAutoBlocked = model.isAutoBlocked(task)
        let dragRefusal: String? = (model.dragRefusalNotice?.taskID == task.id)
            ? model.dragRefusalNotice?.message
            : nil
        let repoChip = model.repoChip(for: task)

        VStack(alignment: .leading, spacing: 6) {
            Button {
                model.selectWorkCard(isSelected ? nil : task.id)
            } label: {
                WorkBoardCardView(
                    task: task,
                    projectName: projectName,
                    isSelected: isSelected,
                    activityState: column == .doing
                        ? AgentActivityState(runtime: runtime, liveState: liveState)
                        : nil,
                    assignedSlotId: column == .doing ? liveState?.slotId : nil,
                    liveStatus: column == .doing ? liveState?.liveStatus : nil,
                    liveStatusActivity: column == .doing ? liveState?.activity : nil,
                    blockedBy: blockedBy,
                    isAutoBlocked: isAutoBlocked,
                    gatingPrereqs: gatingPrereqs,
                    repoChip: repoChip
                )
            }
            .buttonStyle(.plain)
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
    /// Activity of the worker behind `liveStatus`, used to tint the
    /// subtitle row per design Q4: `WaitingForInput` reads in the
    /// accent colour to match the "needs human" pill, `Errored` reads
    /// in red, `Idle` dims further than `.secondary`. The default
    /// `nil` is treated as the plain `.secondary` colour.
    var liveStatusActivity: WorkerActivity? = nil
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

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
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
                        if task.status == "blocked" {
                            Image(systemName: "lock.fill")
                                .font(.caption)
                                .foregroundStyle(.orange)
                                .accessibilityLabel("Blocked")
                        }
                        if let repoChip {
                            RepoChipView(presentation: repoChip)
                        }
                        Text(task.name)
                            .font(.body.weight(.medium))
                            .foregroundStyle(.primary)
                            .multilineTextAlignment(.leading)
                    }
                    if let blockedBy, !blockedBy.isEmpty {
                        Text("Blocked by \(blockedBy)")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(2)
                            .help("Blocked by \(blockedBy)")
                    }
                }
                Spacer(minLength: 0)
            }

            if let liveStatus, !liveStatus.isEmpty {
                Text(liveStatus)
                    .font(.caption)
                    .foregroundStyle(liveStatusColor)
                    .lineLimit(2)
                    .truncationMode(.tail)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .help(liveStatus)
                    .accessibilityLabel("Live status: \(liveStatus)")
            }

            if hasFooterContent {
                HStack {
                    PriorityChip(priority: WorkPriority.parse(task.priority))
                    if let projectName, !projectName.isEmpty {
                        WorkStatusBadge(text: projectName)
                    }
                    if task.status == "blocked" {
                        WorkStatusBadge(text: "Blocked")
                    }
                    if isAutoBlocked {
                        Image(systemName: "link")
                            .font(.caption2.weight(.semibold))
                            .foregroundStyle(.orange)
                            .help(autoBlockTooltip)
                            .accessibilityLabel("Auto-blocked by dependencies")
                            .accessibilityValue(autoBlockTooltip)
                    }
                    Spacer()
                }
            }

            if let prURL = task.prURL, !prURL.isEmpty {
                PRURLLink(urlString: prURL, font: .caption)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .fill(cardBackground)
                .overlay(
                    RoundedRectangle(cornerRadius: 12, style: .continuous)
                        .strokeBorder(borderColor, lineWidth: isSelected ? 2 : 1)
                )
        )
        .draggable(task.id)
    }

    /// The footer renders the priority chip on every card so a glance
    /// at the board immediately separates `[HIGH]` work from the rest
    /// without authors having to prefix names. The other footer
    /// elements (project tag, blocked tag) appear conditionally.
    private var hasFooterContent: Bool {
        true
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

    /// Tint for the live-status subtitle row. Q4 of the design pairs
    /// the colour with the activity dot: red for errored runs, the
    /// accent colour when the worker is waiting on a human, a dimmer
    /// grey when the worker is idle, and the normal `.secondary` grey
    /// while the worker is actively working.
    private var liveStatusColor: Color {
        switch liveStatusActivity {
        case .errored:
            return .red
        case .waitingForInput:
            return .accentColor
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
        if task.status == "blocked" {
            return Color.orange.opacity(0.08)
        }
        return Color(nsColor: .windowBackgroundColor)
    }

    private var borderColor: Color {
        if isSelected {
            return .accentColor
        }
        if task.status == "blocked" {
            return .orange
        }
        return Color(nsColor: .separatorColor)
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
        case .resolved(let resolved, _, _):
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
        Circle()
            .fill(fillColor)
            .frame(width: 7, height: 7)
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
        }
    }
}

private struct WorkCardPopoverView: View {
    @ObservedObject var model: ChatViewModel
    let task: WorkTask

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(alignment: .top, spacing: 12) {
                VStack(alignment: .leading, spacing: 6) {
                    Text(task.name)
                        .font(.title3.weight(.semibold))
                    Text(task.isChore ? "Chore" : "Task")
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
                priorityRow
                if let ordinal = task.ordinal, !task.isChore {
                    metadataRow("Phase", value: "\(ordinal)")
                }
                metadataPRRow(prURL: task.prURL)
                sourceChipRow
            }

            WorkDependenciesSection(model: model, taskID: task.id)

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
                    MarkdownViewerWindowController.shared.present(
                        title: task.name,
                        markdown: task.description
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

    var body: some View {
        let label = shortLabel(for: urlString) ?? urlString
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
            .help(urlString)
            .onHover { hovering in
                if hovering {
                    NSCursor.pointingHand.push()
                } else {
                    NSCursor.pop()
                }
            }
        } else {
            Text(label)
                .font(font)
                .foregroundStyle(.secondary)
                .lineLimit(1)
        }
    }

    private func shortLabel(for urlString: String) -> String? {
        guard let url = URL(string: urlString),
              let host = url.host?.lowercased(),
              host == "github.com" || host == "www.github.com"
        else {
            return nil
        }
        let parts = url.path.split(separator: "/", omittingEmptySubsequences: true).map(String.init)
        guard parts.count == 4,
              parts[2] == "pull",
              !parts[0].isEmpty,
              !parts[1].isEmpty,
              !parts[3].isEmpty,
              parts[3].allSatisfy(\.isNumber)
        else {
            return nil
        }
        return "\(parts[0])/\(parts[1])#\(parts[3])"
    }
}

private struct WorkCreateSheet: View {
    let request: WorkCreateRequest
    let onCancel: () -> Void
    let onCreate: (String, String, String, String) -> Void

    @State private var name = ""
    @State private var description = ""
    @State private var repoRemoteURL = ""
    @State private var goal = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(title)
                .font(.title3.weight(.semibold))

            TextField("Name", text: $name)

            switch request.kind {
            case .product:
                TextField("Description", text: $description)
                VStack(alignment: .leading, spacing: 4) {
                    TextField(
                        ProductRepoFieldCopy.placeholder,
                        text: $repoRemoteURL
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
            }

            HStack {
                Spacer()
                Button("Cancel", action: onCancel)
                Button("Create") {
                    onCreate(name, description, repoRemoteURL, goal)
                }
                .keyboardShortcut(.defaultAction)
                .disabled(name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(20)
        .frame(width: 420)
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

private struct WorkEditSheet: View {
    let request: WorkEditRequest
    let onCancel: () -> Void
    let onSave: (String, String, String, String, String, String, String) -> Void

    @State private var name: String
    @State private var description: String
    @State private var status: String
    @State private var repoRemoteURL: String
    @State private var goal: String
    @State private var priority: String
    @State private var prURL: String

    init(
        request: WorkEditRequest,
        onCancel: @escaping () -> Void,
        onSave: @escaping (String, String, String, String, String, String, String) -> Void
    ) {
        self.request = request
        self.onCancel = onCancel
        self.onSave = onSave

        switch request.item {
        case .product(let product):
            _name = State(initialValue: product.name)
            _description = State(initialValue: product.description)
            _status = State(initialValue: product.status)
            _repoRemoteURL = State(initialValue: product.repoRemoteURL ?? "")
            _goal = State(initialValue: "")
            _priority = State(initialValue: "")
            _prURL = State(initialValue: "")
        case .project(let project):
            _name = State(initialValue: project.name)
            _description = State(initialValue: project.description)
            _status = State(initialValue: project.status)
            _repoRemoteURL = State(initialValue: "")
            _goal = State(initialValue: project.goal)
            _priority = State(initialValue: project.priority)
            _prURL = State(initialValue: "")
        case .task(let task), .chore(let task):
            _name = State(initialValue: task.name)
            _description = State(initialValue: task.description)
            _status = State(initialValue: task.status)
            _repoRemoteURL = State(initialValue: "")
            _goal = State(initialValue: "")
            _priority = State(initialValue: task.priority)
            _prURL = State(initialValue: task.prURL ?? "")
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(title)
                .font(.title3.weight(.semibold))

            TextField("Name", text: $name)
            TextField("Description", text: $description)

            switch request.item {
            case .product:
                Picker("Status", selection: $status) {
                    ForEach(["active", "paused", "archived"], id: \.self) { status in
                        Text(status.capitalized).tag(status)
                    }
                }
                TextField("Remote URL", text: $repoRemoteURL)
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
            }

            HStack {
                Spacer()
                Button("Cancel", action: onCancel)
                Button("Save") {
                    onSave(name, description, status, repoRemoteURL, goal, priority, prURL)
                }
                .keyboardShortcut(.defaultAction)
                .disabled(name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(20)
        .frame(width: 440)
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

/// Capsule chip surfacing a repo's short name on a kanban card or
/// product header. Hover tooltip carries the full URL plus the
/// provenance string ("Inherited from product" vs "Override on this
/// card") so the reader can tell where the URL came from without
/// digging into the popover. Pure view — all the mode/provenance
/// logic lives on `RepoChipPresentation` so tests don't need a
/// SwiftUI host.
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
        .foregroundStyle(foregroundColor)
        .padding(.horizontal, 7)
        .padding(.vertical, 3)
        .background(backgroundColor)
        .clipShape(Capsule())
        .overlay(
            Capsule().strokeBorder(borderColor, lineWidth: 0.5)
        )
        .help(presentation.tooltip)
        .accessibilityLabel(presentation.accessibilityLabel)
    }

    private var foregroundColor: Color {
        switch presentation.provenance {
        case .productDefault:
            return Color(nsColor: .secondaryLabelColor)
        case .taskOverride:
            return .accentColor
        }
    }

    private var backgroundColor: Color {
        switch presentation.provenance {
        case .productDefault:
            return Color(nsColor: .controlBackgroundColor)
        case .taskOverride:
            return Color.accentColor.opacity(0.12)
        }
    }

    private var borderColor: Color {
        switch presentation.provenance {
        case .productDefault:
            return Color(nsColor: .separatorColor)
        case .taskOverride:
            return Color.accentColor.opacity(0.4)
        }
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

private struct NativeWorkBoardColumn: Identifiable {
    let id: String
    let view: AnyView
}

private struct NativeWorkBoardScrollView: NSViewRepresentable {
    let columns: [NativeWorkBoardColumn]
    private let columnWidth: CGFloat = workBoardColumnWidth
    private let spacing: CGFloat = workBoardColumnSpacing
    private let horizontalPadding: CGFloat = workBoardHorizontalPadding

    func makeCoordinator() -> Coordinator {
        Coordinator(
            columnWidth: columnWidth,
            spacing: spacing,
            horizontalPadding: horizontalPadding
        )
    }

    func makeNSView(context: Context) -> NSScrollView {
        let scrollView = WorkBoardScrollView()
        let clipView = HorizontalOnlyClipView()
        clipView.drawsBackground = false

        scrollView.drawsBackground = false
        scrollView.borderType = .noBorder
        scrollView.hasHorizontalScroller = true
        scrollView.hasVerticalScroller = false
        scrollView.autohidesScrollers = true
        scrollView.horizontalScrollElasticity = .automatic
        scrollView.verticalScrollElasticity = .none
        scrollView.contentView = clipView
        scrollView.documentView = context.coordinator.documentView

        let coordinator = context.coordinator
        scrollView.onLayout = { [weak scrollView] in
            guard let scrollView else { return }
            coordinator.layoutColumns(in: scrollView)
        }
        return scrollView
    }

    func updateNSView(_ nsView: NSScrollView, context: Context) {
        let coordinator = context.coordinator
        if nsView.documentView !== coordinator.documentView {
            nsView.documentView = coordinator.documentView
        }

        coordinator.sync(columns: columns)
        coordinator.layoutColumns(in: nsView)
    }

    @MainActor
    final class Coordinator {
        let documentView = FlippedContentView()
        var hostingViews: [NSHostingView<AnyView>] = []

        private let columnWidth: CGFloat
        private let spacing: CGFloat
        private let horizontalPadding: CGFloat
        private var isLayingOut = false

        init(columnWidth: CGFloat, spacing: CGFloat, horizontalPadding: CGFloat) {
            self.columnWidth = columnWidth
            self.spacing = spacing
            self.horizontalPadding = horizontalPadding
        }

        func sync(columns: [NativeWorkBoardColumn]) {
            while hostingViews.count > columns.count {
                hostingViews.removeLast().removeFromSuperview()
            }

            while hostingViews.count < columns.count {
                let hostingView = NSHostingView(rootView: AnyView(EmptyView()))
                documentView.addSubview(hostingView)
                hostingViews.append(hostingView)
            }

            for (hostingView, column) in zip(hostingViews, columns) {
                hostingView.rootView = column.view
            }
        }

        func layoutColumns(in scrollView: NSScrollView) {
            guard !isLayingOut else { return }
            isLayingOut = true
            defer { isLayingOut = false }

            var clipWidth = scrollView.contentView.bounds.width
            var clipHeight = scrollView.contentView.bounds.height
            let contentWidth = totalContentWidth(for: hostingViews.count)
            let hasOverflow = contentWidth > clipWidth + 0.5
            if scrollView.hasHorizontalScroller != hasOverflow {
                scrollView.hasHorizontalScroller = hasOverflow
                scrollView.tile()
                clipWidth = scrollView.contentView.bounds.width
                clipHeight = scrollView.contentView.bounds.height
            }

            documentView.frame = NSRect(
                origin: .zero,
                size: NSSize(width: max(contentWidth, clipWidth), height: clipHeight)
            )

            var x = horizontalPadding
            for hostingView in hostingViews {
                hostingView.frame = NSRect(
                    x: x,
                    y: 0,
                    width: columnWidth,
                    height: clipHeight
                )
                x += columnWidth + spacing
            }

            // The board only scrolls horizontally. Clamp any stale vertical offset
            // back to zero so project/filter changes can't hide the column headers.
            let currentOrigin = scrollView.contentView.bounds.origin
            let maxHorizontalOffset = max(0, documentView.frame.width - clipWidth)
            let clampedOrigin = NSPoint(
                x: min(max(currentOrigin.x, 0), maxHorizontalOffset),
                y: 0
            )
            if abs(currentOrigin.x - clampedOrigin.x) > 0.5
                || abs(currentOrigin.y - clampedOrigin.y) > 0.5
            {
                scrollView.contentView.scroll(to: clampedOrigin)
                scrollView.reflectScrolledClipView(scrollView.contentView)
            }
        }

        private func totalContentWidth(for columnCount: Int) -> CGFloat {
            guard columnCount > 0 else { return 0 }
            return horizontalPadding
                + (CGFloat(columnCount) * columnWidth)
                + (CGFloat(max(columnCount - 1, 0)) * spacing)
                + horizontalPadding
        }
    }
}

/// NSScrollView subclass that calls back on every geometry change so the
/// embedded column hosting views can be re-laid out. Without this the lanes
/// stay at the height they had when SwiftUI last sent a state update, so
/// resizing the window vertically would not grow the lanes.
private final class WorkBoardScrollView: NSScrollView {
    var onLayout: (() -> Void)?

    override func tile() {
        super.tile()
        onLayout?()
    }
}

private final class FlippedContentView: NSView {
    override var isFlipped: Bool { true }
}

private final class HorizontalOnlyClipView: NSClipView {
    override func constrainBoundsRect(_ proposedBounds: NSRect) -> NSRect {
        var constrained = super.constrainBoundsRect(proposedBounds)
        constrained.origin.y = 0
        return constrained
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
        let area = NSTrackingArea(
            rect: bounds,
            options: [.mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
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

    /// AppKit calls this whenever cursor rects need to be reset.
    /// Using `addCursorRect` instead of `NSCursor.push/pop` so the
    /// system manages cursor swapping — no stale resize cursor
    /// surviving a layout change or window-key transition (the
    /// "cursor stuck after the agent finished" symptom).
    override func resetCursorRects() {
        discardCursorRects()
        addCursorRect(bounds, cursor: .resizeLeftRight)
    }

    override func mouseEntered(with event: NSEvent) {
        isHovering = true
        needsDisplay = true
    }

    override func mouseExited(with event: NSEvent) {
        isHovering = false
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
