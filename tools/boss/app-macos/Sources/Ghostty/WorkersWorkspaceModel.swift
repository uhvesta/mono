import Foundation

@MainActor
final class WorkersWorkspaceModel: ObservableObject {
    static let workerSlotCount = 8

    let runtime: GhosttyRuntime
    @Published private(set) var slots: [WorkerSlot]

    init() {
        self.runtime = GhosttyRuntime.shared
        self.slots = (1...Self.workerSlotCount).map { slot in
            WorkerSlot(slotId: slot, idleFlavorCycle: Int.random(in: 0...10_000))
        }
    }

    /// Allocate a free slot, configure a `TerminalPaneSession` with
    /// the given workspace + env + initial input, and return the
    /// slot id + shell pid. Returns `.failure(.noAvailableSlot)` if
    /// all 8 slots are occupied.
    func spawnWorkerPane(_ request: EngineSpawnRequest) -> EngineSpawnResult {
        guard let index = slots.firstIndex(where: { $0.session == nil }) else {
            return .failure(.noAvailableSlot)
        }

        let slotId = slots[index].slotId
        let launchSpec = TerminalLaunchSpec(
            fontSize: 10.0,
            workingDirectory: request.workspacePath,
            initialInput: request.initialInput,
            env: request.env
        )
        let session = TerminalPaneSession(
            id: "run-\(request.runId)",
            role: .worker(slot: slotId),
            launchSpec: launchSpec
        )
        slots[index].session = session
        slots[index].runId = request.runId
        slots[index].summary = request.summary

        // TODO: use proc_listpids(PROC_PPID_ONLY, ...)
        // right after surface init to find the shell pid. For now
        // return 0 and let the registry's ancestor walk fail
        // gracefully — the spawn flow plumbing is what we're proving
        // out at this layer; richer pid correlation lands as a follow-up.
        return .success(slotId: slotId, shellPid: 0)
    }

    /// Release a previously allocated slot. Currently just clears the
    /// session reference so SwiftUI tears down the surface; real
    /// kill-grace handling lands as a follow-up.
    func releaseWorkerPane(slotId: Int, killGraceSeconds: UInt32) -> EngineReleaseResult {
        guard let index = slots.firstIndex(where: { $0.slotId == slotId }) else {
            return .failure(.unknownSlot)
        }
        guard slots[index].session != nil else {
            return .failure(.unknownSlot)
        }
        slots[index].session = nil
        slots[index].runId = nil
        slots[index].summary = nil
        // Re-roll the idle flavor so consecutive idle bouts on the same
        // slot don't show the same line — fresh recreation each time
        // the crew member clocks out.
        slots[index].idleFlavorCycle &+= 1
        return .success
    }

    /// Write text into the slot's libghostty surface as if user-typed.
    /// Used for probe injection (Stop-boundary text from the engine)
    /// and `bossctl agents send`.
    func sendToPane(slotId: Int, text: String) -> EngineSendResult {
        guard let index = slots.firstIndex(where: { $0.slotId == slotId }) else {
            return .failure(.unknownSlot)
        }
        guard let session = slots[index].session else {
            return .failure(.unknownSlot)
        }
        guard let host = session.hostView else {
            return .failure(.internalFailure("pane has no live surface"))
        }
        host.writeText(text)
        return .success
    }
}

struct WorkerSlot: Identifiable, Equatable {
    let slotId: Int
    var session: TerminalPaneSession?
    var runId: String?
    /// Short lowercase gerund phrase the engine generated for this
    /// run (e.g. `"fixing the fencer scraper"`). Rendered in the pane
    /// titlebar as `"<WorkerName> is <phrase>"` in place of `runId`
    /// when present; the runId stays available as a tooltip.
    var summary: String?
    /// Bumped every time the slot re-enters idle so the flavor line
    /// changes between idle bouts; kept stable for the lifetime of a
    /// single bout so renders don't flicker.
    var idleFlavorCycle: Int = 0

    var id: Int { slotId }

    static func == (lhs: WorkerSlot, rhs: WorkerSlot) -> Bool {
        lhs.slotId == rhs.slotId
            && lhs.runId == rhs.runId
            && lhs.summary == rhs.summary
            && lhs.idleFlavorCycle == rhs.idleFlavorCycle
            && lhs.session === rhs.session
    }
}
