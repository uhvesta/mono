import Foundation
import GhosttyKit

enum ClaudeMonitorState: Equatable {
    case unavailable
    case notDetected
    case ready
    case working

    var label: String {
        switch self {
        case .unavailable:
            "Claude Unknown"
        case .notDetected:
            "Claude Not Detected"
        case .ready:
            "Claude Ready"
        case .working:
            "Claude Is Working"
        }
    }
}

struct TerminalLaunchSpec {
    let fontSize: Float32
    let workingDirectory: String
    let initialInput: String
    /// Env vars to set on the spawned shell, layered over the app's
    /// inherited env. The engine builds a strict allowlist for worker
    /// spawns (sanitized PATH excluding `bossctl`, plus
    /// `BOSS_EVENTS_SOCKET` / `BOSS_LEASE_ID`); the Boss pane passes
    /// `bossSessionEnv()` to set `BOSS_BIN_DIR`, `BOSS_BIN`, and an
    /// initial PATH prepend; ad-hoc test panes pass an empty array.
    let env: [(String, String)]

    init(
        fontSize: Float32,
        workingDirectory: String,
        initialInput: String,
        env: [(String, String)] = []
    ) {
        self.fontSize = fontSize
        self.workingDirectory = workingDirectory
        self.initialInput = initialInput
        self.env = env
    }
}

struct ClaudeMonitorSnapshot {
    let tail: String
    let claudeVisible: Bool
    let busy: Bool
    let promptVisible: Bool
    let promptLine: String?
    let starting: Bool
}

struct ClaudeMonitorTracker {
    private let idleDebouncePolls = 2
    private var lastTail: String?
    private var lastPromptLine: String?
    private var turnInFlight = false
    private var stablePromptPolls = 0

    mutating func reset() {
        lastTail = nil
        lastPromptLine = nil
        turnInFlight = false
        stablePromptPolls = 0
    }

    mutating func evaluate(_ snapshot: ClaudeMonitorSnapshot?) -> ClaudeMonitorState {
        guard let snapshot else {
            reset()
            return .unavailable
        }

        guard snapshot.claudeVisible else {
            reset()
            return .notDetected
        }

        let tailChanged = lastTail.map { $0 != snapshot.tail } ?? false
        let promptJustSubmitted =
            !turnInFlight &&
            tailChanged &&
            promptHasInput(lastPromptLine) &&
            snapshot.promptVisible &&
            !promptHasInput(snapshot.promptLine)

        defer {
            lastTail = snapshot.tail
            lastPromptLine = snapshot.promptLine
        }

        if snapshot.busy || snapshot.starting {
            turnInFlight = true
            stablePromptPolls = 0
            return .working
        }

        if promptJustSubmitted {
            turnInFlight = true
            stablePromptPolls = 0
        }

        if snapshot.promptVisible {
            guard turnInFlight else {
                stablePromptPolls = 0
                return .ready
            }

            stablePromptPolls = tailChanged ? 1 : stablePromptPolls + 1
            if stablePromptPolls >= idleDebouncePolls {
                turnInFlight = false
                stablePromptPolls = 0
                return .ready
            }

            return .working
        }

        turnInFlight = true
        stablePromptPolls = 0
        return .working
    }

    private func promptHasInput(_ promptLine: String?) -> Bool {
        guard let promptLine else { return false }
        let trimmed = promptLine.trimmingCharacters(in: .whitespaces)
        guard trimmed.hasPrefix("❯") else { return false }
        return !trimmed.dropFirst().trimmingCharacters(in: .whitespaces).isEmpty
    }
}

enum PaneRole: Equatable {
    case boss
    case worker(slot: Int)

    var defaultTitle: String {
        switch self {
        case .boss: "Picard"
        case .worker(let slot): WorkerNames.name(forSlot: slot)
        }
    }
}

@MainActor
final class TerminalPaneSession: ObservableObject, Identifiable {
    let id: String
    let role: PaneRole
    let launchSpec: TerminalLaunchSpec

    @Published var displayTitle: String
    @Published var workingDirectory: String
    @Published var rendererHealthy = false
    @Published var statusMessage: String?
    @Published var terminalReady = false
    @Published var claudeState: ClaudeMonitorState = .unavailable

    weak var hostView: GhosttyTerminalHostView?
    /// The foreground pid of this pane's PTY, or 0 when the surface is not
    /// yet live. Delegates to `GhosttyTerminalHostView.foregroundPid`.
    var shellPid: Int32 { hostView?.foregroundPid ?? 0 }
    private var claudeMonitorTracker = ClaudeMonitorTracker()
    /// Called on the main actor when the pane's child process exits.
    /// Boss pane sets this to a restart closure; worker panes leave it nil.
    var onChildExited: (() -> Void)?
    /// Called on the main actor each time a libghostty surface is
    /// successfully attached to this session. Fires on initial creation
    /// and on every restart (the surface is torn down and re-created
    /// when the child exits). Boss pane uses this to re-register the
    /// Boss trust root after a restart produces a new shell pid.
    var onSurfaceAttached: (() -> Void)?

    init(id: String, role: PaneRole, launchSpec: TerminalLaunchSpec) {
        self.id = id
        self.role = role
        self.launchSpec = launchSpec
        self.displayTitle = role.defaultTitle
        self.workingDirectory = launchSpec.workingDirectory
    }

    func setTitle(_ title: String) {
        displayTitle = title.isEmpty ? role.defaultTitle : title
    }

    func attach(hostView: GhosttyTerminalHostView) {
        self.hostView = hostView
        terminalReady = true
        onSurfaceAttached?()
    }

    func updateClaudeMonitor(snapshot: ClaudeMonitorSnapshot?) {
        claudeState = claudeMonitorTracker.evaluate(snapshot)
    }
}
