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

enum EngineRevealError: Sendable {
    case internalFailure(String)
}

enum EngineRevealResult: Sendable {
    case success
    case failure(EngineRevealError)
}

enum EngineRequestKind: Sendable {
    case spawnWorkerPane(EngineSpawnRequest)
    case releaseWorkerPane(slotId: Int, killGraceSeconds: UInt32)
    case sendToPane(slotId: Int, text: String)
    case focusWorkerPane(slotId: Int)
    case interruptWorkerPane(slotId: Int)
    case revealWorkItem(workItemId: String, productId: String)
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
    /// Response to `list_conflict_resolutions` — the filtered set of
    /// rows for the Engine tab. Phase 5 #13/#14 of the merge-conflict
    /// design.
    case conflictResolutionsList(attempts: [WorkConflictResolution])
    /// Response to `list_ci_remediations` — the filtered set of
    /// `ci_remediations` rows for the Engine tab. Phase 11 #37 of
    /// the merge-conflict design (CI extensions).
    case ciRemediationsList(attempts: [WorkCiRemediation])
    /// Activity-feed push: a fresh conflict-resolution attempt was
    /// created (or a `retry` reset an existing one) and a worker is
    /// about to take over. The Engine tab refreshes; the badge state
    /// is unaffected (only `succeeded` counts as a "cleared" event).
    case conflictResolutionStarted(productID: String, workItemID: String, attemptID: String, prURL: String)
    /// Activity-feed push: an attempt finished successfully. Drives the
    /// "🔧 conflict cleared" PR-card badge (Phase 5 #15) and refreshes
    /// the Engine tab.
    case conflictResolutionSucceeded(productID: String, workItemID: String, attemptID: String, prURL: String)
    /// Activity-feed push: an attempt failed terminally.
    case conflictResolutionFailed(productID: String, workItemID: String, attemptID: String, prURL: String, failureReason: String)
    /// Activity-feed push: an attempt was abandoned on purpose.
    case conflictResolutionAbandoned(productID: String, workItemID: String, attemptID: String, prURL: String, failureReason: String)
    /// Activity-feed push: a fresh CI-remediation attempt was created
    /// for an in-review PR. `attemptKind` is `"fix"` or `"retrigger"`
    /// per the engine's pre-spawn triage. Mirrors
    /// `conflictResolutionStarted` (merge-conflict-handling-in-review
    /// Phase 10 #34).
    case ciRemediationStarted(productID: String, workItemID: String, attemptID: String, prURL: String, attemptKind: String)
    /// Activity-feed push: the engine observed the parent PR back at
    /// CI clean and retired the remediation attempt. The parent has
    /// been flipped from `blocked: ci_failure` back to `in_review`.
    case ciRemediationSucceeded(productID: String, workItemID: String, attemptID: String, prURL: String)
    /// Activity-feed push: the engine cleared `blocked: ci_failure` on a
    /// task but found no active remediation attempt to retire — the prior
    /// attempt was already terminal (failed/abandoned). Distinct from
    /// `ciRemediationSucceeded`: the `ci failing` badge should be cleared
    /// but the `ci auto-fixed` badge must NOT be set.
    case ciFailureCleared(productID: String, workItemID: String, prURL: String)
    /// Activity-feed push: a CI-remediation attempt terminated in
    /// `failed`. Fired when the worker calls
    /// `boss engine ci mark-failed` or when the completion-path
    /// catch-all (`no_push_no_classification`) fires.
    case ciRemediationFailed(productID: String, workItemID: String, attemptID: String, prURL: String, failureReason: String)
    /// Activity-feed push: a CI-remediation attempt was abandoned on
    /// purpose (parent PR closed externally, manual move, etc.).
    case ciRemediationAbandoned(productID: String, workItemID: String, attemptID: String, prURL: String, failureReason: String)
    /// Activity-feed push: the engine has given up auto-fixing this
    /// PR's CI. The parent is now `blocked: ci_failure_exhausted` and
    /// the user is the next actor (typically via
    /// `boss engine ci retry <work-item-id>`).
    case ciRemediationExhausted(productID: String, workItemID: String, prURL: String, attemptsUsed: Int, budget: Int)
    /// Response to `list_feature_flags` — a snapshot of every
    /// registered engine feature flag and its current value. Drives
    /// the Feature Flags debug pane.
    case featureFlagsList(flags: [FeatureFlag])
    /// Echoed result of a `set_feature_flag` toggle: the engine has
    /// persisted the new value and consumer-side `is_enabled` checks
    /// will see it immediately. The debug pane uses this as the
    /// "reload confirmed" signal to render the toggle as committed.
    case featureFlagSet(name: String, enabled: Bool)
    /// Response to `get_engine_health` — a snapshot of the engine's
    /// user-visible configuration health (currently
    /// `ANTHROPIC_API_KEY` presence). Empty `issues` means healthy;
    /// any element drives the top-of-window banner and the Settings
    /// pane warning. Introduced after #699 where a missing API key
    /// silently broke summarization with no UI affordance.
    case engineHealthResult(apiKeyPresent: Bool, issues: [EngineHealthIssue])
    /// Response to `get_settings` — snapshot of every per-installation
    /// setting and its current value. Drives the Settings window.
    case settingsList(settings: [EngineSetting])
    /// Echoed result of a `set_setting` toggle: the engine has
    /// persisted the new value. The Settings window uses this as the
    /// "saved" signal.
    case settingSet(key: String, enabled: Bool)
    /// Response to `metrics_list_live` — bulk snapshot of every
    /// registered engine counter and gauge, sorted by name. Drives the
    /// Metrics debug pane's initial load and its polling timer.
    case metricsListLiveResult(entries: [EngineMetric])
    /// Response to `list_attention_items_for_work_item` — open and
    /// resolved attention items for a given product/work-item id.
    case attentionItemsForWorkItemList(workItemID: String, items: [WorkAttentionItem])
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

    /// Ask the engine for the per-installation settings snapshot.
    /// Used by the Settings window on appear so the rendered state
    /// reflects what the engine has persisted.
    func sendGetSettings() {
        sendLine(["type": "get_settings"])
    }

    /// Ask the engine for its user-visible configuration health.
    /// Called once at session-start (after `connected`) so the
    /// top-of-window banner surfaces a missing `ANTHROPIC_API_KEY`
    /// before the user notices summaries never appear (#699). Cheap
    /// — the engine just reads `Option::is_some` on the agent-config
    /// key; no IO.
    func sendGetEngineHealth() {
        sendLine(["type": "get_engine_health"])
    }

    /// Set one per-installation setting. Engine persists to
    /// `settings.toml` and replies with `setting_set` once the
    /// in-memory store is updated.
    func sendSetSetting(key: String, enabled: Bool) {
        sendLine([
            "type": "set_setting",
            "key": key,
            "enabled": enabled,
        ])
    }

    /// Ask the engine for a live snapshot of every registered metric.
    /// Used by the Metrics debug pane on appear and on its 5-second
    /// polling timer so values refresh without a manual reload.
    func sendMetricsListLive() {
        sendLine(["type": "metrics_list_live"])
    }

    /// Signal the engine that the Boss app window just became active.
    /// The engine schedules an immediate pass of every PR-state reconciler
    /// so the kanban reflects upstream GitHub changes (merged PRs, new
    /// review decisions, check-status updates) without waiting for the
    /// next periodic tick. Engine-side quiescing (15 s window) prevents
    /// repeated GitHub API calls on rapid focus-toggle events.
    func sendKickPrReconcilers() {
        sendLine(["type": "kick_pr_reconcilers"])
    }

    /// Ask the engine for the registered feature-flag set. Used by
    /// the Feature Flags debug pane on appear and after every toggle
    /// so the rendered state matches what the engine persisted.
    func sendListFeatureFlags() {
        sendLine(["type": "list_feature_flags"])
    }

    /// Toggle one feature flag. Engine persists to
    /// `feature-flags.toml`, updates the in-memory store, and replies
    /// with `feature_flag_set` once consumer-side `is_enabled` calls
    /// see the new value.
    func sendSetFeatureFlag(name: String, enabled: Bool) {
        sendLine([
            "type": "set_feature_flag",
            "name": name,
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

    func sendListAttentionItemsForWorkItem(workItemID: String) {
        sendLine([
            "type": "list_attention_items_for_work_item",
            "work_item_id": workItemID,
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

    func sendCreateTask(
        productId: String,
        projectId: String,
        name: String,
        description: String,
        repoRemoteURL: String? = nil
    ) {
        var payload: [String: Any] = [
            "type": "create_task",
            "product_id": productId,
            "project_id": projectId,
            "name": name,
            "description": description,
            "created_via": "mac_app",
        ]
        if let repoRemoteURL, !repoRemoteURL.isEmpty {
            payload["repo_remote_url"] = repoRemoteURL
        }
        sendLine(payload)
    }

    func sendCreateChore(
        productId: String,
        name: String,
        description: String,
        repoRemoteURL: String? = nil
    ) {
        var payload: [String: Any] = [
            "type": "create_chore",
            "product_id": productId,
            "name": name,
            "description": description,
            "created_via": "mac_app",
        ]
        if let repoRemoteURL, !repoRemoteURL.isEmpty {
            payload["repo_remote_url"] = repoRemoteURL
        }
        sendLine(payload)
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

    func sendSetProductExternalTracker(
        productId: String,
        kind: String,
        config: [String: Any]
    ) {
        sendLine([
            "type": "set_product_external_tracker",
            "product_id": productId,
            "kind": kind,
            "config": config,
            "unset": false,
        ])
    }

    func sendUnsetProductExternalTracker(productId: String) {
        sendLine([
            "type": "set_product_external_tracker",
            "product_id": productId,
            "unset": true,
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

    /// Engine-tab listing fetch (Phase 5 #14). `productID = nil`
    /// returns every product's attempts; `statuses` is AND-ed on the
    /// server, `limit` caps the response.
    func sendListConflictResolutions(
        productID: String? = nil,
        statuses: [String] = [],
        workItemID: String? = nil,
        limit: Int? = nil
    ) {
        var payload: [String: Any] = ["type": "list_conflict_resolutions"]
        if let productID {
            payload["product_id"] = productID
        }
        if !statuses.isEmpty {
            payload["status"] = statuses
        }
        if let workItemID {
            payload["work_item_id"] = workItemID
        }
        if let limit {
            payload["limit"] = limit
        }
        sendLine(payload)
    }

    /// Engine-tab listing fetch for CI remediations (design Phase 11
    /// #37). Mirror of `sendListConflictResolutions`.
    func sendListCiRemediations(
        productID: String? = nil,
        statuses: [String] = [],
        workItemID: String? = nil,
        limit: Int? = nil
    ) {
        var payload: [String: Any] = ["type": "list_ci_remediations"]
        if let productID {
            payload["product_id"] = productID
        }
        if !statuses.isEmpty {
            payload["status"] = statuses
        }
        if let workItemID {
            payload["work_item_id"] = workItemID
        }
        if let limit {
            payload["limit"] = limit
        }
        sendLine(payload)
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

    func sendRevealWorkItemResponse(requestId: String, result: EngineRevealResult) {
        let resultPayload: [String: Any]
        switch result {
        case .success:
            resultPayload = ["Ok": [String: Any]()]
        case .failure(let error):
            resultPayload = ["Err": revealEngineToAppErrorPayload(error)]
        }
        sendLine([
            "type": "engine_response",
            "request_id": requestId,
            "response": [
                "kind": "reveal_work_item",
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

    private func revealEngineToAppErrorPayload(_ error: EngineRevealError) -> [String: Any] {
        switch error {
        case .internalFailure(let message):
            return ["kind": "internal", "message": message]
        }
    }

    /// Test-only spy: invoked on every outbound payload before
    /// JSON-encoding. Tests inject a recorder to assert that the
    /// chore/task create flow puts `repo_remote_url` on the wire as
    /// expected (multi-repo work modeling design Q10). Setting the
    /// hook does not bypass the real send — the socket write still
    /// runs when a connection exists, so production-path callers see
    /// no behaviour change.
    var outboundRecorder: (([String: Any]) -> Void)?

    private func sendLine(_ payload: [String: Any]) {
        outboundRecorder?(payload)

        // Log outbound engine_response messages so both sides of every
        // IPC round-trip have a disk record.
        if let type = payload["type"] as? String, type == "engine_response",
           let requestId = payload["request_id"] as? String,
           let response = payload["response"] as? [String: Any],
           let kind = response["kind"] as? String {
            IpcLog.shared.log(
                requestId: requestId,
                direction: "app→engine",
                kind: kind,
                body: response
            )
        }

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
                IpcLog.shared.log(
                    requestId: requestId,
                    direction: "engine→app",
                    kind: kind,
                    body: request
                )
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
                case "reveal_work_item":
                    let workItemId = request["work_item_id"] as? String ?? ""
                    let productId = request["product_id"] as? String ?? ""
                    emit(.engineRequest(
                        requestId: requestId,
                        request: .revealWorkItem(workItemId: workItemId, productId: productId)
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
            case "conflict_resolutions_list":
                let raw = payload["attempts"] as? [[String: Any]] ?? []
                let attempts = raw.compactMap(parseConflictResolution)
                emit(.conflictResolutionsList(attempts: attempts))
            case "ci_remediations_list":
                let raw = payload["attempts"] as? [[String: Any]] ?? []
                let attempts = raw.compactMap(parseCiRemediation)
                emit(.ciRemediationsList(attempts: attempts))
            case "conflict_resolution_started":
                emit(.conflictResolutionStarted(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    attemptID: payload["attempt_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? ""
                ))
            case "conflict_resolution_succeeded":
                emit(.conflictResolutionSucceeded(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    attemptID: payload["attempt_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? ""
                ))
            case "conflict_resolution_failed":
                emit(.conflictResolutionFailed(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    attemptID: payload["attempt_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? "",
                    failureReason: payload["failure_reason"] as? String ?? ""
                ))
            case "conflict_resolution_abandoned":
                emit(.conflictResolutionAbandoned(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    attemptID: payload["attempt_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? "",
                    failureReason: payload["failure_reason"] as? String ?? ""
                ))
            case "ci_remediation_started":
                emit(.ciRemediationStarted(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    attemptID: payload["attempt_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? "",
                    attemptKind: payload["attempt_kind"] as? String ?? ""
                ))
            case "ci_remediation_succeeded":
                emit(.ciRemediationSucceeded(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    attemptID: payload["attempt_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? ""
                ))
            case "ci_failure_cleared":
                emit(.ciFailureCleared(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? ""
                ))
            case "ci_remediation_failed":
                emit(.ciRemediationFailed(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    attemptID: payload["attempt_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? "",
                    failureReason: payload["failure_reason"] as? String ?? ""
                ))
            case "ci_remediation_abandoned":
                emit(.ciRemediationAbandoned(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    attemptID: payload["attempt_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? "",
                    failureReason: payload["failure_reason"] as? String ?? ""
                ))
            case "ci_remediation_exhausted":
                emit(.ciRemediationExhausted(
                    productID: payload["product_id"] as? String ?? "",
                    workItemID: payload["work_item_id"] as? String ?? "",
                    prURL: payload["pr_url"] as? String ?? "",
                    attemptsUsed: (payload["attempts_used"] as? NSNumber)?.intValue ?? 0,
                    budget: (payload["budget"] as? NSNumber)?.intValue ?? 0
                ))
            case "feature_flags_list":
                let raw = payload["flags"] as? [[String: Any]] ?? []
                let flags = raw.compactMap(parseFeatureFlag)
                emit(.featureFlagsList(flags: flags))
            case "feature_flag_set":
                let name = payload["name"] as? String ?? ""
                let enabled = (payload["enabled"] as? NSNumber)?.boolValue ?? false
                if !name.isEmpty {
                    emit(.featureFlagSet(name: name, enabled: enabled))
                }
            case "engine_health_result":
                let report = payload["report"] as? [String: Any] ?? [:]
                let apiKeyPresent = (report["anthropic_api_key_present"] as? NSNumber)?.boolValue ?? false
                let rawIssues = report["issues"] as? [[String: Any]] ?? []
                let issues = rawIssues.compactMap(parseEngineHealthIssue)
                emit(.engineHealthResult(apiKeyPresent: apiKeyPresent, issues: issues))
            case "settings_list":
                let raw = payload["settings"] as? [[String: Any]] ?? []
                let settings = raw.compactMap(parseEngineSetting)
                emit(.settingsList(settings: settings))
            case "setting_set":
                let key = payload["key"] as? String ?? ""
                let enabled = (payload["enabled"] as? NSNumber)?.boolValue ?? false
                if !key.isEmpty {
                    emit(.settingSet(key: key, enabled: enabled))
                }
            case "metrics_list_live_result":
                let raw = payload["entries"] as? [[String: Any]] ?? []
                let entries = raw.compactMap(parseEngineMetric)
                emit(.metricsListLiveResult(entries: entries))
            case "attention_items_for_work_item_list":
                let workItemID = payload["work_item_id"] as? String ?? ""
                let raw = payload["items"] as? [[String: Any]] ?? []
                let items = raw.compactMap(parseAttentionItem)
                if !workItemID.isEmpty {
                    emit(.attentionItemsForWorkItemList(workItemID: workItemID, items: items))
                }
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

        var externalTrackerConfigString: String? = nil
        if let configObj = payload["external_tracker_config"],
           !(configObj is NSNull),
           let data = try? JSONSerialization.data(withJSONObject: configObj) {
            externalTrackerConfigString = String(data: data, encoding: .utf8)
        }
        return WorkProduct(
            id: id,
            name: name,
            slug: slug,
            description: description,
            repoRemoteURL: payload["repo_remote_url"] as? String,
            status: status,
            createdAt: createdAt,
            updatedAt: updatedAt,
            externalTrackerKind: payload["external_tracker_kind"] as? String,
            externalTrackerConfig: externalTrackerConfigString
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
            designDocPath: payload["design_doc_path"] as? String,
            shortID: (payload["short_id"] as? NSNumber)?.intValue
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
            createdVia: (payload["created_via"] as? String) ?? "unknown",
            repoRemoteURL: payload["repo_remote_url"] as? String,
            blockedReason: payload["blocked_reason"] as? String,
            blockedAttemptID: payload["blocked_attempt_id"] as? String,
            shortID: (payload["short_id"] as? NSNumber)?.intValue,
            autostart: (payload["autostart"] as? Bool) ?? false,
            ciRequiredState: payload["ci_required_state"] as? String,
            ciRequiredDetail: payload["ci_required_detail"] as? String,
            reviewRequiredState: payload["review_required_state"] as? String,
            reviewRequiredDetail: payload["review_required_detail"] as? String,
            prStatePolledAt: payload["pr_state_polled_at"] as? String,
            mergeQueueState: payload["merge_queue_state"] as? String,
            externalRef: parseExternalRef(payload["external_ref"])
        )
    }

    private func parseExternalRef(_ value: Any?) -> WorkItemExternalRef? {
        guard let dict = value as? [String: Any],
              let kind = dict["kind"] as? String,
              let canonicalID = dict["canonical_id"] as? String,
              let webURL = dict["web_url"] as? String
        else { return nil }
        var rawString = "{}"
        if let rawObj = dict["raw"],
           !(rawObj is NSNull),
           let data = try? JSONSerialization.data(withJSONObject: rawObj) {
            rawString = String(data: data, encoding: .utf8) ?? "{}"
        }
        return WorkItemExternalRef(
            kind: kind,
            canonicalID: canonicalID,
            raw: rawString,
            webURL: webURL,
            syncedAt: dict["synced_at"] as? String,
            unboundAt: dict["unbound_at"] as? String
        )
    }

    private func parseConflictResolution(_ payload: [String: Any]) -> WorkConflictResolution? {
        guard let id = payload["id"] as? String,
              let productID = payload["product_id"] as? String,
              let workItemID = payload["work_item_id"] as? String,
              let prURL = payload["pr_url"] as? String,
              let headBranch = payload["head_branch"] as? String,
              let baseBranch = payload["base_branch"] as? String,
              let status = payload["status"] as? String,
              let createdAt = payload["created_at"] as? String
        else {
            return nil
        }
        let prNumber = (payload["pr_number"] as? NSNumber)?.intValue ?? 0
        return WorkConflictResolution(
            id: id,
            productID: productID,
            workItemID: workItemID,
            prURL: prURL,
            prNumber: prNumber,
            headBranch: headBranch,
            baseBranch: baseBranch,
            baseSHAAtTrigger: payload["base_sha_at_trigger"] as? String,
            headSHABefore: payload["head_sha_before"] as? String,
            headSHAAfter: payload["head_sha_after"] as? String,
            status: status,
            failureReason: payload["failure_reason"] as? String,
            cubeLeaseID: payload["cube_lease_id"] as? String,
            cubeWorkspaceID: payload["cube_workspace_id"] as? String,
            workerID: payload["worker_id"] as? String,
            conflictDiagnosis: payload["conflict_diagnosis"] as? String,
            createdAt: createdAt,
            startedAt: payload["started_at"] as? String,
            finishedAt: payload["finished_at"] as? String
        )
    }

    private func parseCiRemediation(_ payload: [String: Any]) -> WorkCiRemediation? {
        guard let id = payload["id"] as? String,
              let productID = payload["product_id"] as? String,
              let workItemID = payload["work_item_id"] as? String,
              let prURL = payload["pr_url"] as? String,
              let headBranch = payload["head_branch"] as? String,
              let headSHAAtTrigger = payload["head_sha_at_trigger"] as? String,
              let attemptKind = payload["attempt_kind"] as? String,
              let failedChecks = payload["failed_checks"] as? String,
              let status = payload["status"] as? String,
              let createdAt = payload["created_at"] as? String
        else {
            return nil
        }
        let prNumber = (payload["pr_number"] as? NSNumber)?.intValue ?? 0
        let consumesBudget = (payload["consumes_budget"] as? NSNumber)?.intValue ?? 0
        return WorkCiRemediation(
            id: id,
            productID: productID,
            workItemID: workItemID,
            prURL: prURL,
            prNumber: prNumber,
            headBranch: headBranch,
            headSHAAtTrigger: headSHAAtTrigger,
            headSHAAfter: payload["head_sha_after"] as? String,
            attemptKind: attemptKind,
            consumesBudget: consumesBudget,
            failedChecks: failedChecks,
            triageClass: payload["triage_class"] as? String,
            logExcerpt: payload["log_excerpt"] as? String,
            status: status,
            failureReason: payload["failure_reason"] as? String,
            cubeLeaseID: payload["cube_lease_id"] as? String,
            cubeWorkspaceID: payload["cube_workspace_id"] as? String,
            workerID: payload["worker_id"] as? String,
            createdAt: createdAt,
            startedAt: payload["started_at"] as? String,
            finishedAt: payload["finished_at"] as? String
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


    private func parseAttentionItem(_ payload: [String: Any]) -> WorkAttentionItem? {
        guard let data = try? JSONSerialization.data(withJSONObject: payload),
              let item = try? JSONDecoder().decode(WorkAttentionItem.self, from: data)
        else {
            return nil
        }
        return item
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

    private func parseFeatureFlag(_ payload: [String: Any]) -> FeatureFlag? {
        guard
            let name = payload["name"] as? String,
            !name.isEmpty,
            let description = payload["description"] as? String,
            let category = payload["category"] as? String,
            let defaultEnabled = (payload["default_enabled"] as? NSNumber)?.boolValue,
            let enabled = (payload["enabled"] as? NSNumber)?.boolValue
        else {
            return nil
        }
        return FeatureFlag(
            name: name,
            description: description,
            category: category,
            defaultEnabled: defaultEnabled,
            enabled: enabled
        )
    }

    private func parseEngineHealthIssue(_ payload: [String: Any]) -> EngineHealthIssue? {
        guard
            let kind = payload["kind"] as? String,
            !kind.isEmpty,
            let severity = payload["severity"] as? String,
            let title = payload["title"] as? String,
            let body = payload["body"] as? String
        else {
            return nil
        }
        return EngineHealthIssue(kind: kind, severity: severity, title: title, body: body)
    }

    private func parseEngineSetting(_ payload: [String: Any]) -> EngineSetting? {
        guard
            let key = payload["key"] as? String,
            !key.isEmpty,
            let description = payload["description"] as? String,
            let defaultEnabled = (payload["default_enabled"] as? NSNumber)?.boolValue,
            let enabled = (payload["enabled"] as? NSNumber)?.boolValue
        else {
            return nil
        }
        return EngineSetting(
            key: key,
            description: description,
            defaultEnabled: defaultEnabled,
            enabled: enabled
        )
    }

    private func parseEngineMetric(_ payload: [String: Any]) -> EngineMetric? {
        guard
            let name = payload["name"] as? String,
            !name.isEmpty,
            let description = payload["description"] as? String,
            let kind = payload["kind"] as? String,
            let value = (payload["value"] as? NSNumber)?.int64Value,
            let timestampMs = (payload["timestamp_ms"] as? NSNumber)?.int64Value
        else {
            return nil
        }
        let stale = (payload["stale"] as? NSNumber)?.boolValue ?? false
        return EngineMetric(
            name: name,
            description: description,
            kind: kind,
            value: value,
            timestampMs: timestampMs,
            stale: stale
        )
    }
}
