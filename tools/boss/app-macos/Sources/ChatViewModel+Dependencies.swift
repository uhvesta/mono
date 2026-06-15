import Foundation

extension ChatViewModel {
    /// All prereqs per task, resolved to display rows. Read O(1) per card;
    /// rebuilt lazily on first read after invalidation (see `rebuildPrereqCache`).
    var dependencyPrereqsByTaskID: [String: [WorkDependencyRow]] {
        if cachedDependencyPrereqs == nil { rebuildPrereqCache() }
        return cachedDependencyPrereqs ?? [:]
    }

    /// Unsatisfied (still-gating) prereqs per task. Read O(1) per card.
    var gatingPrereqsByTaskID: [String: [WorkDependencyRow]] {
        if cachedGatingPrereqs == nil { rebuildPrereqCache() }
        return cachedGatingPrereqs ?? [:]
    }

    /// Build the O(1) id → work-item index from the four task buckets. First
    /// writer wins, preserving the bucket-search precedence the previous
    /// linear `task(withID:)` had (ids are unique across buckets, so this only
    /// matters defensively). Buckets, in order:
    /// - `tasksByProjectID` / `choresByProductID`: project-laned tasks/chores.
    /// - `productLevelRevisionsByProductID`: chore-parented revisions, which
    ///   live in neither bucket above (issue #789).
    /// - `productLevelTasksByProductID`: product-level investigations and any
    ///   other product-level kind (issue #886).
    func rebuildTaskIndex() {
        var index: [String: WorkTask] = [:]
        for tasks in tasksByProjectID.values {
            for task in tasks where index[task.id] == nil { index[task.id] = task }
        }
        for chores in choresByProductID.values {
            for chore in chores where index[chore.id] == nil { index[chore.id] = chore }
        }
        for revisions in productLevelRevisionsByProductID.values {
            for revision in revisions where index[revision.id] == nil { index[revision.id] = revision }
        }
        for tasks in productLevelTasksByProductID.values {
            for task in tasks where index[task.id] == nil { index[task.id] = task }
        }
        taskIndexByID = index
    }

    /// Resolve the human-readable label for the rows currently gating
    /// `task` — i.e. its incomplete `blocks` prerequisites. Used by
    /// the kanban card to show "Blocked by <prereq title>" under the
    /// task name when the engine has parked the row in `blocked`. The
    /// caller is expected to gate on `task.status == "blocked"` so we
    /// don't compute this for cards that aren't rendering the badge.
    func blockedByLabel(for task: WorkTask) -> String? {
        let edges = dependenciesByProductID[task.productID] ?? []
        guard !edges.isEmpty else { return nil }
        let names: [String] = edges.compactMap { edge in
            guard edge.dependentID == task.id, edge.relation == "blocks" else {
                return nil
            }
            guard let name = workItemName(for: edge.prerequisiteID),
                  !isWorkItemSatisfied(edge.prerequisiteID)
            else {
                return nil
            }
            return name
        }
        guard !names.isEmpty else { return nil }
        return names.joined(separator: ", ")
    }

    /// All `blocks` prereqs for `task` joined against the work tree,
    /// rendered in card-detail and tooltip order. Includes already-
    /// satisfied edges so the popover can show the full picture (the
    /// chain badge tooltip and the auto-block predicate filter further
    /// for "incomplete" only).
    func dependencyPrereqs(for taskID: String) -> [WorkDependencyRow] {
        guard let productID = task(withID: taskID)?.productID
            ?? project(withID: taskID)?.productID
        else {
            return []
        }
        let edges = dependenciesByProductID[productID] ?? []
        return edges
            .filter { $0.dependentID == taskID && $0.relation == "blocks" }
            .map { workDependencyRow(forID: $0.prerequisiteID) }
    }

    /// All `blocks` dependents of `taskID`. Used by the card detail
    /// Dependencies subsection to show "what does this gate?".
    func dependencyDependents(for taskID: String) -> [WorkDependencyRow] {
        guard let productID = task(withID: taskID)?.productID
            ?? project(withID: taskID)?.productID
        else {
            return []
        }
        let edges = dependenciesByProductID[productID] ?? []
        return edges
            .filter { $0.prerequisiteID == taskID && $0.relation == "blocks" }
            .map { workDependencyRow(forID: $0.dependentID) }
    }

    /// Subset of `dependencyPrereqs` that are still gating the row —
    /// i.e. not yet in a satisfied status. Drives the chain badge's
    /// hover tooltip ("gated by …") and the auto-block predicate.
    func gatingPrereqs(for taskID: String) -> [WorkDependencyRow] {
        dependencyPrereqs(for: taskID).filter { !isWorkItemSatisfied($0.id) }
    }

    /// True iff the engine parked the row in `blocked` (rather than the
    /// user choosing it). The chain badge appears only for these rows
    /// per design Q7 — manual blocks already get the lane and would
    /// double up with the icon.
    func isAutoBlocked(_ task: WorkTask) -> Bool {
        task.status == "blocked"
            && task.lastStatusActor == "engine"
            && !gatingPrereqs(for: task.id).isEmpty
    }

    /// True iff the row currently has at least one unsatisfied gating
    /// prereq. Drag refusal keys on this rather than `lastStatusActor`
    /// because the engine refuses *any* manual move out of `blocked`
    /// while gated, regardless of who set the status last (Q4).
    func hasGatingPrereqs(_ task: WorkTask) -> Bool {
        !gatingPrereqs(for: task.id).isEmpty
    }

    // MARK: - Dependency badge hover / frontier highlight

    /// The parent task ID when the currently selected card is a revision.
    /// Drives the green-border highlight on the parent card so clicking a
    /// revision lights up the task it amends — the inverse of the "In
    /// revision" badge hover which highlights child revisions.
    var selectedRevisionParentID: String? {
        guard let task = selectedTask, task.kind == "revision" else { return nil }
        return task.parentTaskId
    }

    /// Called when the pointer enters or leaves a Dependency badge on a
    /// kanban card. On enter, computes the actionable prerequisite
    /// frontier — the set of reachable, unblocked, open prerequisites —
    /// and publishes them so every frontier card gets a transient
    /// highlight. On leave (`nil`), clears the set.
    func setDepBadgeHover(_ taskID: String?) {
        guard let taskID else {
            depFrontierHighlightIDs = []
            return
        }
        depFrontierHighlightIDs = actionablePrereqFrontier(for: taskID)
    }

    /// Called when the pointer enters or leaves an "In revision" badge on a
    /// kanban card. On enter, collects all active (todo/active) revision tasks
    /// whose `parentTaskId` matches `taskID` and highlights them with the same
    /// green-border overlay used by the dep frontier. On leave (`nil`), clears.
    func setRevisionBadgeHover(_ taskID: String?) {
        guard let taskID else {
            revisionHighlightIDs = []
            return
        }
        let matches: (WorkTask) -> Bool = {
            $0.kind == "revision"
                && $0.parentTaskId == taskID
                && ($0.status == "todo" || $0.status == "active")
        }
        var ids: Set<String> = []
        for tasks in tasksByProjectID.values {
            ids.formUnion(tasks.filter(matches).map(\.id))
        }
        for revisions in productLevelRevisionsByProductID.values {
            ids.formUnion(revisions.filter(matches).map(\.id))
        }
        revisionHighlightIDs = ids
    }

    /// Transitively walks the prerequisite DAG from `taskID` and
    /// returns the IDs of every node that is:
    ///   - reachable (transitively reachable through `blocks` edges),
    ///   - unblocked (no incomplete prerequisites of its own), AND
    ///   - open (not in a terminal / satisfied status).
    ///
    /// These are the "next actionable" items: completing them advances
    /// the dependency frontier one step closer to unblocking the chore.
    /// Deeper nodes that are still blocked themselves are traversed but
    /// not added to the frontier (they aren't actionable yet); once they
    /// unblock, the frontier advances through them automatically on the
    /// next hover.
    func actionablePrereqFrontier(for taskID: String) -> Set<String> {
        guard let productID = task(withID: taskID)?.productID else { return [] }
        let edges = dependenciesByProductID[productID] ?? []

        var frontier: Set<String> = []
        var visited: Set<String> = [taskID]
        var queue: [String] = [taskID]

        while !queue.isEmpty {
            let current = queue.removeFirst()
            let prereqIDs = edges
                .filter { $0.dependentID == current && $0.relation == "blocks" }
                .map { $0.prerequisiteID }

            for prereqID in prereqIDs {
                guard !visited.contains(prereqID) else { continue }
                visited.insert(prereqID)

                // Skip already-satisfied (terminal) items — they aren't open.
                guard !isWorkItemSatisfied(prereqID) else { continue }

                // An unblocked, open item is exactly what "actionable" means.
                if gatingPrereqs(for: prereqID).isEmpty {
                    frontier.insert(prereqID)
                } else {
                    // Still blocked itself — keep walking its prerequisites
                    // so we can find the true frontier deeper in the DAG.
                    queue.append(prereqID)
                }
            }
        }

        return frontier
    }

    /// Rebuild the gating/dependency prereq caches from current edge and
    /// task/project data. Called synchronously from `invalidateWorkCache()`
    /// and `dependenciesByProductID.didSet` so the caches are always current
    /// before the next SwiftUI render pass. O(total edges) per call.
    func rebuildPrereqCache() {
        var gating: [String: [WorkDependencyRow]] = [:]
        var prereqs: [String: [WorkDependencyRow]] = [:]
        for edges in dependenciesByProductID.values {
            // Group block-edges by dependentID in one pass over this product's edges.
            var byDependent: [String: [WorkItemDependency]] = [:]
            for edge in edges where edge.relation == "blocks" {
                byDependent[edge.dependentID, default: []].append(edge)
            }
            for (taskID, taskEdges) in byDependent {
                let rows = taskEdges.map { workDependencyRow(forID: $0.prerequisiteID) }
                prereqs[taskID] = rows
                // Filter on the status already resolved into each row rather
                // than calling isWorkItemSatisfied(_:), which would look the
                // same id up a second time — halving the lookups per edge.
                gating[taskID] = rows.filter { !isWorkItemRowSatisfied($0) }
            }
        }
        cachedDependencyPrereqs = prereqs
        cachedGatingPrereqs = gating
    }

    /// Row-status equivalent of `isWorkItemSatisfied(_:)`: a task/chore is
    /// satisfied at `done`; a project at `done` or `archived`. An unresolved
    /// prereq (kind `.unknown`, status `"unknown"`) is treated as unsatisfied,
    /// matching the id-based helper's nil-lookup behaviour.
    private func isWorkItemRowSatisfied(_ row: WorkDependencyRow) -> Bool {
        switch row.kind {
        case .project:
            return row.status == "done" || row.status == "archived"
        case .task, .chore, .unknown:
            return row.status == "done"
        }
    }

    private func workDependencyRow(forID id: String) -> WorkDependencyRow {
        if id.hasPrefix("proj_") {
            if let project = project(withID: id) {
                return WorkDependencyRow(
                    id: project.id,
                    title: project.name,
                    status: project.status,
                    kind: .project
                )
            }
        } else if let task = task(withID: id) {
            return WorkDependencyRow(
                id: task.id,
                title: task.name,
                status: task.status,
                kind: task.isChore ? .chore : .task
            )
        }
        return WorkDependencyRow(id: id, title: id, status: "unknown", kind: .unknown)
    }

    private func workItemName(for id: String) -> String? {
        if id.hasPrefix("proj_") {
            return project(withID: id)?.name
        }
        return task(withID: id)?.name
    }

    /// Mirrors the engine's `status_satisfies` rule: a task/chore is
    /// satisfied at `done`; a project is satisfied at `done` or
    /// `archived`. Used to hide already-finished prereqs from the
    /// "Blocked by …" label on the off-chance an edge survives a
    /// status change momentarily.
    private func isWorkItemSatisfied(_ id: String) -> Bool {
        if id.hasPrefix("proj_") {
            guard let status = project(withID: id)?.status else { return false }
            return status == "done" || status == "archived"
        }
        guard let status = task(withID: id)?.status else { return false }
        return status == "done"
    }
}
