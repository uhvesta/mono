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
                .frame(width: 170)
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
        .alert(item: $model.pendingPermission) { request in
            Alert(
                title: Text("Permission Request"),
                message: Text(request.title),
                primaryButton: .default(Text("Allow")) {
                    model.respondToPendingPermission(granted: true)
                },
                secondaryButton: .destructive(Text("Deny")) {
                    model.respondToPendingPermission(granted: false)
                }
            )
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
        #if canImport(GhosttyKit)
        WorkersDetailView(workspace: workersWorkspace, liveStates: model.liveWorkerStates)
            .background(Color(nsColor: .windowBackgroundColor))
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
        .background(Color(nsColor: .windowBackgroundColor))
        #endif
    }

    private var workSidebar: some View {
        List {
            if !model.products.isEmpty {
                Section {
                    ZStack(alignment: .trailing) {
                        SidebarProductPicker(
                            selection: workProductSelection,
                            products: model.products
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
                        WorkSidebarFilterRow(
                            title: project.name,
                            subtitle: nil,
                            systemImage: "folder",
                            isSelected: isOn,
                            trailing: project.status.capitalized,
                            showsCheckbox: true,
                            isCheckboxOn: isOn
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
                model.selectedProduct?.id ?? model.products.first?.id
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
            if model.products.isEmpty {
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

    private func composer(
        draft: Binding<String>,
        agentID: String?,
        isReady: Bool,
        isSending: Bool,
        autoFocus: Bool,
        focusTrigger: String?,
        onSend: @escaping () -> Void
    ) -> some View {
        let isDraftEmpty = draft.wrappedValue.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        let canSend = agentID != nil && !isDraftEmpty && !isSending && isReady

        return VStack(spacing: 0) {
            HStack(alignment: .center, spacing: 10) {
                ComposerTextView(
                    text: draft,
                    placeholder: isReady ? "Type a message…" : "Agent starting…",
                    autoFocus: autoFocus,
                    focusTrigger: focusTrigger,
                    onSubmit: onSend
                )
                .frame(height: 36)
                .frame(maxWidth: .infinity)

                Button(action: onSend) {
                    Image(systemName: "paperplane.fill")
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(canSend ? .primary : .secondary)
                        .frame(width: 18, height: 18)
                }
                .buttonStyle(.plain)
                .keyboardShortcut(.return, modifiers: [.command])
                .disabled(!canSend)
                .help("Send")
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .background(Color(nsColor: .controlBackgroundColor))
            .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .stroke(Color(nsColor: .separatorColor), lineWidth: 1)
            )
            .padding(.horizontal, 16)
            .padding(.bottom, 12)
            .padding(.top, 4)

            if isSending {
                HStack(spacing: 6) {
                    ProgressView()
                        .controlSize(.mini)
                    Text("Working…")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Spacer()
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 8)
            }
        }
    }

    private var workBossPanel: some View {
        let isCollapsed = model.isBossPanelCollapsed

        return VStack(spacing: 0) {
            bossAgentHeader(isCollapsed: isCollapsed)

            if isCollapsed {
                Spacer(minLength: 0)
                Text("Picard")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .rotationEffect(.degrees(-90))
                Spacer(minLength: 0)
            } else {
                #if canImport(GhosttyKit)
                BossPaneTerminalView(boss: bossPane)
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
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
                #endif
            }
        }
        .frame(width: isCollapsed ? workBossPanelCollapsedWidth : model.bossPanelWidth)
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
                    Image(systemName: AgentRole.boss.systemImage)
                        .foregroundStyle(Color.accentColor)
                        .font(.system(size: 13, weight: .semibold))
                }
            }
            .frame(width: 26, height: 26)

            if !isCollapsed {
                Text(AgentRole.boss.title)
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
            model.moveTask(taskID, to: column)
            return true
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
                    Text(section.title)
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
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
                    projectName: task.isChore ? nil : model.projectName(for: task.projectID),
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

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            if showsCheckbox {
                Image(systemName: isCheckboxOn ? "checkmark.square.fill" : "square")
                    .foregroundStyle(isCheckboxOn ? Color.accentColor : .secondary)
                    .font(.system(size: 14, weight: .medium))
                    .frame(width: 15, alignment: .center)
                    .padding(.top, 2)
            } else {
                Image(systemName: systemImage)
                    .foregroundStyle(isSelected ? .primary : .secondary)
                    .font(.system(size: 14, weight: .medium))
                    .frame(width: 15, alignment: .center)
                    .padding(.top, 2)
            }
            VStack(alignment: .leading, spacing: subtitle == nil ? 0 : 2) {
                HStack(alignment: .top, spacing: 8) {
                    Text(title)
                        .font(.body.weight(isSelected ? .semibold : .regular))
                        .foregroundStyle(.primary)
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

private struct SidebarProductPicker: NSViewRepresentable {
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
                assignedSlotId: column == .doing ? liveState?.slotId : nil
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
    }
}

private struct WorkBoardCardView: View {
    let task: WorkTask
    let projectName: String?
    let isSelected: Bool
    let activityState: AgentActivityState?
    /// Slot id of the worker currently bound to this card, when the
    /// card lives in the Doing lane. Drives the small crew portrait
    /// in the title row so a glance at the board tells you which
    /// crew member is on which task.
    let assignedSlotId: Int?

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
                Text(task.name)
                    .font(.body.weight(.medium))
                    .foregroundStyle(.primary)
                    .multilineTextAlignment(.leading)
                Spacer(minLength: 0)
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
                Text(task.description)
                    .fixedSize(horizontal: false, vertical: true)
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
            }

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
                TextField("Remote URL", text: $repoRemoteURL)
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

private struct ComposerTextView: NSViewRepresentable {
    @Binding var text: String
    let placeholder: String
    let autoFocus: Bool
    var focusTrigger: String?
    let onSubmit: () -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(parent: self)
    }

    func makeNSView(context: Context) -> NSScrollView {
        let scrollView = NSScrollView()
        scrollView.drawsBackground = false
        scrollView.borderType = .noBorder
        scrollView.hasVerticalScroller = true
        scrollView.autohidesScrollers = true
        scrollView.scrollerStyle = .overlay

        let textView = ComposerNSTextView()
        textView.delegate = context.coordinator
        textView.isEditable = true
        textView.isSelectable = true
        textView.isRichText = false
        textView.importsGraphics = false
        textView.allowsUndo = true
        textView.font = .preferredFont(forTextStyle: .body)
        textView.textColor = .labelColor
        textView.backgroundColor = .clear
        textView.drawsBackground = false
        textView.focusRingType = .none
        textView.textContainer?.lineFragmentPadding = 0
        textView.isHorizontallyResizable = false
        textView.isVerticallyResizable = true
        textView.autoresizingMask = [.width]
        textView.maxSize = NSSize(
            width: CGFloat.greatestFiniteMagnitude,
            height: CGFloat.greatestFiniteMagnitude
        )
        textView.minSize = NSSize(width: 0, height: 0)
        textView.textContainer?.widthTracksTextView = true
        textView.submitHandler = onSubmit
        textView.placeholder = placeholder
        textView.string = text

        scrollView.documentView = textView
        context.coordinator.textView = textView
        context.coordinator.didAutoFocus = false
        return scrollView
    }

    func updateNSView(_ nsView: NSScrollView, context: Context) {
        context.coordinator.parent = self
        guard let textView = context.coordinator.textView else {
            return
        }

        textView.submitHandler = onSubmit
        textView.placeholder = placeholder
        if textView.string != text {
            textView.string = text
            textView.needsDisplay = true
        }

        let shouldFocus: Bool
        if !context.coordinator.didAutoFocus, autoFocus {
            context.coordinator.didAutoFocus = true
            shouldFocus = true
        } else if focusTrigger != context.coordinator.lastFocusTrigger {
            context.coordinator.lastFocusTrigger = focusTrigger
            shouldFocus = true
        } else {
            shouldFocus = false
        }

        if shouldFocus {
            DispatchQueue.main.async {
                guard let window = textView.window else {
                    return
                }
                window.makeFirstResponder(textView)
            }
        }
    }

    final class Coordinator: NSObject, NSTextViewDelegate {
        var parent: ComposerTextView
        weak var textView: ComposerNSTextView?
        var didAutoFocus = false
        var lastFocusTrigger: String?

        init(parent: ComposerTextView) {
            self.parent = parent
        }

        func textDidChange(_ notification: Notification) {
            guard let textView = notification.object as? NSTextView else {
                return
            }
            parent.text = textView.string
            textView.needsDisplay = true
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

private final class ComposerNSTextView: NSTextView {
    var submitHandler: (() -> Void)?
    var placeholder: String = "" {
        didSet {
            needsDisplay = true
        }
    }

    override func layout() {
        super.layout()
        guard let layoutManager, let textContainer, let scrollView = enclosingScrollView else { return }
        layoutManager.ensureLayout(for: textContainer)
        let textHeight = layoutManager.usedRect(for: textContainer).height
        let visibleHeight = scrollView.contentSize.height
        let topInset = max(0, (visibleHeight - textHeight) / 2)
        if abs(textContainerInset.height - topInset) > 0.5 {
            textContainerInset = NSSize(width: 0, height: topInset)
        }
    }

    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)

        guard string.isEmpty, !placeholder.isEmpty, let font else {
            return
        }

        let origin = textContainerOrigin
        let x = origin.x + (textContainer?.lineFragmentPadding ?? 0)
        let y = origin.y
        let attrs: [NSAttributedString.Key: Any] = [
            .font: font,
            .foregroundColor: NSColor.placeholderTextColor,
        ]
        (placeholder as NSString).draw(at: NSPoint(x: x, y: y), withAttributes: attrs)
    }

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        guard event.type == .keyDown else {
            return super.performKeyEquivalent(with: event)
        }

        let modifiers = event.modifierFlags.intersection([.command, .shift, .option, .control])
        guard modifiers == [.command], let chars = event.charactersIgnoringModifiers else {
            return super.performKeyEquivalent(with: event)
        }

        switch chars.lowercased() {
        case "a":
            selectAll(nil)
            return true
        case "c":
            copy(nil)
            return true
        case "v":
            paste(nil)
            return true
        case "x":
            cut(nil)
            return true
        case "z":
            undoManager?.undo()
            return true
        default:
            return super.performKeyEquivalent(with: event)
        }
    }

    override func doCommand(by selector: Selector) {
        let isNewlineCommand = selector == #selector(insertNewline(_:))
            || selector == #selector(insertLineBreak(_:))
            || selector == #selector(insertNewlineIgnoringFieldEditor(_:))
        guard isNewlineCommand, !hasMarkedText() else {
            super.doCommand(by: selector)
            return
        }

        let modifiers = NSApp.currentEvent?.modifierFlags.intersection([
            .shift,
            .control,
            .option,
            .command,
        ]) ?? []

        if modifiers == [.shift] {
            insertNewline(nil)
            return
        }

        if modifiers.isEmpty {
            submitHandler?()
            return
        }

        super.doCommand(by: selector)
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
