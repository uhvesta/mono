import Foundation

/// Holds the engine's `worker.live_states` snapshot in its own
/// observable, so a hook event arriving on every Claude tool turn does
/// not invalidate every view that observes `ChatViewModel`. Only the
/// kanban Doing-column cards and the worker pane titlebar consume
/// this; everything else (toolbar pickers, sidebar, Boss panel)
/// observes only `ChatViewModel` and is unaffected by the high-rate
/// `worker.live_states` traffic.
@MainActor
final class LiveWorkerStateStore: ObservableObject {
    @Published private(set) var byRunID: [String: WorkerLiveState] = [:]
    @Published private(set) var bySlot: [Int: WorkerLiveState] = [:]

    /// Count of workers in an actively-doing-work state: `spawning` or
    /// `working`. Used by the quit-confirmation guard to decide whether
    /// to warn before exiting. Does not include `waitingForInput` or
    /// error/terminal states — those are not "work in flight" for the
    /// purposes of the dialog.
    var activeAgentCount: Int {
        bySlot.values.filter { $0.activity == .spawning || $0.activity == .working }.count
    }

    /// Replace the snapshot with `states`. Skips the publish when the
    /// new snapshot is value-equal to the previous one — a hook event
    /// that nudged `lastEventAt` but left every per-slot field
    /// otherwise unchanged still reaches us, and republishing it would
    /// invalidate every observing card for no visible delta.
    func update(states: [WorkerLiveState]) {
        let newByRunID = Dictionary(uniqueKeysWithValues: states.map { ($0.runId, $0) })
        let newBySlot = Dictionary(uniqueKeysWithValues: states.map { ($0.slotId, $0) })
        if newByRunID != byRunID {
            byRunID = newByRunID
        }
        if newBySlot != bySlot {
            bySlot = newBySlot
        }
    }
}
