import Foundation

struct PendingPermission: Identifiable {
    let id: String
    let agentId: String
    let title: String
}

@MainActor
final class ChatViewModel: ObservableObject {
    @Published var navigationMode: NavigationMode = .agents
    @Published var agents: [Agent] = []
    @Published var bossDraft: String = ""
    @Published var isConnected: Bool = false
    @Published var pendingPermission: PendingPermission?
    @Published var products: [WorkProduct] = []
    @Published var projectsByProductID: [String: [WorkProject]] = [:]
    @Published var tasksByProjectID: [String: [WorkTask]] = [:]
    @Published var choresByProductID: [String: [WorkTask]] = [:]
    @Published var selectedWorkProductID: String?
    @Published var selectedProjectFilterIDs: Set<String> = []
    @Published var includeChores: Bool = true
    @Published var showBlockedOnly: Bool = false
    @Published var selectedWorkCardID: String?
    @Published var workBoardGrouping: WorkBoardGrouping = .none
    @Published var selectedWorkNodeID: WorkNodeID?
    @Published var pendingWorkCreateRequest: WorkCreateRequest?
    @Published var pendingWorkEditRequest: WorkEditRequest?
    @Published var workErrorMessage: String?
    @Published var workSearchText: String = ""
    @Published var isBossPanelCollapsed: Bool = false

    var bossAgent: Agent? {
        agents.first { $0.isBoss }
    }

    var bossAgentID: String? {
        bossAgent?.id
    }

    var bossTimeline: [TranscriptItem] {
        bossAgent?.timeline ?? []
    }

    var isBossAgentSending: Bool {
        bossAgent?.isSending ?? false
    }

    var isBossAgentBootstrapping: Bool {
        guard let agentId = bossAgentID else { return false }
        return bootstrappingBossAgentIDs.contains(agentId)
    }

    var bossBootstrapErrorMessage: String? {
        guard let agentId = bossAgentID else { return nil }
        return bossBootstrapErrorsByAgentID[agentId]
    }

    var isBossAgentReady: Bool {
        bossAgent?.isReady ?? false
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
        switch selectedProjectFilterIDs.count {
        case 0:
            return "All projects"
        case 1:
            return selectedProject?.name ?? "1 project"
        case let count:
            return "\(count) projects"
        }
    }

    var hasProjectFilters: Bool {
        !selectedProjectFilterIDs.isEmpty
    }

    var selectedTask: WorkTask? {
        guard let taskID = selectedWorkCardID else { return nil }
        return task(withID: taskID)
    }

    var projectsForSelectedProduct: [WorkProject] {
        guard let productID = currentSelectedProductID else { return [] }
        return (projectsByProductID[productID] ?? []).sorted(by: projectSort)
    }

    var visibleWorkItems: [WorkTask] {
        guard let productID = currentSelectedProductID else { return [] }

        let query = workSearchText.trimmingCharacters(in: .whitespacesAndNewlines)
        let projectFilter = selectedProjectFilterIDs

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
    private let processController = EngineProcessController()
    private let socketPath: String
    private let showSystemMessages: Bool
    private var didStart = false
    private var didStartEngine = false
    private var hasConnectedOnce = false
    private var pendingBossCreation = false
    private var bootstrappingBossAgentIDs: Set<String> = []
    private var bootstrappedBossAgentIDs: Set<String> = []
    private var bossBootstrapErrorsByAgentID: [String: String] = [:]
    private var permissionQueue: [PendingPermission] = []
    private var subscribedWorkTopics: Set<String> = []
    private let defaults = UserDefaults.standard

    private let maxTerminalOutputChars = 200_000
    private let navigationModeDefaultsKey = "boss.navigationMode"
    private let selectedWorkProductDefaultsKey = "boss.work.selectedProductID"
    private let selectedProjectFilterIDsDefaultsKey = "boss.work.projectFilterIDs"
    private let includeChoresDefaultsKey = "boss.work.includeChores"
    private let showBlockedOnlyDefaultsKey = "boss.work.showBlockedOnly"
    private let workBoardGroupingDefaultsKey = "boss.work.grouping"
    private let bossPanelCollapsedDefaultsKey = "boss.work.bossPanelCollapsed"

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
        if let groupingRaw = defaults.string(forKey: workBoardGroupingDefaultsKey),
           let grouping = WorkBoardGrouping(rawValue: groupingRaw) {
            workBoardGrouping = grouping
        }
        isBossPanelCollapsed = defaults.bool(forKey: bossPanelCollapsedDefaultsKey)

        processController.onOutputLine = { [weak self] line in
            self?.appendSystemMessage(line)
        }

        engine.onEvent = { [weak self] event in
            self?.handle(event)
        }

        // In the AppKit-hosted macOS shell, the root SwiftUI `.task` can be
        // missed on some launches. Schedule the normal startup path here too so
        // the engine connection still comes up reliably.
        DispatchQueue.main.async { [weak self] in
            self?.startIfNeeded()
        }
    }

    deinit {
        processController.stop()
        engine.stop()
    }

    func createAgent(name: String? = nil, role: AgentRole = .standard) {
        if role == .boss {
            pendingBossCreation = true
        }
        engine.sendCreateAgent(name: name, role: role)
    }

    func ensureBossAgent() {
        guard bossAgent == nil, !pendingBossCreation else { return }
        createAgent(name: AgentRole.boss.title, role: .boss)
    }

    func sendBossDraft() {
        guard let agentId = bossAgentID else { return }
        guard isBossAgentReady else { return }
        let trimmed = bossDraft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }

        appendMessage(agentId: agentId, role: .user, text: trimmed)
        mutateAgent(agentId) { $0.isSending = true; $0.activeAssistantMessageID = nil }
        engine.sendPrompt(agentId: agentId, text: bossPromptText(for: trimmed))
        bossDraft = ""
    }

    func toggleBossPanelCollapsed() {
        isBossPanelCollapsed.toggle()
        defaults.set(isBossPanelCollapsed, forKey: bossPanelCollapsedDefaultsKey)
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
        goal: String = ""
    ) {
        let trimmedName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedName.isEmpty else { return }

        workErrorMessage = nil
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
                description: description
            )
        case .chore(let productID):
            engine.sendCreateChore(
                productId: productID,
                name: trimmedName,
                description: description
            )
        }

        pendingWorkCreateRequest = nil
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

    func moveTask(_ taskID: String, to column: WorkBoardColumnKey) {
        guard let task = task(withID: taskID) else { return }
        let targetStatus = column.targetStatus
        guard task.status != targetStatus else { return }
        engine.sendUpdateWorkItem(id: task.id, patch: ["status": targetStatus])
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

    func respondToPendingPermission(granted: Bool) {
        guard let pending = pendingPermission else { return }
        engine.sendPermissionResponse(agentId: pending.agentId, id: pending.id, granted: granted)
        appendSystemMessage(
            "[permission] \(granted ? "allowed" : "denied"): \(pending.title)",
            agentId: pending.agentId
        )
        pendingPermission = nil
        showNextPermissionIfNeeded()
    }

    func refreshWork() {
        guard isConnected else { return }
        engine.sendListProducts()
        if let productID = currentSelectedProductID {
            engine.sendGetWorkTree(productId: productID)
        }
    }

    // MARK: - Event Handling

    var paneSpawnHandler: ((EngineSpawnRequest) -> EngineSpawnResult)?
    var paneReleaseHandler: ((Int, UInt32) -> EngineReleaseResult)?
    var paneSendHandler: ((Int, String) -> EngineSendResult)?

    private func handle(_ event: EngineEvent) {
        switch event {
        case .connected:
            isConnected = true
            hasConnectedOnce = true
            pendingBossCreation = false
            engine.sendRegisterAppSession()
            engine.sendListAgents()
            refreshWorkSubscriptions()
            engine.sendListProducts()
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
            }
        case .disconnected:
            isConnected = false
            subscribedWorkTopics.removeAll()
            bootstrappingBossAgentIDs.removeAll()
            bootstrappedBossAgentIDs.removeAll()
            bossBootstrapErrorsByAgentID.removeAll()
            for i in agents.indices {
                agents[i].isSending = false
                agents[i].activeAssistantMessageID = nil
            }
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
            if let selectedWorkProductID,
               !self.products.contains(where: { $0.id == selectedWorkProductID }) {
                self.selectedWorkProductID = nil
                self.selectedProjectFilterIDs = []
                self.selectedWorkCardID = nil
                defaults.removeObject(forKey: selectedWorkProductDefaultsKey)
                persistProjectFilterIDs()
            }
            if currentSelectedProductID == nil, let first = self.products.first {
                self.selectedWorkProductID = first.id
                defaults.set(first.id, forKey: selectedWorkProductDefaultsKey)
                engine.sendGetWorkTree(productId: first.id)
            } else if let productID = currentSelectedProductID {
                engine.sendGetWorkTree(productId: productID)
            }
            refreshWorkSubscriptions()
        case .projectsList(let productId, let projects):
            projectsByProductID[productId] = projects.sorted(by: projectSort)
        case .workTree(let product, let projects, let tasks, let chores):
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
            reconcileWorkSelection()
            refreshWorkSubscriptions()
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
        case .agentCreated(let agent):
            pendingBossCreation = pendingBossCreation && !agent.isBoss
            if agent.isBoss {
                bossBootstrapErrorsByAgentID[agent.id] = nil
            }
            upsertAgent(agent)
        case .agentReady(let agentId):
            mutateAgent(agentId) { $0.isReady = true }
            startBossBootstrapIfNeeded(agentId: agentId)
        case .agentList(let list):
            pendingBossCreation = false
            synchronizeAgents(with: list)
            for agent in list {
                mutateAgent(agent.id) { $0.isReady = true }
                if agent.isBoss {
                    startBossBootstrapIfNeeded(agentId: agent.id)
                }
            }
            ensureBossAgent()
        case .agentRemoved(let agentId):
            agents.removeAll { $0.id == agentId }
            bootstrappingBossAgentIDs.remove(agentId)
            bootstrappedBossAgentIDs.remove(agentId)
            bossBootstrapErrorsByAgentID[agentId] = nil
        case .chunk(let agentId, let text):
            guard !isBossBootstrapping(agentId: agentId) else { return }
            appendAssistantChunk(agentId: agentId, text: text)
        case .done(let agentId, let stopReason):
            if isBossBootstrapping(agentId: agentId) {
                completeBossBootstrap(agentId: agentId)
                return
            }
            mutateAgent(agentId) { $0.isSending = false; $0.activeAssistantMessageID = nil }
            appendSystemMessage("[done] \(stopReason)", agentId: agentId)
        case .toolCall(let agentId, let name, let status):
            guard !isBossBootstrapping(agentId: agentId) else { return }
            appendSystemMessage("[tool] \(name) (\(status))", agentId: agentId)
        case .terminalStarted(let agentId, let id, let title, let command, let cwd):
            guard !isBossBootstrapping(agentId: agentId) else { return }
            mutateAgent(agentId) { $0.activeAssistantMessageID = nil }
            upsertTerminalActivity(agentId: agentId, id: id, title: title, command: command, cwd: cwd)
        case .terminalOutput(let agentId, let id, let text):
            guard !isBossBootstrapping(agentId: agentId) else { return }
            appendTerminalOutput(agentId: agentId, id: id, text: text)
        case .terminalDone(let agentId, let id, let exitCode, let signal):
            guard !isBossBootstrapping(agentId: agentId) else { return }
            completeTerminalActivity(agentId: agentId, id: id, exitCode: exitCode, signal: signal)
        case .permissionRequest(let agentId, let id, let title):
            guard !isBossBootstrapping(agentId: agentId) else {
                if isExpectedBossBootstrapPermission(title: title) {
                    engine.sendPermissionResponse(agentId: agentId, id: id, granted: true)
                } else {
                    completeBossBootstrap(
                        agentId: agentId,
                        error: "Boss bootstrap unexpectedly requested permission."
                    )
                }
                return
            }
            enqueuePermission(agentId: agentId, id: id, title: title)
        case .error(let agentId, let message):
            if let agentId, isBossBootstrapping(agentId: agentId) {
                completeBossBootstrap(agentId: agentId, error: message)
                return
            }
            if let agentId {
                mutateAgent(agentId) { $0.isSending = false; $0.activeAssistantMessageID = nil }
            }
            if shouldSuppressSocketStartupError(message) { return }
            if let agentId {
                appendSystemMessage("[error] \(message)", agentId: agentId, alwaysShow: true)
            } else {
                workErrorMessage = message
            }
        }
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
        var topics: Set<String> = ["work.products"]
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

    private func agentIndex(_ agentId: String) -> Int? {
        agents.firstIndex { $0.id == agentId }
    }

    private func upsertAgent(_ agent: Agent) {
        if let index = agentIndex(agent.id) {
            let existing = agents[index]
            agents[index].name = agent.name
            agents[index].role = agent.role
            agents[index].isReady = existing.isReady || agent.isReady
            return
        }

        agents.append(agent)
        agents.sort { lhs, rhs in
            if lhs.role != rhs.role {
                return lhs.role == .boss
            }
            return lhs.name.localizedCaseInsensitiveCompare(rhs.name) == .orderedAscending
        }
    }

    private func synchronizeAgents(with incoming: [Agent]) {
        let incomingIDs = Set(incoming.map(\.id))
        agents.removeAll { !incomingIDs.contains($0.id) }
        for agent in incoming {
            upsertAgent(agent)
        }
    }

    private func mutateAgent(_ agentId: String, _ body: (inout Agent) -> Void) {
        guard let index = agentIndex(agentId) else { return }
        body(&agents[index])
    }

    private func appendMessage(agentId: String, role: ChatRole, text: String) {
        mutateAgent(agentId) {
            $0.timeline.append(.message(ChatMessage(role: role, text: text)))
        }
    }

    private func startBossBootstrapIfNeeded(agentId: String) {
        guard bossAgentID == agentId || agents.contains(where: { $0.id == agentId && $0.isBoss }) else {
            return
        }
        guard !bootstrappedBossAgentIDs.contains(agentId) else { return }
        guard !bootstrappingBossAgentIDs.contains(agentId) else { return }

        bootstrappingBossAgentIDs.insert(agentId)
        bossBootstrapErrorsByAgentID[agentId] = nil
        mutateAgent(agentId) { $0.isSending = true; $0.activeAssistantMessageID = nil }
        engine.sendPrompt(agentId: agentId, text: bossBootstrapPrompt())
    }

    private func completeBossBootstrap(agentId: String, error: String? = nil) {
        bootstrappingBossAgentIDs.remove(agentId)
        if let error {
            bossBootstrapErrorsByAgentID[agentId] = error
        } else {
            bootstrappedBossAgentIDs.insert(agentId)
            bossBootstrapErrorsByAgentID[agentId] = nil
        }
        mutateAgent(agentId) { $0.isSending = false; $0.activeAssistantMessageID = nil }
    }

    private func isBossBootstrapping(agentId: String) -> Bool {
        bootstrappingBossAgentIDs.contains(agentId)
    }

    private func isExpectedBossBootstrapPermission(title: String) -> Bool {
        title.localizedCaseInsensitiveContains("boss reference")
    }

    private func bossPromptText(for userText: String) -> String {
        """
        <boss_ui_context>
        \(bossRuntimeContext())
        </boss_ui_context>

        <user_request>
        \(userText)
        </user_request>
        """
    }

    private func bossBootstrapPrompt() -> String {
        """
        This is hidden session bootstrap work for The Boss.

        Before interacting with the user, run `boss reference --json --no-input` once and read it carefully. Treat that output as the authoritative Boss CLI reference for this session.

        Rules:
        - Do not use `boss ... --help` for syntax discovery during this bootstrap.
        - Do not ask the user anything.
        - Do not create or update any Boss work items.
        - Do not inspect the repository.
        - Your task is not complete until you have actually run the command and read its output.

        After you have loaded the reference, reply with a very short acknowledgement.
        """
    }

    private func bossRuntimeContext() -> String {
        var lines: [String] = []

        if let selectedProduct {
            lines.append("current_product_id: \(selectedProduct.id)")
            lines.append("current_product_name: \(selectedProduct.name)")
            lines.append("current_product_slug: \(selectedProduct.slug)")
            lines.append("current_product_status: \(selectedProduct.status)")
            if let repoRemoteURL = selectedProduct.repoRemoteURL, !repoRemoteURL.isEmpty {
                lines.append("current_product_repo: \(repoRemoteURL)")
            }
            if !selectedProduct.description.isEmpty {
                lines.append(
                    "current_product_description: \(bossContextSnippet(selectedProduct.description))"
                )
            }
        } else {
            lines.append("current_product: none_selected")
            if !products.isEmpty {
                lines.append("available_products:")
                for product in products.prefix(8) {
                    lines.append(
                        "- \(product.name) [slug=\(product.slug), status=\(product.status)]"
                    )
                }
                if products.count > 8 {
                    lines.append("- ... and \(products.count - 8) more products")
                }
            }
        }

        if let selectedProject {
            lines.append("current_project_filter: \(selectedProject.name)")
            lines.append("current_project_filter_id: \(selectedProject.id)")
        } else {
            lines.append("current_project_filter: all_projects")
        }

        let projects = projectsForSelectedProduct
        if projects.isEmpty {
            lines.append("existing_projects: none")
        } else {
            lines.append("existing_projects:")
            for project in projects.prefix(12) {
                let taskCount = (tasksByProjectID[project.id] ?? []).count
                var summary = "- \(project.name) [id=\(project.id), status=\(project.status), priority=\(project.priority), tasks=\(taskCount)]"
                if !project.goal.isEmpty {
                    summary += " goal=\(bossContextSnippet(project.goal))"
                }
                lines.append(summary)
            }
            if projects.count > 12 {
                lines.append("- ... and \(projects.count - 12) more projects")
            }
        }

        if let chores = currentSelectedProductID.flatMap({ choresByProductID[$0] }) {
            lines.append("current_product_chore_count: \(chores.count)")
        }

        if let selectedTask {
            lines.append("selected_work_item: \(selectedTask.name) [kind=\(selectedTask.kind), status=\(selectedTask.status)]")
        }

        lines.append("instruction: use this context when deciding whether work belongs in an existing project, should be represented as a chore, or should become a new project.")

        return lines.joined(separator: "\n")
    }

    private func bossContextSnippet(_ text: String, limit: Int = 140) -> String {
        let normalized = text
            .replacingOccurrences(of: "\n", with: " ")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        guard normalized.count > limit else { return normalized }
        let end = normalized.index(normalized.startIndex, offsetBy: limit)
        return normalized[..<end].trimmingCharacters(in: .whitespacesAndNewlines) + "..."
    }

    private func appendSystemMessage(_ text: String, agentId: String? = nil, alwaysShow: Bool = false) {
        guard alwaysShow || showSystemMessages else { return }
        if let agentId {
            appendMessage(agentId: agentId, role: .system, text: text)
        }
    }

    private func enqueuePermission(agentId: String, id: String, title: String) {
        let request = PendingPermission(id: id, agentId: agentId, title: title)
        if pendingPermission == nil {
            pendingPermission = request
        } else {
            permissionQueue.append(request)
        }
    }

    private func showNextPermissionIfNeeded() {
        guard pendingPermission == nil, !permissionQueue.isEmpty else { return }
        pendingPermission = permissionQueue.removeFirst()
    }

    private func appendAssistantChunk(agentId: String, text: String) {
        guard let agentIdx = agentIndex(agentId) else { return }
        let agent = agents[agentIdx]

        if let msgId = agent.activeAssistantMessageID,
           let timelineIdx = messageIndex(in: agents[agentIdx].timeline, for: msgId) {
            guard case .message(var message) = agents[agentIdx].timeline[timelineIdx] else { return }
            message.text += text
            agents[agentIdx].timeline[timelineIdx] = .message(message)
            return
        }

        let message = ChatMessage(role: .assistant, text: text)
        agents[agentIdx].activeAssistantMessageID = message.id
        agents[agentIdx].timeline.append(.message(message))
    }

    private func messageIndex(in timeline: [TranscriptItem], for id: UUID) -> Int? {
        timeline.firstIndex { item in
            guard case .message(let message) = item else { return false }
            return message.id == id
        }
    }

    private func upsertTerminalActivity(agentId: String, id: String, title: String, command: String, cwd: String?) {
        let index = ensureTerminalActivity(agentId: agentId, id: id)
        guard let agentIdx = agentIndex(agentId),
              case .terminal(var activity) = agents[agentIdx].timeline[index] else { return }
        activity.title = title
        if !command.isEmpty { activity.command = command }
        if let cwd { activity.cwd = cwd }
        activity.status = "Running…"
        agents[agentIdx].timeline[index] = .terminal(activity)
    }

    private func appendTerminalOutput(agentId: String, id: String, text: String) {
        let index = ensureTerminalActivity(agentId: agentId, id: id)
        guard let agentIdx = agentIndex(agentId),
              case .terminal(var activity) = agents[agentIdx].timeline[index] else { return }
        activity.output += text
        if activity.output.count > maxTerminalOutputChars {
            let overflow = activity.output.count - maxTerminalOutputChars
            activity.output.removeFirst(overflow)
        }
        agents[agentIdx].timeline[index] = .terminal(activity)
    }

    private func completeTerminalActivity(agentId: String, id: String, exitCode: Int?, signal: String?) {
        let index = ensureTerminalActivity(agentId: agentId, id: id)
        guard let agentIdx = agentIndex(agentId),
              case .terminal(var activity) = agents[agentIdx].timeline[index] else { return }
        if let exitCode {
            activity.status = exitCode == 0 ? "Done" : "Failed (exit \(exitCode))"
        } else if let signal, !signal.isEmpty {
            activity.status = "Terminated (signal \(signal))"
        } else {
            activity.status = "Done"
        }
        agents[agentIdx].timeline[index] = .terminal(activity)
    }

    private func ensureTerminalActivity(agentId: String, id: String) -> Int {
        guard let agentIdx = agentIndex(agentId) else {
            let agent = Agent(id: agentId, name: agentId)
            agents.append(agent)
            return ensureTerminalActivity(agentId: agentId, id: id)
        }

        if let index = agents[agentIdx].terminalEntryIndexByID[id],
           index < agents[agentIdx].timeline.count,
           case .terminal = agents[agentIdx].timeline[index] {
            return index
        }

        let activity = TerminalActivity(
            id: id, title: "Terminal command", command: "", cwd: nil, output: "", status: "Running…"
        )
        let index = agents[agentIdx].timeline.count
        agents[agentIdx].timeline.append(.terminal(activity))
        agents[agentIdx].terminalEntryIndexByID[id] = index
        return index
    }

    private func product(withID id: String) -> WorkProduct? {
        products.first { $0.id == id }
    }

    private func project(withID id: String) -> WorkProject? {
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

    func workItems(in column: WorkBoardColumnKey) -> [WorkTask] {
        visibleWorkItems
            .filter { $0.boardColumn == column }
            .sorted(by: boardTaskSort)
    }

    func workSections(in column: WorkBoardColumnKey) -> [WorkBoardSection] {
        let items = workItems(in: column)
        guard workBoardGrouping == .project else {
            return [WorkBoardSection(id: column.rawValue, title: column.title, items: items)]
        }

        let grouped = Dictionary(grouping: items) { task in
            task.isChore ? "Chores" : (projectName(for: task.projectID) ?? "No Project")
        }

        return grouped.keys.sorted().compactMap { key in
            guard let sectionItems = grouped[key], !sectionItems.isEmpty else { return nil }
            return WorkBoardSection(id: "\(column.rawValue)-\(key)", title: key, items: sectionItems)
        }
    }

    func isTaskVisible(_ task: WorkTask) -> Bool {
        workItems(in: task.boardColumn).contains(where: { $0.id == task.id })
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
            upsertProduct(product)
        case .project(let project):
            engine.sendGetWorkTree(productId: project.productID)
        case .task(let task), .chore(let task):
            engine.sendGetWorkTree(productId: task.productID)
        }
        workErrorMessage = nil
    }

    private func reconcileWorkSelection() {
        guard let selectedWorkProductID else { return }

        if !products.contains(where: { $0.id == selectedWorkProductID }) {
            self.selectedWorkProductID = products.first?.id
            if let firstProductID = products.first?.id {
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
