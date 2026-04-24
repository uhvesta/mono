import AppKit
import SwiftUI
import GhosttyKit

@main
struct GhosttyProtoApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var workspace = TerminalWorkspaceModel()

    init() {
        GhosttyBootstrap.ensureInitialized()
        NSApplication.shared.setActivationPolicy(.regular)
    }

    var body: some Scene {
        WindowGroup {
            ContentView(workspace: workspace)
                .frame(idealWidth: 1600, idealHeight: 900)
        }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApplication.shared.activate(ignoringOtherApps: true)
    }
}

enum GhosttyBootstrap {
    private static let initialized: Void = {
        let result = ghostty_init(UInt(CommandLine.argc), CommandLine.unsafeArgv)
        guard result == GHOSTTY_SUCCESS else {
            fatalError("ghostty_init failed with status \(result)")
        }
    }()

    static func ensureInitialized() {
        _ = initialized
    }
}
