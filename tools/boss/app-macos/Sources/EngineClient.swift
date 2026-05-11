import Foundation
import Network

struct EngineSpawnRequest: Sendable {
    let runId: String
    let workspacePath: String
    /// 1-indexed slot the engine has claimed for this worker. The
    /// app must host the pane in this exact slot or fail with
    /// `.slotBusy`. The engine is the source of truth for slot
    /// allocation; the previous `firstIndex(where:)` heuristic in
    /// the app has been removed.
    let slotId: Int
    let initialInput: String
    let env: [(String, String)]
    /// Engine-supplied 2–4 word summary of the task this worker is
    /// running. Surfaced in the pane titlebar in place of the raw
    /// run id; the run id is kept available as a tooltip for
    /// traceability. `nil` means the engine had no summary to offer
    /// — the pane falls back to displaying the run id.
    let summary: String?
}

enum EngineSpawnError: Sendable {
    case noAvailableSlot
    /// Engine asked us to host the pane in a slot that already has a
    /// session. Surfaces engine↔app disagreement explicitly instead
    /// of silently re-allocating to a different slot, which would
    /// re-introduce the dual-allocator bug the engine-owns-slots
    /// refactor exists to fix.
    case slotBusy
    case internalFailure(String)
}

enum EngineSpawnResult: Sendable {
    case success(slotId: Int, shellPid: Int32)
    case failure(EngineSpawnError)
}

enum EngineReleaseError: Sendable {
    case unknownSlot
    case internalFailure(String)
}

enum EngineReleaseResult: Sendable {
    case success
    case failure(EngineReleaseError)
}

enum EngineSendError: Sendable {
    case unknownSlot
    case internalFailure(String)
}

enum EngineSendResult: Sendable {
    case success
    case failure(EngineSendError)
}

enum EngineFocusError: Sendable {
    case unknownSlot
    case internalFailure(String)
}

enum EngineFocusResult: Sendable {
    case success
    case failure(EngineFocusError)
}

enum EngineInterruptError: Sendable {
    case unknownSlot
    case internalFailure(String)
}

enum EngineInterruptResult: Sendable {
    case success
    case failure(EngineInterruptError)
}

enum EngineRequestKind: Sendable {
    case spawnWorkerPane(EngineSpawnRequest)
    case releaseWorkerPane(slotId: Int, killGraceSeconds: UInt32)
    case sendToPane(slotId: Int, text: String)
    case focusWorkerPane(slotId: Int)
    case interruptWorkerPane(slotId: Int)
}

enum EngineEvent {
    case connected
    case disconnected
    case workInvalidated(topic: String, productId: String?, itemIds: [String])
    case appSessionRegistered
    case engineRequest(requestId: String, request: EngineRequestKind)
    case productsList(products: [WorkProduct])
    case projectsList(productId: String, projects: [WorkProject])
    case workTree(product: WorkProduct, projects: [WorkProject], tasks: [WorkTask], chores: [WorkTask], taskRuntimes: [WorkTaskRuntime], dependencies: [WorkItemDependency])
    case workItemCreated(item: WorkItemPayload)
    case workItemUpdated(item: WorkItemPayload)
    case projectTasksReordered(projectId: String, taskIds: [String])
    case workItemDeleted(id: String)
    case workError(message: String)
    case error(message: String)
    /// Snapshot of every allocated worker slot's live runtime state.
    /// Delivered both as a one-shot reply to
    /// `list_worker_live_states` and as a topic push on
    /// `worker.live_states` whenever any slot changes.
    case workerLiveStatesList(states: [WorkerLiveState])
    /// Snapshot of slot ids whose live-status summarizer has been
    /// manually disabled by the human. Sourced from a one-shot reply
    /// to `list_live_status_disabled_slots`.
    case liveStatusDisabledSlotsList(slotIds: [Int])
    /// Echoed result of a `set_live_status_enabled` toggle. The UI
    /// uses this to confirm the engine accepted the change before
    /// flipping local state.
    case liveStatusEnabledSet(slotId: Int, enabled: Bool)
    /// Engine reply to a `ResolveProjectDesignDoc` RPC. Carries the
    /// per-project `ProjectDesignDocState` the kanban consumes to
    /// pick the right icon affordance and open dispatch.
    case projectDesignDocResolved(output: ResolveProjectDesignDocOutput)
}

final class EngineClient: @unchecked Sendable {
    var onEvent: (@MainActor @Sendable (EngineEvent) -> Void)?

    private let socketPath: String
    private let queue = DispatchQueue(label: "Boss.EngineClient")
    private var connection: NWConnection?
    private var buffer = Data()
    private var shouldReconnect = false

    init(socketPath: String) {
        self.socketPath = socketPath
    }

    func start() {
        shouldReconnect = true
        connect()
    }

    func stop() {
        shouldReconnect = false
        connection?.cancel()
        connection = nil
        buffer.removeAll(keepingCapacity: false)
    }

    private func connect() {
        guard connection == nil else {
            return
        }

        let parameters = NWParameters(tls: nil, tcp: NWProtocolTCP.Options())
        let endpoint = NWEndpoint.unix(path: socketPath)
        let connection = NWConnection(to: endpoint, using: parameters)
        self.connection = connection

        connection.stateUpdateHandler = { [weak self] (state: NWConnection.State) in
            guard let self else { return }
            switch state {
            case .ready:
                self.emit(.connected)
                self.receiveNext()
            case .waiting(let error):
                self.emit(.error(message: "socket waiting: \(error.localizedDescription)"))
                self.connection = nil
                connection.cancel()
                self.emit(.disconnected)
                self.scheduleReconnect()
            case .failed(let error):
                self.emit(.error(message: "socket failed: \(error.localizedDescription)"))
                self.connection = nil
                self.emit(.disconnected)
                self.scheduleReconnect()
            case .cancelled:
                self.connection = nil
                self.emit(.disconnected)
                self.scheduleReconnect()
            default:
                break
            }
        }

        connection.start(queue: queue)
    }

    func sendListProducts() {
        sendLine(["type": "list_products"])
    }

    /// Ask the engine for the current live runtime snapshot of every
    /// allocated worker slot. Pair this with a subscription to the
    /// `worker.live_states` topic to keep up to date in real time.
    func sendListWorkerLiveStates() {
        sendLine(["type": "list_worker_live_states"])
    }

    /// Ask the engine for the current set of slot ids that have the
    /// live-status summarizer disabled. Used at session start so the
    /// Agents-tab toggle reflects the persisted state.
    func sendListLiveStatusDisabledSlots() {
        sendLine(["type": "list_live_status_disabled_slots"])
    }

    /// Toggle the live-status summarizer for one slot. The engine
    /// persists the choice in its metadata KV so it survives an
    /// engine restart.
    func sendSetLiveStatusEnabled(slotId: Int, enabled: Bool) {
        sendLine([
            "type": "set_live_status_enabled",
            "slot_id": slotId,
            "enabled": enabled,
        ])
    }

    func sendSubscribe(topics: [String]) {
        sendLine([
            "type": "subscribe",
            "topics": topics,
        ])
    }

    func sendUnsubscribe(topics: [String]) {
        sendLine([
            "type": "unsubscribe",
            "topics": topics,
        ])
    }

    func sendGetWorkTree(productId: String) {
        sendLine([
            "type": "get_work_tree",
            "product_id": productId,
        ])
    }

    func sendCreateProduct(name: String, description: String, repoRemoteURL: String) {
        sendLine([
            "type": "create_product",
            "name": name,
            "description": description,
            "repo_remote_url": repoRemoteURL,
        ])
    }

    func sendCreateProject(productId: String, name: String, description: String, goal: String) {
        sendLine([
            "type": "create_project",
            "product_id": productId,
            "name": name,
            "description": description,
            "goal": goal,
        ])
    }

    func sendCreateTask(productId: String, projectId: String, name: String, description: String) {
        sendLine([
            "type": "create_task",
            "product_id": productId,
            "project_id": projectId,
            "name": name,
            "description": description,
            "created_via": "mac_app",
        ])
    }

    func sendCreateChore(productId: String, name: String, description: String) {
        sendLine([
            "type": "create_chore",
            "product_id": productId,
            "name": name,
            "description": description,
            "created_via": "mac_app",
        ])
    }

    func sendUpdateWorkItem(id: String, patch: [String: Any]) {
        sendLine([
            "type": "update_work_item",
            "id": id,
            "patch": patch,
        ])
    }

    /// Ask the engine to schedule an execution for `workItemId`.
    /// Mirrors the bossctl `work start` path. Idempotent — the
    /// engine treats a non-terminal latest execution as the current
    /// owner and won't create a duplicate. Used by the kanban's
    /// drop-into-Doing flow described in
    /// `tools/boss/docs/designs/work-kanban.md` §1.
    func sendRequestExecution(workItemId: String) {
        sendLine([
            "type": "request_execution",
            "work_item_id": workItemId,
        ])
    }

    func sendDeleteWorkItem(id: String) {
        sendLine([
            "type": "delete_work_item",
            "id": id,
        ])
    }

    func sendReorderProjectTasks(projectId: String, taskIds: [String]) {
        sendLine([
            "type": "reorder_project_tasks",
            "project_id": projectId,
            "task_ids": taskIds,
        ])
    }

    func sendRegisterAppSession() {
        sendLine([
            "type": "register_app_session",
        ])
    }

    /// Resolve a project's design-doc pointer. Engine replies with
    /// `project_design_doc_resolved` carrying a
    /// `ResolveProjectDesignDocOutput` whose `state` discriminator
    /// drives the kanban affordance and the open dispatcher. No DB
    /// writes; no topic events — callers can re-issue lazily as cards
    /// scroll into view without polluting the work tree.
    func sendResolveProjectDesignDoc(projectID: String) {
        sendLine([
            "type": "resolve_project_design_doc",
            "project_id": projectID,
        ])
    }

    func sendSpawnWorkerPaneResponse(requestId: String, result: EngineSpawnResult) {
        let resultPayload: [String: Any]
        switch result {
        case .success(let slotId, let shellPid):
            resultPayload = [
                "Ok": [
                    "slot_id": slotId,
                    "shell_pid": Int(shellPid),
                ]
            ]
        case .failure(let error):
            resultPayload = ["Err": engineToAppErrorPayload(error)]
        }
        sendLine([
            "type": "engine_response",
            "request_id": requestId,
            "response": [
                "kind": "spawn_worker_pane",
                "result": resultPayload,
            ],
        ])
    }

    func sendReleaseWorkerPaneResponse(requestId: String, result: EngineReleaseResult) {
        let resultPayload: [String: Any]
        switch result {
        case .success:
            resultPayload = ["Ok": [String: Any]()]
        case .failure(let error):
            resultPayload = ["Err": releaseEngineToAppErrorPayload(error)]
        }
        sendLine([
            "type": "engine_response",
            "request_id": requestId,
            "response": [
                "kind": "release_worker_pane",
                "result": resultPayload,
            ],
        ])
    }

    func sendSendToPaneResponse(requestId: String, result: EngineSendResult) {
        let resultPayload: [String: Any]
        switch result {
        case .success:
            resultPayload = ["Ok": [String: Any]()]
        case .failure(let error):
            resultPayload = ["Err": sendEngineToAppErrorPayload(error)]
        }
        sendLine([
            "type": "engine_response",
            "request_id": requestId,
            "response": [
                "kind": "send_to_pane",
                "result": resultPayload,
            ],
        ])
    }

    func sendFocusWorkerPaneResponse(requestId: String, result: EngineFocusResult) {
        let resultPayload: [String: Any]
        switch result {
        case .success:
            resultPayload = ["Ok": [String: Any]()]
        case .failure(let error):
            resultPayload = ["Err": focusEngineToAppErrorPayload(error)]
        }
        sendLine([
            "type": "engine_response",
            "request_id": requestId,
            "response": [
                "kind": "focus_worker_pane",
                "result": resultPayload,
            ],
        ])
    }

    func sendInterruptWorkerPaneResponse(requestId: String, result: EngineInterruptResult) {
        let resultPayload: [String: Any]
        switch result {
        case .success:
            resultPayload = ["Ok": [String: Any]()]
        case .failure(let error):
            resultPayload = ["Err": interruptEngineToAppErrorPayload(error)]
        }
        sendLine([
            "type": "engine_response",
            "request_id": requestId,
            "response": [
                "kind": "interrupt_worker_pane",
                "result": resultPayload,
            ],
        ])
    }

    private func engineToAppErrorPayload(_ error: EngineSpawnError) -> [String: Any] {
        switch error {
        case .noAvailableSlot:
            return ["kind": "no_available_slot"]
        case .slotBusy:
            return ["kind": "slot_busy"]
        case .internalFailure(let message):
            return ["kind": "internal", "message": message]
        }
    }

    private func releaseEngineToAppErrorPayload(_ error: EngineReleaseError) -> [String: Any] {
        switch error {
        case .unknownSlot:
            return ["kind": "unknown_slot"]
        case .internalFailure(let message):
            return ["kind": "internal", "message": message]
        }
    }

    private func sendEngineToAppErrorPayload(_ error: EngineSendError) -> [String: Any] {
        switch error {
        case .unknownSlot:
            return ["kind": "unknown_slot"]
        case .internalFailure(let message):
            return ["kind": "internal", "message": message]
        }
    }

    private func focusEngineToAppErrorPayload(_ error: EngineFocusError) -> [String: Any] {
        switch error {
        case .unknownSlot:
            return ["kind": "unknown_slot"]
        case .internalFailure(let message):
            return ["kind": "internal", "message": message]
        }
    }

    private func interruptEngineToAppErrorPayload(_ error: EngineInterruptError) -> [String: Any] {
        switch error {
        case .unknownSlot:
            return ["kind": "unknown_slot"]
        case .internalFailure(let message):
            return ["kind": "internal", "message": message]
        }
    }

    private func sendLine(_ payload: [String: Any]) {
        guard let connection else {
            emit(.error(message:"engine connection is not established"))
            return
        }

        do {
            let envelope: [String: Any] = [
                "request_id": UUID().uuidString,
                "payload": payload,
            ]
            var data = try JSONSerialization.data(withJSONObject: envelope, options: [])
            data.append(0x0A)

            connection.send(content: data, completion: .contentProcessed { [weak self] error in
                guard let self else { return }
                if let error {
                    self.emit(.error(message:"socket send failed: \(error.localizedDescription)"))
                }
            })
        } catch {
            emit(.error(message:"failed to encode payload: \(error.localizedDescription)"))
        }
    }

    private func receiveNext() {
        connection?.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
            [weak self] data, _, isComplete, error in
            guard let self else { return }

            if let error {
                self.emit(.error(message:"socket receive failed: \(error.localizedDescription)"))
                self.connection = nil
                self.emit(.disconnected)
                self.scheduleReconnect()
                return
            }

            if let data, !data.isEmpty {
                self.buffer.append(data)
                self.consumeLines()
            }

            if isComplete {
                self.connection = nil
                self.emit(.disconnected)
                self.scheduleReconnect()
                return
            }

            self.receiveNext()
        }
    }

    private func consumeLines() {
        while let newline = buffer.firstIndex(of: 0x0A) {
            let lineData = buffer[..<newline]
            buffer.removeSubrange(...newline)

            guard !lineData.isEmpty else {
                continue
            }

            guard let envelope = try? JSONSerialization.jsonObject(with: Data(lineData)) as? [String: Any],
                let payload = envelope["payload"] as? [String: Any],
                let type = payload["type"] as? String
            else {
                emit(.error(message:"received invalid JSON message from engine"))
                continue
            }

            switch type {
            case "topic_event":
                let topic = payload["topic"] as? String ?? ""
                guard let eventPayload = payload["event"] as? [String: Any],
                      let eventType = eventPayload["type"] as? String
                else {
                    break
                }
                if eventType == "work_invalidated" {
                    let productId = eventPayload["product_id"] as? String
                    let itemIds = eventPayload["item_ids"] as? [String] ?? []
                    emit(.workInvalidated(topic: topic, productId: productId, itemIds: itemIds))
                }
            case "products_list":
                let products = (payload["products"] as? [[String: Any]] ?? []).compactMap(parseProduct)
                emit(.productsList(products: products))
            case "projects_list":
                let productId = payload["product_id"] as? String ?? ""
                let projects = (payload["projects"] as? [[String: Any]] ?? []).compactMap(parseProject)
                emit(.projectsList(productId: productId, projects: projects))
            case "work_tree":
                guard let productPayload = payload["product"] as? [String: Any],
                      let product = parseProduct(productPayload)
                else {
                    emit(.error(message:"received invalid work tree payload from engine"))
                    break
                }
                let projects = (payload["projects"] as? [[String: Any]] ?? []).compactMap(parseProject)
                let tasks = (payload["tasks"] as? [[String: Any]] ?? []).compactMap(parseTask)
                let chores = (payload["chores"] as? [[String: Any]] ?? []).compactMap(parseTask)
                let taskRuntimes = (payload["task_runtimes"] as? [[String: Any]] ?? [])
                    .compactMap(parseTaskRuntime)
                let dependencies = (payload["dependencies"] as? [[String: Any]] ?? [])
                    .compactMap(parseWorkItemDependency)
                emit(.workTree(
                    product: product,
                    projects: projects,
                    tasks: tasks,
                    chores: chores,
                    taskRuntimes: taskRuntimes,
                    dependencies: dependencies
                ))
            case "work_item_created":
                guard let itemPayload = payload["item"] as? [String: Any],
                      let item = parseWorkItem(itemPayload)
                else {
                    emit(.error(message: "received invalid work item payload from engine"))
                    break
                }
                emit(.workItemCreated(item: item))
            case "work_item_updated":
                guard let itemPayload = payload["item"] as? [String: Any],
                      let item = parseWorkItem(itemPayload)
                else {
                    emit(.error(message: "received invalid work item payload from engine"))
                    break
                }
                emit(.workItemUpdated(item: item))
            case "project_tasks_reordered":
                let projectId = payload["project_id"] as? String ?? ""
                let taskIds = payload["task_ids"] as? [String] ?? []
                emit(.projectTasksReordered(projectId: projectId, taskIds: taskIds))
            case "work_item_deleted":
                let id = payload["id"] as? String ?? ""
                guard !id.isEmpty else {
                    break
                }
                emit(.workItemDeleted(id: id))
            case "work_error":
                let message = payload["message"] as? String ?? "unknown work error"
                emit(.workError(message: message))
            case "error":
                let message = payload["message"] as? String ?? "unknown engine error"
                emit(.error(message: message))
            case "app_session_registered":
                emit(.appSessionRegistered)
            case "engine_request":
                guard
                    let requestId = payload["request_id"] as? String,
                    let request = payload["request"] as? [String: Any],
                    let kind = request["kind"] as? String
                else {
                    emit(.error(message:"engine_request missing required fields"))
                    break
                }
                switch kind {
                case "spawn_worker_pane":
                    let runId = request["run_id"] as? String ?? ""
                    let workspacePath = request["workspace_path"] as? String ?? ""
                    let slotId = (request["slot_id"] as? NSNumber)?.intValue ?? 0
                    let initialInput = request["initial_input"] as? String ?? ""
                    let env = (request["env"] as? [[String: Any]] ?? []).compactMap {
                        item -> (String, String)? in
                        guard let k = item["key"] as? String, let v = item["value"] as? String else {
                            return nil
                        }
                        return (k, v)
                    }
                    let summary = request["summary"] as? String
                    let spawn = EngineSpawnRequest(
                        runId: runId,
                        workspacePath: workspacePath,
                        slotId: slotId,
                        initialInput: initialInput,
                        env: env,
                        summary: summary
                    )
                    emit(.engineRequest(requestId: requestId, request: .spawnWorkerPane(spawn)))
                case "release_worker_pane":
                    let slotId = (request["slot_id"] as? NSNumber)?.intValue ?? 0
                    let killGrace = (request["kill_grace_seconds"] as? NSNumber)?.uint32Value ?? 0
                    emit(.engineRequest(
                        requestId: requestId,
                        request: .releaseWorkerPane(slotId: slotId, killGraceSeconds: killGrace)
                    ))
                case "send_to_pane":
                    let slotId = (request["slot_id"] as? NSNumber)?.intValue ?? 0
                    let text = request["text"] as? String ?? ""
                    emit(.engineRequest(
                        requestId: requestId,
                        request: .sendToPane(slotId: slotId, text: text)
                    ))
                case "focus_worker_pane":
                    let slotId = (request["slot_id"] as? NSNumber)?.intValue ?? 0
                    emit(.engineRequest(
                        requestId: requestId,
                        request: .focusWorkerPane(slotId: slotId)
                    ))
                case "interrupt_worker_pane":
                    let slotId = (request["slot_id"] as? NSNumber)?.intValue ?? 0
                    emit(.engineRequest(
                        requestId: requestId,
                        request: .interruptWorkerPane(slotId: slotId)
                    ))
                default:
                    emit(.error(message:"engine_request unknown kind: \(kind)"))
                }
            case "worker_live_states_list":
                let raw = payload["states"] as? [[String: Any]] ?? []
                let states = raw.compactMap(parseWorkerLiveState)
                emit(.workerLiveStatesList(states: states))
            case "live_status_disabled_slots_list":
                let raw = payload["slot_ids"] as? [Any] ?? []
                let slotIds = raw.compactMap { ($0 as? NSNumber)?.intValue }
                emit(.liveStatusDisabledSlotsList(slotIds: slotIds))
            case "live_status_enabled_set":
                let slotId = (payload["slot_id"] as? NSNumber)?.intValue ?? 0
                let enabled = (payload["enabled"] as? NSNumber)?.boolValue ?? false
                emit(.liveStatusEnabledSet(slotId: slotId, enabled: enabled))
            case "project_design_doc_resolved":
                guard let outputPayload = payload["output"] as? [String: Any],
                      let outputData = try? JSONSerialization.data(withJSONObject: outputPayload),
                      let output = try? JSONDecoder().decode(
                        ResolveProjectDesignDocOutput.self,
                        from: outputData
                      )
                else {
                    emit(.error(message: "received invalid project_design_doc_resolved payload"))
                    break
                }
                emit(.projectDesignDocResolved(output: output))
            default:
                break
            }
        }
    }

    private func emit(_ event: EngineEvent) {
        Task { @MainActor in
            self.onEvent?(event)
        }
    }

    private func scheduleReconnect() {
        guard shouldReconnect else {
            return
        }

        queue.asyncAfter(deadline: .now() + 1.0) { [weak self] in
            guard let self, self.shouldReconnect, self.connection == nil else {
                return
            }
            self.connect()
        }
    }

    private func parseProduct(_ payload: [String: Any]) -> WorkProduct? {
        guard let id = payload["id"] as? String,
              let name = payload["name"] as? String,
              let slug = payload["slug"] as? String,
              let description = payload["description"] as? String,
              let status = payload["status"] as? String,
              let createdAt = payload["created_at"] as? String,
              let updatedAt = payload["updated_at"] as? String
        else {
            return nil
        }

        return WorkProduct(
            id: id,
            name: name,
            slug: slug,
            description: description,
            repoRemoteURL: payload["repo_remote_url"] as? String,
            status: status,
            createdAt: createdAt,
            updatedAt: updatedAt
        )
    }

    private func parseProject(_ payload: [String: Any]) -> WorkProject? {
        guard let id = payload["id"] as? String,
              let productId = payload["product_id"] as? String,
              let name = payload["name"] as? String,
              let slug = payload["slug"] as? String,
              let description = payload["description"] as? String,
              let goal = payload["goal"] as? String,
              let status = payload["status"] as? String,
              let priority = payload["priority"] as? String,
              let createdAt = payload["created_at"] as? String,
              let updatedAt = payload["updated_at"] as? String
        else {
            return nil
        }

        return WorkProject(
            id: id,
            productID: productId,
            name: name,
            slug: slug,
            description: description,
            goal: goal,
            status: status,
            priority: priority,
            createdAt: createdAt,
            updatedAt: updatedAt,
            lastStatusActor: (payload["last_status_actor"] as? String) ?? "human",
            designDocRepoRemoteURL: payload["design_doc_repo_remote_url"] as? String,
            designDocBranch: payload["design_doc_branch"] as? String,
            designDocPath: payload["design_doc_path"] as? String
        )
    }

    private func parseTask(_ payload: [String: Any]) -> WorkTask? {
        guard let id = payload["id"] as? String,
              let productId = payload["product_id"] as? String,
              let kind = payload["kind"] as? String,
              let name = payload["name"] as? String,
              let description = payload["description"] as? String,
              let status = payload["status"] as? String,
              let createdAt = payload["created_at"] as? String,
              let updatedAt = payload["updated_at"] as? String
        else {
            return nil
        }

        let ordinal = (payload["ordinal"] as? NSNumber)?.intValue
        // Pre-priority engines may not emit the field at all; default
        // to `medium` to match the schema default rather than crashing
        // the parse on a missing key.
        let priority = (payload["priority"] as? String) ?? "medium"

        return WorkTask(
            id: id,
            productID: productId,
            projectID: payload["project_id"] as? String,
            kind: kind,
            name: name,
            description: description,
            status: status,
            priority: priority,
            ordinal: ordinal,
            prURL: payload["pr_url"] as? String,
            deletedAt: payload["deleted_at"] as? String,
            createdAt: createdAt,
            updatedAt: updatedAt,
            lastStatusActor: (payload["last_status_actor"] as? String) ?? "human",
            createdVia: (payload["created_via"] as? String) ?? "unknown"
        )
    }

    private func parseTaskRuntime(_ payload: [String: Any]) -> WorkTaskRuntime? {
        guard let workItemID = payload["work_item_id"] as? String else {
            return nil
        }
        return WorkTaskRuntime(
            workItemID: workItemID,
            executionStatus: payload["execution_status"] as? String,
            runStatus: payload["run_status"] as? String,
            executionID: payload["execution_id"] as? String
        )
    }

    private func parseWorkItemDependency(_ payload: [String: Any]) -> WorkItemDependency? {
        guard let dependentID = payload["dependent_id"] as? String,
              let prerequisiteID = payload["prerequisite_id"] as? String
        else {
            return nil
        }
        let relation = payload["relation"] as? String ?? "blocks"
        return WorkItemDependency(
            dependentID: dependentID,
            prerequisiteID: prerequisiteID,
            relation: relation
        )
    }


    private func parseWorkItem(_ payload: [String: Any]) -> WorkItemPayload? {
        guard let itemType = payload["item_type"] as? String else {
            return nil
        }

        switch itemType {
        case "product":
            guard let product = parseProduct(payload) else { return nil }
            return .product(product)
        case "project":
            guard let project = parseProject(payload) else { return nil }
            return .project(project)
        case "task":
            guard let task = parseTask(payload) else { return nil }
            return .task(task)
        case "chore":
            guard let task = parseTask(payload) else { return nil }
            return .chore(task)
        default:
            return nil
        }
    }

    private func parseWorkerLiveState(_ payload: [String: Any]) -> WorkerLiveState? {
        guard
            let slotId = (payload["slot_id"] as? NSNumber)?.intValue,
            let runId = payload["run_id"] as? String,
            let model = payload["model"] as? String,
            let activityRaw = payload["activity"] as? String,
            let activity = WorkerActivity(rawValue: activityRaw)
        else {
            return nil
        }
        let shellPid = (payload["shell_pid"] as? NSNumber)?.int32Value ?? 0
        return WorkerLiveState(
            slotId: slotId,
            runId: runId,
            model: model,
            shellPid: shellPid,
            lastEventAt: payload["last_event_at"] as? String,
            currentTool: payload["current_tool"] as? String,
            lastToolEndedAt: payload["last_tool_ended_at"] as? String,
            activity: activity,
            liveStatus: payload["live_status"] as? String,
            liveStatusAt: payload["live_status_at"] as? String
        )
    }
}
