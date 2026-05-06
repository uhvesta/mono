import Foundation
import Network

struct EngineSpawnRequest: Sendable {
    let runId: String
    let workspacePath: String
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

enum EngineRequestKind: Sendable {
    case spawnWorkerPane(EngineSpawnRequest)
    case releaseWorkerPane(slotId: Int, killGraceSeconds: UInt32)
    case sendToPane(slotId: Int, text: String)
}

enum EngineEvent {
    case connected
    case disconnected
    case workInvalidated(topic: String, productId: String?, itemIds: [String])
    case appSessionRegistered
    case engineRequest(requestId: String, request: EngineRequestKind)
    case productsList(products: [WorkProduct])
    case projectsList(productId: String, projects: [WorkProject])
    case workTree(product: WorkProduct, projects: [WorkProject], tasks: [WorkTask], chores: [WorkTask], taskRuntimes: [WorkTaskRuntime])
    case workItemCreated(item: WorkItemPayload)
    case workItemUpdated(item: WorkItemPayload)
    case projectTasksReordered(projectId: String, taskIds: [String])
    case workItemDeleted(id: String)
    case workError(message: String)
    case agentCreated(agent: Agent)
    case agentList(agents: [Agent])
    case agentRemoved(agentId: String)
    case chunk(agentId: String, text: String)
    case done(agentId: String, stopReason: String)
    case toolCall(agentId: String, name: String, status: String)
    case terminalStarted(agentId: String, id: String, title: String, command: String, cwd: String?)
    case terminalOutput(agentId: String, id: String, text: String)
    case terminalDone(agentId: String, id: String, exitCode: Int?, signal: String?)
    case permissionRequest(agentId: String, id: String, title: String)
    case agentReady(agentId: String)
    case error(agentId: String?, message: String)
}

final class EngineClient: @unchecked Sendable {
    var onEvent: (@MainActor @Sendable (EngineEvent) -> Void)?

    private let socketPath: String
    private let queue = DispatchQueue(label: "BossMacApp.EngineClient")
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
                self.emit(.error(agentId: nil, message: "socket waiting: \(error.localizedDescription)"))
                self.connection = nil
                connection.cancel()
                self.emit(.disconnected)
                self.scheduleReconnect()
            case .failed(let error):
                self.emit(.error(agentId: nil, message: "socket failed: \(error.localizedDescription)"))
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

    func sendCreateAgent(name: String?, role: AgentRole = .standard) {
        var payload: [String: Any] = [
            "type": "create_agent",
            "role": role.rawValue,
        ]
        if let name {
            payload["name"] = name
        }
        sendLine(payload)
    }

    func sendListAgents() {
        sendLine(["type": "list_agents"])
    }

    func sendRemoveAgent(agentId: String) {
        sendLine([
            "type": "remove_agent",
            "agent_id": agentId,
        ])
    }

    func sendPrompt(agentId: String, text: String) {
        sendLine([
            "type": "prompt",
            "agent_id": agentId,
            "text": text,
        ])
    }

    func sendPermissionResponse(agentId: String, id: String, granted: Bool) {
        sendLine([
            "type": "permission_response",
            "agent_id": agentId,
            "id": id,
            "granted": granted,
        ])
    }

    func sendListProducts() {
        sendLine(["type": "list_products"])
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
        ])
    }

    func sendCreateChore(productId: String, name: String, description: String) {
        sendLine([
            "type": "create_chore",
            "product_id": productId,
            "name": name,
            "description": description,
        ])
    }

    func sendUpdateWorkItem(id: String, patch: [String: Any]) {
        sendLine([
            "type": "update_work_item",
            "id": id,
            "patch": patch,
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

    private func engineToAppErrorPayload(_ error: EngineSpawnError) -> [String: Any] {
        switch error {
        case .noAvailableSlot:
            return ["kind": "no_available_slot"]
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

    private func sendLine(_ payload: [String: Any]) {
        guard let connection else {
            emit(.error(agentId: nil, message: "engine connection is not established"))
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
                    self.emit(.error(agentId: nil, message: "socket send failed: \(error.localizedDescription)"))
                }
            })
        } catch {
            emit(.error(agentId: nil, message: "failed to encode payload: \(error.localizedDescription)"))
        }
    }

    private func receiveNext() {
        connection?.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
            [weak self] data, _, isComplete, error in
            guard let self else { return }

            if let error {
                self.emit(.error(agentId: nil, message: "socket receive failed: \(error.localizedDescription)"))
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
                emit(.error(agentId: nil, message: "received invalid JSON message from engine"))
                continue
            }

            let agentId = payload["agent_id"] as? String

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
                    emit(.error(agentId: nil, message: "received invalid work tree payload from engine"))
                    break
                }
                let projects = (payload["projects"] as? [[String: Any]] ?? []).compactMap(parseProject)
                let tasks = (payload["tasks"] as? [[String: Any]] ?? []).compactMap(parseTask)
                let chores = (payload["chores"] as? [[String: Any]] ?? []).compactMap(parseTask)
                let taskRuntimes = (payload["task_runtimes"] as? [[String: Any]] ?? [])
                    .compactMap(parseTaskRuntime)
                emit(.workTree(
                    product: product,
                    projects: projects,
                    tasks: tasks,
                    chores: chores,
                    taskRuntimes: taskRuntimes
                ))
            case "work_item_created":
                guard let itemPayload = payload["item"] as? [String: Any],
                      let item = parseWorkItem(itemPayload)
                else {
                    emit(.error(agentId: nil, message: "received invalid work item payload from engine"))
                    break
                }
                emit(.workItemCreated(item: item))
            case "work_item_updated":
                guard let itemPayload = payload["item"] as? [String: Any],
                      let item = parseWorkItem(itemPayload)
                else {
                    emit(.error(agentId: nil, message: "received invalid work item payload from engine"))
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
            case "agent_created":
                guard let agent = parseAgent(payload) else {
                    emit(.error(agentId: nil, message: "received invalid agent payload from engine"))
                    break
                }
                emit(.agentCreated(agent: agent))
            case "agent_ready":
                emit(.agentReady(agentId: agentId ?? ""))
            case "agent_list":
                let agents = (payload["agents"] as? [[String: Any]] ?? []).compactMap {
                    parseAgent($0, isReady: true)
                }
                emit(.agentList(agents: agents))
            case "agent_removed":
                emit(.agentRemoved(agentId: agentId ?? ""))
            case "chunk":
                if let text = payload["text"] as? String, let aid = agentId {
                    emit(.chunk(agentId: aid, text: text))
                }
            case "done":
                let stopReason = payload["stop_reason"] as? String ?? "unknown"
                emit(.done(agentId: agentId ?? "", stopReason: stopReason))
            case "tool_call":
                let name = payload["name"] as? String ?? "tool"
                let status = payload["status"] as? String ?? "update"
                emit(.toolCall(agentId: agentId ?? "", name: name, status: status))
            case "terminal_started":
                let id = payload["id"] as? String ?? UUID().uuidString
                let title = payload["title"] as? String ?? "Terminal"
                let command = payload["command"] as? String ?? ""
                let cwd = payload["cwd"] as? String
                emit(.terminalStarted(agentId: agentId ?? "", id: id, title: title, command: command, cwd: cwd))
            case "terminal_output":
                let id = payload["id"] as? String ?? ""
                let text = payload["text"] as? String ?? ""
                guard !id.isEmpty, !text.isEmpty else {
                    break
                }
                emit(.terminalOutput(agentId: agentId ?? "", id: id, text: text))
            case "terminal_done":
                let id = payload["id"] as? String ?? ""
                guard !id.isEmpty else {
                    break
                }
                let exitCode = (payload["exit_code"] as? NSNumber)?.intValue
                let signal = payload["signal"] as? String
                emit(.terminalDone(agentId: agentId ?? "", id: id, exitCode: exitCode, signal: signal))
            case "permission_request":
                let id = payload["id"] as? String ?? ""
                let title = payload["title"] as? String ?? "Permission"
                emit(.permissionRequest(agentId: agentId ?? "", id: id, title: title))
            case "error":
                let message = payload["message"] as? String ?? "unknown engine error"
                emit(.error(agentId: agentId, message: message))
            case "app_session_registered":
                emit(.appSessionRegistered)
            case "engine_request":
                guard
                    let requestId = payload["request_id"] as? String,
                    let request = payload["request"] as? [String: Any],
                    let kind = request["kind"] as? String
                else {
                    emit(.error(agentId: nil, message: "engine_request missing required fields"))
                    break
                }
                switch kind {
                case "spawn_worker_pane":
                    let runId = request["run_id"] as? String ?? ""
                    let workspacePath = request["workspace_path"] as? String ?? ""
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
                default:
                    emit(.error(agentId: nil, message: "engine_request unknown kind: \(kind)"))
                }
            default:
                break
            }
        }
    }

    private func emit(_ event: EngineEvent) {
        Task { @MainActor in
            onEvent?(event)
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
            updatedAt: updatedAt
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

        return WorkTask(
            id: id,
            productID: productId,
            projectID: payload["project_id"] as? String,
            kind: kind,
            name: name,
            description: description,
            status: status,
            ordinal: ordinal,
            prURL: payload["pr_url"] as? String,
            deletedAt: payload["deleted_at"] as? String,
            createdAt: createdAt,
            updatedAt: updatedAt
        )
    }

    private func parseTaskRuntime(_ payload: [String: Any]) -> WorkTaskRuntime? {
        guard let workItemID = payload["work_item_id"] as? String else {
            return nil
        }
        return WorkTaskRuntime(
            workItemID: workItemID,
            executionStatus: payload["execution_status"] as? String,
            runStatus: payload["run_status"] as? String
        )
    }

    private func parseAgent(_ payload: [String: Any], isReady: Bool = false) -> Agent? {
        guard let id = payload["agent_id"] as? String,
              let name = payload["name"] as? String
        else {
            return nil
        }

        let rawRole = payload["role"] as? String ?? AgentRole.standard.rawValue
        let role = AgentRole(rawValue: rawRole) ?? .standard
        return Agent(id: id, name: name, role: role, isReady: isReady)
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
}
