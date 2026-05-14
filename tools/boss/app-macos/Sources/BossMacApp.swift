import AppKit
import SwiftUI

@main
struct BossMacApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var chatModel = ChatViewModel()

    var body: some Scene {
        WindowGroup {
            ContentView()
        }
        .environmentObject(chatModel)
        .windowToolbarStyle(.unified(showsTitle: false))
        .defaultSize(width: 1060, height: 680)
        .commands {
            TextEditingCommands()
            CommandMenu("Debug") {
                DispatchEventsCommand()
                EngineCommand()
            }
        }

        WindowGroup("Description", id: "markdown-viewer", for: MarkdownViewerContent.self) { $content in
            if let content {
                MarkdownViewerView(title: content.title, source: content.markdown)
            }
        }
        .defaultSize(width: 760, height: 640)

        Window("Dispatch Events", id: "dispatch-events") {
            DispatchEventsViewer()
        }
        .defaultSize(width: 1040, height: 620)

        Window("Engine", id: "engine") {
            EngineView()
        }
        .environmentObject(chatModel)
        .defaultSize(width: 1040, height: 620)
    }
}

private struct DispatchEventsCommand: View {
    @Environment(\.openWindow) private var openWindow
    @Environment(\.dismissWindow) private var dismissWindow
    @AppStorage("boss.dispatchEventsViewer.visible") private var isOpen = false

    var body: some View {
        Button("Dispatch Events") {
            if isOpen {
                isOpen = false
                dismissWindow(id: "dispatch-events")
            } else {
                isOpen = true
                openWindow(id: "dispatch-events")
            }
        }
        .keyboardShortcut("d", modifiers: [.command, .shift])
    }
}

private struct EngineCommand: View {
    @Environment(\.openWindow) private var openWindow
    @Environment(\.dismissWindow) private var dismissWindow
    @AppStorage("boss.engineViewer.visible") private var isOpen = false

    var body: some View {
        Button("Engine") {
            if isOpen {
                isOpen = false
                dismissWindow(id: "engine")
            } else {
                isOpen = true
                openWindow(id: "engine")
            }
        }
        .keyboardShortcut("e", modifiers: [.command, .shift])
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
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }
}
