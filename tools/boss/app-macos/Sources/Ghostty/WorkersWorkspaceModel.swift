import Foundation

@MainActor
final class WorkersWorkspaceModel: ObservableObject {
    static let workerSlotCount = 8

    let runtime: GhosttyRuntime
    let workerPanes: [TerminalPaneSession]

    init() {
        let launchSpec = TerminalLaunchSpec(
            fontSize: 10.0,
            workingDirectory: NSHomeDirectory(),
            initialInput: "claude\n"
        )

        self.runtime = GhosttyRuntime()
        self.workerPanes = (1...Self.workerSlotCount).map { slot in
            TerminalPaneSession(
                id: "worker-\(slot)",
                role: .worker(slot: slot),
                launchSpec: launchSpec
            )
        }
    }
}
