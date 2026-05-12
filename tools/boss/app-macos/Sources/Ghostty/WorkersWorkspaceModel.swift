import AppKit
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

    /// Host a worker pane in the slot the engine has claimed for
    /// this worker (`request.slotId`). The engine is the source of
    /// truth for slot allocation: this method honors the requested
    /// slot or fails — it never picks a different slot.
    ///
    /// Returns:
    ///  - `.failure(.internalFailure)` if `slotId` is outside
    ///    `1...workerSlotCount` (engine asked for a slot that
    ///    doesn't exist on this app).
    ///  - `.failure(.slotBusy)` if the requested slot already hosts
    ///    a session (engine and app disagree about what's free —
    ///    the engine should reconcile rather than retry blindly).
    func spawnWorkerPane(_ request: EngineSpawnRequest) -> EngineSpawnResult {
        let requestedSlot = request.slotId
        guard requestedSlot >= 1, requestedSlot <= Self.workerSlotCount,
              let index = slots.firstIndex(where: { $0.slotId == requestedSlot })
        else {
            return .failure(.internalFailure(
                "engine requested slot \(requestedSlot), valid range is 1...\(Self.workerSlotCount)"
            ))
        }
        guard slots[index].session == nil else {
            return .failure(.slotBusy)
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

    /// Type text into the slot's libghostty surface and submit it as
    /// if the user had pasted the body and pressed Return. Used for
    /// probe injection (Stop-boundary text from the engine), `bossctl
    /// agents send`, and the macOS app's intervene affordance.
    ///
    /// The submit step happens inside `submitText` — see its docstring
    /// for why a trailing `\n` inside the payload is not enough to
    /// land the prompt: libghostty's paste path delivers control
    /// characters as input-field content, not as a keystroke.
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
        host.submitText(text)
        return .success
    }

    /// Bring the slot's libghostty surface to first responder and
    /// raise the host window. Mirrors the user-click path in
    /// `GhosttyTerminalHostView.mouseDown` (which also calls
    /// `makeFirstResponder(self)`), then activates the application so
    /// the window is visible if it was minimised or behind another
    /// app. Used by `bossctl agents focus`.
    func focusWorkerPane(slotId: Int) -> EngineFocusResult {
        guard let index = slots.firstIndex(where: { $0.slotId == slotId }) else {
            return .failure(.unknownSlot)
        }
        guard let session = slots[index].session else {
            return .failure(.unknownSlot)
        }
        guard let host = session.hostView else {
            return .failure(.internalFailure("pane has no live surface"))
        }
        guard let window = host.window else {
            // No host window means the pane isn't on screen yet
            // (NSView never moved into a window). The slot is
            // allocated but unrenderable, so refuse instead of
            // silently no-op'ing.
            return .failure(.internalFailure("pane has no host window"))
        }
        NSApp.activate(ignoringOtherApps: true)
        if window.isMiniaturized {
            window.deminiaturize(nil)
        }
        window.makeKeyAndOrderFront(nil)
        window.makeFirstResponder(host)
        return .success
    }

    /// Deliver an Esc keystroke to the slot's libghostty surface —
    /// equivalent to the human pressing Esc with the pane focused.
    /// Routes through the same `ghostty_surface_key` path used by
    /// `keyDown(with:)`, so libghostty's keymap translation produces
    /// the right ESC byte sequence in the pty (and Claude treats it
    /// as an in-flight-turn cancel). Used by `bossctl agents
    /// interrupt`.
    func interruptWorkerPane(slotId: Int) -> EngineInterruptResult {
        guard let index = slots.firstIndex(where: { $0.slotId == slotId }) else {
            return .failure(.unknownSlot)
        }
        guard let session = slots[index].session else {
            return .failure(.unknownSlot)
        }
        guard let host = session.hostView else {
            return .failure(.internalFailure("pane has no live surface"))
        }
        host.sendInterrupt()
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
