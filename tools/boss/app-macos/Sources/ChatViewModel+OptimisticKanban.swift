import Foundation

extension ChatViewModel {
    struct DragRefusalNotice: Equatable {
        let taskID: String
        let message: String
    }

    /// Variant of `moveTask` that returns whether the drop was
    /// accepted. Used by the kanban's `dropDestination` so the source
    /// lane can render an inline warning when the engine would refuse
    /// the underlying status change.
    func attemptMoveTask(_ taskID: String, to column: WorkBoardColumnKey) -> Bool {
        guard let task = task(withID: taskID) else { return false }
        let targetStatus = column.targetStatus

        // Dispatch-pending rows (status=todo, autostart=true) show in the
        // Doing column but their status is already "todo" — same as Backlog's
        // targetStatus. The normal status-equality guard would silently refuse
        // the drop. Instead, accept Doing→Backlog for these rows and flip
        // autostart=false; lane routing then moves the card to Backlog.
        let isDispatchPending = task.status == "todo" && task.autostart
        if isDispatchPending && column == .backlog {
            // Dispatch-pending tasks show in Doing (todo+autostart). Flipping
            // autostart=false moves them to Backlog; optimistically reflect that.
            pendingMoveOriginByTaskID[task.id] = .doing
            optimisticColumnByTaskID[task.id] = .backlog
            invalidateWorkCache()
            engine.sendUpdateWorkItem(id: task.id, patch: ["autostart": false])
            return true
        }

        guard task.status != targetStatus else { return false }

        if task.status == "blocked",
           targetStatus != "blocked",
           hasGatingPrereqs(task)
        {
            let count = gatingPrereqs(for: task.id).count
            let plural = count == 1 ? "prerequisite" : "prerequisites"
            dragRefusalNotice = DragRefusalNotice(
                taskID: task.id,
                message: "\(task.name) is gated by \(count) incomplete \(plural) — clear them or remove the edge first."
            )
            scheduleDragRefusalDismiss(for: task.id)
            return false
        }

        moveTask(taskID, to: column)
        return true
    }

    func clearDragRefusal() {
        dragRefusalNotice = nil
    }

    func scheduleDragRefusalDismiss(for taskID: String) {
        Task { [weak self] in
            try? await Task.sleep(nanoseconds: 5_000_000_000)
            await MainActor.run { [weak self] in
                guard let self,
                      self.dragRefusalNotice?.taskID == taskID
                else { return }
                self.dragRefusalNotice = nil
            }
        }
    }

    /// Bounce all unconfirmed in-flight optimistic moves back to their origin
    /// columns and surface an inline notice. Called when `work_error` arrives
    /// while moves are pending, or when `workItemUpdated` reports an unexpected
    /// status (engine silently rejected the transition).
    func bounceBackOptimisticMoves(message: String?) {
        guard !pendingMoveOriginByTaskID.isEmpty else { return }
        let bouncedIDs = Array(pendingMoveOriginByTaskID.keys)
        for id in bouncedIDs {
            optimisticColumnByTaskID.removeValue(forKey: id)
            pendingMoveOriginByTaskID.removeValue(forKey: id)
        }
        invalidateWorkCache()
        if let firstID = bouncedIDs.first, let message {
            dragRefusalNotice = DragRefusalNotice(taskID: firstID, message: message)
            scheduleDragRefusalDismiss(for: firstID)
        }
    }

    /// After the engine's work tree arrives and `tasksByProjectID` reflects
    /// the latest status, clear optimistic overrides for cards whose real
    /// board column now matches the target. Safe to call before the next
    /// SwiftUI render — the cache is already stale, so the first re-read
    /// will see the real `boardColumn` value, which equals the override we
    /// just dropped, producing no visible change.
    func reconcileOptimisticOverrides(from tasks: [WorkTask]) {
        for task in tasks {
            guard optimisticColumnByTaskID[task.id] != nil else { continue }
            let realColumn = realEffectiveBoardColumn(for: task)
            if realColumn == optimisticColumnByTaskID[task.id] {
                // Real state now matches: drop the override, no flicker.
                optimisticColumnByTaskID.removeValue(forKey: task.id)
                pendingMoveOriginByTaskID.removeValue(forKey: task.id)
            }
            // If the real column doesn't match and the move is still pending
            // (pendingMoveOriginByTaskID has an entry), the work_error handler
            // will bounce it when the error arrives. Leave the override in
            // place so the card stays at the optimistic position while we wait.
        }
    }
}
