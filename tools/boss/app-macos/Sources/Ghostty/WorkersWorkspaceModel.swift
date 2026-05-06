import Foundation

@MainActor
final class WorkersWorkspaceModel: ObservableObject {
    static let workerSlotCount = 8

    let runtime: GhosttyRuntime
    @Published private(set) var slots: [WorkerSlot]

    init() {
        self.runtime = GhosttyRuntime.shared
        self.slots = (1...Self.workerSlotCount).map { slot in
            WorkerSlot(slotId: slot)
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
            initialInput: request.initialInput
        )
        let session = TerminalPaneSession(
            id: "run-\(request.runId)",
            role: .worker(slot: slotId),
            launchSpec: launchSpec
        )
        slots[index].session = session
        slots[index].runId = request.runId

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

    var id: Int { slotId }

    static func == (lhs: WorkerSlot, rhs: WorkerSlot) -> Bool {
        lhs.slotId == rhs.slotId && lhs.runId == rhs.runId && lhs.session === rhs.session
    }
}
