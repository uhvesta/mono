import AppKit
import SwiftUI
import Textual

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
        WorkersDetailView(workspace: workersWorkspace)
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

    private func messageList(items: [TranscriptItem], emptyState: String? = nil) -> some View {
        ScrollViewReader { proxy in
            ScrollView {
                if items.isEmpty, let emptyState {
                    VStack(alignment: .leading, spacing: 10) {
                        Text(emptyState)
                            .font(.callout)
                            .foregroundStyle(.secondary)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(16)
                } else {
                    LazyVStack(alignment: .leading, spacing: 12) {
                        ForEach(items) { item in
                            switch item {
                            case .message(let message):
                                MessageBubble(message: message)
                                    .id(item.id)
                            case .terminal(let terminal):
                                TerminalActivityCard(activity: terminal)
                                    .id(item.id)
                            }
                        }
                    }
                    .padding(16)
                }
            }
            .onChange(of: items.count) {
                if let last = items.last {
                    DispatchQueue.main.async {
                        proxy.scrollTo(last.id, anchor: .bottom)
                    }
                }
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
                Text("Boss")
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
                Image(systemName: AgentRole.boss.systemImage)
                    .foregroundStyle(Color.accentColor)
                    .font(.system(size: 13, weight: .semibold))
            }
            .frame(width: 26, height: 26)

            if !isCollapsed {
                VStack(alignment: .leading, spacing: 1) {
                    Text(AgentRole.boss.title)
                        .font(.subheadline.weight(.semibold))
                        .foregroundStyle(.primary)
                        .lineLimit(1)
                    Text(bossStatusText)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }
            }

            Spacer(minLength: 0)

            Button {
                model.toggleBossPanelCollapsed()
            } label: {
                Image(systemName: isCollapsed ? "chevron.left" : "chevron.right")
                    .font(.system(size: 10, weight: .semibold))
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

    private var bossStatusText: String {
        if !model.isConnected {
            return "Disconnected"
        }
        if model.bossAgent == nil {
            return "Starting coordinator…"
        }
        if !model.isBossAgentReady {
            return "Starting coordinator…"
        }
        if model.isBossAgentBootstrapping {
            return "Loading Boss CLI reference…"
        }
        if model.bossBootstrapErrorMessage != nil {
            return "Boss CLI reference failed to load"
        }
        if model.isBossAgentSending {
            return "Coordinating…"
        }
        return "Coordinates work through Boss"
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

            if column == .backlog {
                HStack(spacing: 8) {
                    Button("New Task") {
                        model.presentCreateTask()
                    }
                    .disabled(model.selectedProject == nil || !model.isConnected)

                    Button("New Chore") {
                        model.presentCreateChore()
                    }
                    .disabled(model.selectedProduct == nil || !model.isConnected)
                }
                .font(.caption)
            }

            Divider()

            if itemCount == 0 {
                Text("No items")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, minHeight: 80, alignment: .topLeading)
            } else {
                VStack(alignment: .leading, spacing: 12) {
                    ForEach(sections) { section in
                        if model.workBoardGrouping == .project {
                            Text(section.title)
                                .font(.caption.weight(.semibold))
                                .foregroundStyle(.secondary)
                        }
                        VStack(alignment: .leading, spacing: 10) {
                            ForEach(section.items) { task in
                                Button {
                                    model.selectWorkCard(
                                        model.selectedTask?.id == task.id ? nil : task.id
                                    )
                                } label: {
                                    WorkBoardCardView(
                                        task: task,
                                        projectName: task.isChore ? nil : model.projectName(for: task.projectID),
                                        isSelected: model.selectedTask?.id == task.id
                                    )
                                }
                                .buttonStyle(.plain)
                                .popover(
                                    isPresented: Binding(
                                        get: { model.selectedTask?.id == task.id },
                                        set: { isPresented in
                                            if !isPresented, model.selectedTask?.id == task.id {
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
                    }
                }
            }
            Spacer(minLength: 0)
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
    private func workSidebarSectionTitle(_ title: String) -> some View {
        Text(title)
            .font(.caption.weight(.semibold))
            .foregroundStyle(.secondary)
            .textCase(.uppercase)
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
                        .lineLimit(isSelected ? 2 : 1)
                        .fixedSize(horizontal: false, vertical: true)
                        .layoutPriority(1)

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

private struct WorkBoardCardView: View {
    let task: WorkTask
    let projectName: String?
    let isSelected: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .top) {
                Text(task.name)
                    .font(.body.weight(.medium))
                    .foregroundStyle(.primary)
                    .multilineTextAlignment(.leading)
                Spacer(minLength: 8)
                Image(systemName: task.isChore ? "wrench.and.screwdriver" : "circle.hexagongrid")
                    .foregroundStyle(.secondary)
            }

            HStack {
                if let projectName, !projectName.isEmpty {
                    WorkStatusBadge(text: projectName)
                } else {
                    WorkStatusBadge(text: "Chore")
                }
                if task.status == "blocked" {
                    WorkStatusBadge(text: "Blocked")
                }
                Spacer()
                Text(task.status.replacingOccurrences(of: "_", with: " ").capitalized)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            if let prURL = task.prURL, !prURL.isEmpty {
                Text(prURL)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(cardBackground)
        .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .stroke(borderColor, lineWidth: isSelected ? 2 : 1)
        )
        .draggable(task.id)
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
                if let ordinal = task.ordinal, !task.isChore {
                    metadataRow("Phase", value: "\(ordinal)")
                }
                metadataRow("PR", value: task.prURL ?? "Not set")
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
            _priority = State(initialValue: "")
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
            .font(.caption2.weight(.semibold))
            .foregroundStyle(foregroundColor)
            .lineLimit(1)
            .minimumScaleFactor(0.95)
            .padding(.horizontal, 7)
            .padding(.vertical, 2)
            .background(backgroundColor)
            .clipShape(Capsule())
    }

    private var foregroundColor: Color {
        if emphasized {
            return .accentColor
        }
        return Color(nsColor: .secondaryLabelColor)
    }

    private var backgroundColor: Color {
        if emphasized {
            return Color.white.opacity(0.96)
        }
        return Color(nsColor: .controlBackgroundColor)
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
        Coordinator()
    }

    func makeNSView(context: Context) -> NSScrollView {
        let scrollView = NSScrollView()
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
        return scrollView
    }

    func updateNSView(_ nsView: NSScrollView, context: Context) {
        let coordinator = context.coordinator
        if nsView.documentView !== coordinator.documentView {
            nsView.documentView = coordinator.documentView
        }

        coordinator.sync(columns: columns)

        var clipWidth = nsView.contentView.bounds.width
        var clipHeight = nsView.contentView.bounds.height
        let contentWidth = totalContentWidth(for: columns.count)
        let hasOverflow = contentWidth > clipWidth + 0.5
        if nsView.hasHorizontalScroller != hasOverflow {
            nsView.hasHorizontalScroller = hasOverflow
            nsView.tile()
            clipWidth = nsView.contentView.bounds.width
            clipHeight = nsView.contentView.bounds.height
        }

        coordinator.documentView.frame = NSRect(
            origin: .zero,
            size: NSSize(width: max(contentWidth, clipWidth), height: clipHeight)
        )

        var x = horizontalPadding
        for hostingView in coordinator.hostingViews {
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
        let currentOrigin = nsView.contentView.bounds.origin
        let maxHorizontalOffset = max(0, coordinator.documentView.frame.width - clipWidth)
        let clampedOrigin = NSPoint(
            x: min(max(currentOrigin.x, 0), maxHorizontalOffset),
            y: 0
        )
        if abs(currentOrigin.x - clampedOrigin.x) > 0.5
            || abs(currentOrigin.y - clampedOrigin.y) > 0.5
        {
            nsView.contentView.scroll(to: clampedOrigin)
            nsView.reflectScrolledClipView(nsView.contentView)
        }
    }

    private func totalContentWidth(for columnCount: Int) -> CGFloat {
        guard columnCount > 0 else { return 0 }
        return horizontalPadding
            + (CGFloat(columnCount) * columnWidth)
            + (CGFloat(max(columnCount - 1, 0)) * spacing)
            + horizontalPadding
    }

    @MainActor
    final class Coordinator {
        let documentView = FlippedContentView()
        var hostingViews: [NSHostingView<AnyView>] = []

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

private struct MessageBubble: View {
    let message: ChatMessage

    var body: some View {
        switch message.role {
        case .assistant:
            assistantText
        case .user:
            userBubble
        case .system:
            systemText
        }
    }

    private var assistantText: some View {
        HStack {
            StructuredText(markdown: message.text)
                .textual.textSelection(.enabled)
                .frame(maxWidth: 720, alignment: .leading)
            Spacer(minLength: 60)
        }
    }

    private var userBubble: some View {
        HStack {
            Spacer(minLength: 80)
            Text(message.text)
                .font(.body)
                .textSelection(.enabled)
                .padding(12)
                .frame(maxWidth: 560, alignment: .leading)
                .background(.blue.opacity(0.18))
                .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
        }
    }

    private var systemText: some View {
        HStack {
            Text(message.text)
                .font(.caption)
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
                .frame(maxWidth: 720, alignment: .leading)
            Spacer(minLength: 60)
        }
    }
}

private struct TerminalActivityCard: View {
    let activity: TerminalActivity

    @State private var isExpanded: Bool = false
    @State private var isHovering: Bool = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if isExpanded {
                VStack(spacing: 0) {
                    terminalHeader
                        .padding(.horizontal, 12)
                        .padding(.vertical, 10)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(terminalHeaderBackground)

                    Divider()
                        .overlay(Color(nsColor: .separatorColor))

                    TerminalOutputPane(activity: activity, background: terminalOutputBackground)
                }
                .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
                .overlay(
                    RoundedRectangle(cornerRadius: 12, style: .continuous)
                        .stroke(Color(nsColor: .separatorColor), lineWidth: 1)
                )
            } else {
                terminalHeader
                    .padding(.horizontal, 12)
                    .padding(.vertical, 10)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(terminalHeaderBackground)
                    .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
                    .overlay(
                        RoundedRectangle(cornerRadius: 12, style: .continuous)
                            .stroke(Color(nsColor: .separatorColor), lineWidth: 1)
                    )
            }
        }
        .onHover { hovering in
            isHovering = hovering
        }
    }

    private var commandPrefix: String {
        if isFailed {
            return "Failed"
        }
        if isSuccessful {
            return "Success"
        }
        return "Running"
    }

    private var command: String {
        let command = activity.command.isEmpty ? "<command unavailable>" : activity.command
        return command
    }

    private var isSuccessful: Bool {
        activity.status == "Done"
    }

    private var isFailed: Bool {
        activity.status.hasPrefix("Failed") || activity.status.hasPrefix("Terminated")
    }

    private var terminalHeader: some View {
        HStack(alignment: .center, spacing: 12) {
            VStack(alignment: .leading, spacing: 6) {
                if let cwd = activity.cwd, !cwd.isEmpty {
                    Text(cwd)
                        .font(.system(.footnote, design: .monospaced))
                        .foregroundStyle(.secondary)
                }

                commandLineText
                    .font(.system(.callout, design: .monospaced))
                    .textSelection(.enabled)
            }

            Spacer(minLength: 12)

            Button {
                isExpanded.toggle()
            } label: {
                Image(systemName: isExpanded ? "chevron.up" : "chevron.down")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .frame(width: 22, height: 22)
                    .background(Color(nsColor: .quaternaryLabelColor).opacity(0.22))
                    .clipShape(RoundedRectangle(cornerRadius: 6, style: .continuous))
            }
            .buttonStyle(.plain)
            .help(isExpanded ? "Hide output" : "Show output")
            .opacity(isHovering ? 1 : 0)
            .allowsHitTesting(isHovering)
            .animation(.easeInOut(duration: 0.12), value: isHovering)
        }
    }

    private var statusWordColor: Color {
        if isFailed {
            return .red
        }
        if isSuccessful {
            return .green
        }
        return .primary
    }

    private var commandLineText: Text {
        Text(commandPrefix).foregroundColor(statusWordColor)
            + Text(" \(command)").foregroundColor(.primary)
    }

    private var terminalHeaderBackground: Color {
        Color(nsColor: .controlBackgroundColor)
    }

    private var terminalOutputBackground: Color {
        Color(nsColor: .textBackgroundColor)
    }
}

private struct TerminalOutputPane: View {
    let activity: TerminalActivity
    let background: Color

    @State private var isPinnedToBottom: Bool = true
    @State private var suppressOffsetTracking: Bool = false
    @State private var contentFrame: CGRect = .zero
    @State private var viewportHeight: CGFloat = 0

    private let bottomThreshold: CGFloat = 6

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    Text(activity.output.isEmpty ? "" : activity.output)
                        .font(.system(.callout, design: .monospaced))
                        .frame(maxWidth: .infinity, alignment: .topLeading)
                        .textSelection(.enabled)
                        .padding(12)
                    Color.clear
                        .frame(height: 1)
                        .id(outputBottomID)
                }
                .background(
                    GeometryReader { geo in
                        Color.clear.preference(
                            key: TerminalContentFramePreferenceKey.self,
                            value: geo.frame(in: .named(scrollSpaceID))
                        )
                    }
                )
            }
            .coordinateSpace(name: scrollSpaceID)
            .background(
                GeometryReader { geo in
                    Color.clear.preference(
                        key: TerminalViewportHeightPreferenceKey.self,
                        value: geo.size.height
                    )
                }
            )
            .frame(minHeight: 120, maxHeight: 240)
            .background(background)
            .onPreferenceChange(TerminalContentFramePreferenceKey.self) { frame in
                contentFrame = frame
                refreshPinnedState()
            }
            .onPreferenceChange(TerminalViewportHeightPreferenceKey.self) { height in
                viewportHeight = height
                refreshPinnedState()
            }
            .onAppear {
                scrollToBottom(proxy, animated: false)
                isPinnedToBottom = true
            }
            .onChange(of: activity.output.count) { _, _ in
                guard isPinnedToBottom else {
                    return
                }

                suppressOffsetTracking = true
                scrollToBottom(proxy, animated: true)

                DispatchQueue.main.asyncAfter(deadline: .now() + 0.12) {
                    isPinnedToBottom = true
                    suppressOffsetTracking = false
                }
            }
        }
    }

    private var outputBottomID: String {
        "terminal-output-bottom-\(activity.id)"
    }

    private var scrollSpaceID: String {
        "terminal-scroll-space-\(activity.id)"
    }

    private func scrollToBottom(_ proxy: ScrollViewProxy, animated: Bool) {
        if animated {
            withAnimation(.easeOut(duration: 0.12)) {
                proxy.scrollTo(outputBottomID, anchor: .bottom)
            }
        } else {
            proxy.scrollTo(outputBottomID, anchor: .bottom)
        }
    }

    private func refreshPinnedState() {
        guard !suppressOffsetTracking else {
            return
        }

        let bottomDistance = max(0, contentFrame.height + contentFrame.minY - viewportHeight)
        isPinnedToBottom = bottomDistance <= bottomThreshold
    }
}

private struct TerminalContentFramePreferenceKey: PreferenceKey {
    static let defaultValue: CGRect = .zero

    static func reduce(value: inout CGRect, nextValue: () -> CGRect) {
        value = nextValue()
    }
}

private struct TerminalViewportHeightPreferenceKey: PreferenceKey {
    static let defaultValue: CGFloat = 0

    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = nextValue()
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
