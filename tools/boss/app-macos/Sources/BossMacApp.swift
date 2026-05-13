import AppKit
import SwiftUI

@main
struct BossMacApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate

    var body: some Scene {
        WindowGroup {
            ContentView()
        }
        .windowToolbarStyle(.unified(showsTitle: false))
        .defaultSize(width: 1060, height: 680)
        .commands {
            TextEditingCommands()
            CommandMenu("Debug") {
                Button("Dispatch Events") {
                    DispatchEventsWindowController.shared.toggle()
                }
                .keyboardShortcut("d", modifiers: [.command, .shift])
            }
        }
    }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        // When launched outside a regular .app bundle (e.g. `swift run`
        // for local dev), macOS does not auto-promote the process to a
        // foreground UI app — the window opens but never becomes key,
        // so keystrokes go to whichever app was active before launch.
        // Forcing .regular + activate restores key-window status without
        // bringing back the manual NSWindow setup #417 removed.
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
        DispatchEventsWindowController.shared.restoreIfNeeded()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }
}
