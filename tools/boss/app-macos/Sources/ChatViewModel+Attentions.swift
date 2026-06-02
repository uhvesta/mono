import Foundation

extension ChatViewModel {
    // MARK: Attentions (attentions.md — Notifications toolbar + window)

    /// Replace the product's group set from a full `list_attention_groups`
    /// reply, bucketing the flat member list by group id and pruning member
    /// entries for groups that dropped out of the (open) list.
    ///
    /// Handles two response kinds:
    /// - Open-list (default): replaces open/partially_answered groups while
    ///   preserving dismissed groups so the "Rejected" section survives reloads.
    /// - Dismissed-list (state="dismissed" request): merges dismissed groups
    ///   into the stored open list.
    ///
    /// Detection is content-based: the engine only returns dismissed-state
    /// groups when state="dismissed" was explicitly requested, so inspecting
    /// the group states is reliable regardless of network arrival order.
    /// `pendingDismissedGroupLoads` is used only for empty responses where
    /// the state cannot be inferred from group content.
    func applyAttentionGroupsList(
        productID: String,
        groups: [AttentionGroup],
        members: [Attention]
    ) {
        let hasPendingDismissed = pendingDismissedGroupLoads[productID, default: 0] > 0
        // Identify dismissed-batch responses by content rather than arrival order:
        // a non-empty response where every group is dismissed can only come from
        // a state="dismissed" request. For empty responses, fall back to the counter.
        let allGroupsDismissed = !groups.isEmpty && groups.allSatisfy(\.isDismissed)
        let isDismissedBatch = allGroupsDismissed || (groups.isEmpty && hasPendingDismissed)
        if isDismissedBatch && hasPendingDismissed {
            let remaining = (pendingDismissedGroupLoads[productID] ?? 1) - 1
            pendingDismissedGroupLoads[productID] = remaining > 0 ? remaining : nil
        }
        let prior = attentionGroupsByProductID[productID] ?? []
        let incomingIDs = Set(groups.map(\.id))

        var bucketed: [String: [Attention]] = [:]
        for member in members {
            bucketed[member.groupID, default: []].append(member)
        }
        for group in groups {
            attentionMembersByGroupID[group.id] =
                (bucketed[group.id] ?? []).sorted { $0.ordinal < $1.ordinal }
        }

        if isDismissedBatch {
            // Dismissed-only response: upsert dismissed groups, keep open ones.
            // Remove member data for dismissed groups that are no longer present.
            let priorDismissedIDs = Set(prior.filter(\.isDismissed).map(\.id))
            for goneID in priorDismissedIDs.subtracting(incomingIDs) {
                attentionMembersByGroupID.removeValue(forKey: goneID)
            }
            let keptOpen = prior.filter { !$0.isDismissed && !incomingIDs.contains($0.id) }
            attentionGroupsByProductID[productID] = keptOpen + groups
        } else {
            // Open-list response: replace open groups, keep dismissed ones so
            // the "Rejected" restore UI survives full reloads.
            let keptDismissed = prior.filter { $0.isDismissed && !incomingIDs.contains($0.id) }
            let openPriorIDs = Set(prior.filter { !$0.isDismissed }.map(\.id))
            for goneID in openPriorIDs.subtracting(incomingIDs) {
                attentionMembersByGroupID.removeValue(forKey: goneID)
            }
            attentionGroupsByProductID[productID] = groups + keptDismissed
        }
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

    /// Restore a dismissed group back to open so the human can re-evaluate it.
    /// The engine resets all skipped/dismissed members to open and replies
    /// with `attention_group_updated`, which `upsertAttentionGroup` picks up.
    ///
    /// Optimistically updates local state first so the view immediately
    /// re-derives from the restored state rather than waiting for the async
    /// engine round-trip. The engine's confirmation overwrites with
    /// authoritative values (including recomputed group state).
    func restoreAttentionGroup(_ groupID: String) {
        for (productID, groups) in attentionGroupsByProductID {
            if let idx = groups.firstIndex(where: { $0.id == groupID }) {
                var updated = groups[idx]
                updated.state = "open"
                var updatedGroups = groups
                updatedGroups[idx] = updated
                attentionGroupsByProductID[productID] = updatedGroups
                break
            }
        }
        if var members = attentionMembersByGroupID[groupID] {
            for i in members.indices
                where members[i].answerState == "skipped" || members[i].answerState == "dismissed" {
                members[i].answerState = "open"
            }
            attentionMembersByGroupID[groupID] = members
        }
        engine.sendRestoreAttentionGroup(id: groupID)
    }

    /// Action a group — produce its downstream artifact (one revision /
    /// design task, or a batch task-create) and close it. Open members are
    /// skipped first ("skip remaining") so the human needn't touch every row.
    func actionAttentionGroup(_ groupID: String) {
        engine.sendActionAttentionGroup(id: groupID, skipUnanswered: true)
    }

    /// Load dismissed groups for a product and merge them into the stored list.
    /// Called alongside `sendListAttentionGroups` so the "Rejected" section
    /// is populated on product select and app reconnect.
    func loadDismissedAttentionGroups(for productID: String) {
        pendingDismissedGroupLoads[productID, default: 0] += 1
        engine.sendListAttentionGroups(productId: productID, state: "dismissed")
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
}
