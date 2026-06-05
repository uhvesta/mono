import Foundation

extension ChatViewModel {
    // MARK: Attentions (attentions.md — Notifications toolbar + window)

    /// Replace the product's group set from a full `list_attention_groups`
    /// reply, bucketing the flat member list by group id and pruning member
    /// entries for groups that dropped out of the (open) list.
    func applyAttentionGroupsList(
        productID: String,
        groups: [AttentionGroup],
        members: [Attention]
    ) {
        let priorIDs = Set((attentionGroupsByProductID[productID] ?? []).map(\.id))
        let nextIDs = Set(groups.map(\.id))
        for goneID in priorIDs.subtracting(nextIDs) {
            attentionMembersByGroupID.removeValue(forKey: goneID)
        }
        var bucketed: [String: [Attention]] = [:]
        for member in members {
            bucketed[member.groupID, default: []].append(member)
        }
        for group in groups {
            attentionMembersByGroupID[group.id] =
                (bucketed[group.id] ?? []).sorted { $0.ordinal < $1.ordinal }
        }
        attentionGroupsByProductID[productID] = groups
    }

    /// Insert or replace a group within its product bucket (live-update path).
    func upsertAttentionGroup(_ group: AttentionGroup) {
        var list = attentionGroupsByProductID[group.productID] ?? []
        if let idx = list.firstIndex(where: { $0.id == group.id }) {
            list[idx] = group
        } else {
            list.append(group)
        }
        attentionGroupsByProductID[group.productID] = list
    }

    /// Insert or replace one member within its group's row list.
    func upsertAttentionMember(_ member: Attention) {
        var list = attentionMembersByGroupID[member.groupID] ?? []
        if let idx = list.firstIndex(where: { $0.id == member.id }) {
            list[idx] = member
        } else {
            list.append(member)
        }
        attentionMembersByGroupID[member.groupID] = list.sorted { $0.ordinal < $1.ordinal }
    }

    /// Record an answer for a question member (`yes`/`no`, a chosen value, or
    /// free text). The engine replies with `attention_group_updated`.
    func answerAttention(_ attentionID: String, answer: String) {
        engine.sendAnswerAttention(id: attentionID, answer: answer, skip: false, dismiss: false)
    }

    /// Accept a followup member (mark it `answered` with no value) so it is
    /// included when the group is actioned.
    func acceptFollowup(_ attentionID: String) {
        engine.sendAnswerAttention(id: attentionID, answer: nil, skip: false, dismiss: false)
    }

    /// Mark a member `skipped` — a rejected followup or a question the human
    /// chooses not to answer. Skipped members contribute nothing on action.
    func skipAttention(_ attentionID: String) {
        engine.sendAnswerAttention(id: attentionID, answer: nil, skip: true, dismiss: false)
    }

    /// Dismiss a single member (`atn_…`) without producing anything.
    func dismissAttentionMember(_ attentionID: String) {
        engine.sendDismissAttention(id: attentionID, reason: nil)
    }

    /// Dismiss a whole group (`atg_…`) without producing anything.
    func dismissAttentionGroup(_ groupID: String) {
        engine.sendDismissAttention(id: groupID, reason: nil)
    }

    /// Action a group — produce its downstream artifact (one revision /
    /// design task, or a batch task-create) and close it. Open members are
    /// skipped first ("skip remaining") so the human needn't touch every row.
    func actionAttentionGroup(_ groupID: String) {
        engine.sendActionAttentionGroup(id: groupID, skipUnanswered: true)
    }

    /// Jump a group's association into view: a task association reveals its
    /// kanban card; a project association focuses that project's board.
    func revealAttentionAssociation(_ group: AttentionGroup) {
        if let taskID = group.associationTaskID,
           let task = task(withID: taskID) {
            revealWorkCard(taskID, productID: task.productID)
        } else if let projectID = group.associationProjectID {
            revealAttentionProject(projectID)
        }
    }

    /// Focus a project's board in the kanban (Work mode, product selected,
    /// project filter narrowed to the one project).
    func revealAttentionProject(_ projectID: String) {
        guard let project = project(withID: projectID) else { return }
        setNavigationMode(.work)
        if currentSelectedProductID != project.productID {
            selectWorkProduct(project.productID)
        }
        selectedProjectFilterIDs = [projectID]
        persistProjectFilterIDs()
    }

    /// The task a group is associated with, or `nil` for project associations.
    func attentionAssociationTask(_ group: AttentionGroup) -> WorkTask? {
        guard let taskID = group.associationTaskID else { return nil }
        return task(withID: taskID)
    }

    /// Short display label for a group's association — `"T34"` / `"P12"` /
    /// a project name / `"Open"` when neither resolves.
    func attentionAssociationLabel(_ group: AttentionGroup) -> String {
        if let taskID = group.associationTaskID {
            if let task = task(withID: taskID), let shortID = task.shortID {
                return "T\(shortID)"
            }
            return "Task"
        }
        if let projectID = group.associationProjectID {
            if let project = project(withID: projectID) {
                return project.shortID.map { "P\($0)" } ?? project.name
            }
            return "Project"
        }
        return "Open"
    }

    /// Reveal a produced revision / task card after a group is actioned.
    func revealProducedArtifact(_ ref: ProducedArtifactRef) {
        guard let task = task(withID: ref.taskID) else { return }
        revealWorkCard(ref.taskID, productID: task.productID)
    }

    /// Open question groups whose `source_doc_path` matches the given absolute
    /// file path. Checked across all known products so the design-renderer
    /// window (which may display a doc from any product) still finds its
    /// groups regardless of which product is currently selected. The match
    /// is suffix-based: the group's repo-relative `source_doc_path` must
    /// appear as the trailing component of `absolutePath`.
    func openQuestionGroupsForDocPath(_ absolutePath: String) -> [AttentionGroup] {
        attentionGroupsByProductID.values
            .flatMap { $0 }
            .filter { group in
                guard group.kind == "question",
                      group.isOpen,
                      let sourceDocPath = group.sourceDocPath,
                      !sourceDocPath.isEmpty
                else { return false }
                return absolutePath == sourceDocPath
                    || absolutePath.hasSuffix("/\(sourceDocPath)")
            }
            .sorted { $0.createdAt > $1.createdAt }
    }

    /// Open the design doc a question group is about, reusing the project
    /// design-doc viewer when the group's association project resolves.
    func openAttentionDesignDoc(_ group: AttentionGroup) {
        guard let projectID = group.associationProjectID,
              let project = project(withID: projectID)
        else { return }
        openProjectDesignDoc(project)
    }

    /// Engine-tab entry point: ask the engine for the current attempt
    /// list. Idempotent — the view-model just overwrites the array
    /// when the reply lands.
    func refreshConflictResolutions() {
        engine.sendListConflictResolutions(limit: 200)
    }

    /// Mirror of [[refreshConflictResolutions]] for the CI subsystem
    /// (design Phase 11 #37). Idempotent.
    func refreshCiRemediations() {
        engine.sendListCiRemediations(limit: 200)
    }

    /// Refresh both engine-tab attempt subsystems together — the
    /// activity log surfaces a single button that should pull every
    /// row kind in one call.
    func refreshEngineAttempts() {
        engine.sendListConflictResolutions(limit: 200)
        engine.sendListCiRemediations(limit: 200)
    }

    // MARK: - Automation actions

    /// Load automations for the currently selected product. No-op when
    /// disconnected or no product is selected.
    func refreshAutomations() {
        guard isConnected, let productID = currentSelectedProductID else { return }
        automationsFetchStateByProductID[productID] = .loading
        engine.sendListAutomations(productId: productID)
    }

    func createAutomation(
        productID: String,
        name: String,
        cron: String,
        timezone: String,
        standingInstruction: String,
        openTaskLimit: Int = 1,
        enabled: Bool = true,
        repoRemoteURL: String? = nil
    ) {
        engine.sendCreateAutomation(
            productId: productID,
            name: name,
            cron: cron,
            timezone: timezone,
            standingInstruction: standingInstruction,
            openTaskLimit: openTaskLimit,
            enabled: enabled,
            repoRemoteURL: repoRemoteURL
        )
    }

    func updateAutomation(
        id: String,
        name: String? = nil,
        cron: String? = nil,
        timezone: String? = nil,
        standingInstruction: String? = nil,
        openTaskLimit: Int? = nil
    ) {
        var patch: [String: Any] = [:]
        if let name { patch["name"] = name }
        if let cron, let timezone {
            patch["trigger"] = ["kind": "schedule", "cron": cron, "timezone": timezone]
        }
        if let standingInstruction { patch["standing_instruction"] = standingInstruction }
        if let openTaskLimit { patch["open_task_limit"] = openTaskLimit }
        guard !patch.isEmpty else { return }
        engine.sendUpdateAutomation(id: id, patch: patch)
    }

    func enableAutomation(id: String) {
        engine.sendEnableAutomation(id: id)
    }

    func disableAutomation(id: String) {
        engine.sendDisableAutomation(id: id)
    }

    func deleteAutomation(id: String) {
        if selectedAutomationID == id {
            selectedAutomationID = nil
        }
        engine.sendDeleteAutomation(id: id)
    }

    // MARK: - Editorial controls actions

    /// Open the editorial controls sheet for the given product.
    func openEditorialControls(productID: String) {
        editorialControlsProductID = productID
        loadEditorialActions(productID: productID)
    }

    /// Reload the editorial-action audit rows for a product.
    func loadEditorialActions(productID: String) {
        guard isConnected else { return }
        editorialActionsFetchStateByProductID[productID] = .loading
        engine.sendListEditorialActions(productId: productID, limit: 50)
    }

    /// Persist new editorial rules for a product. Pass `nil` to clear all rules.
    func setProductEditorialRules(productID: String, rules: EditorialRules?) {
        engine.sendSetProductEditorialRules(productId: productID, rules: rules)
    }
}
