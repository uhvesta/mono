import AppKit
import SwiftUI

@main
struct BossMacApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var chatModel = ChatViewModel()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .task {
                    appDelegate.liveWorkerStates = chatModel.liveWorkerStates
                }
        }
        .environmentObject(chatModel)
        .windowToolbarStyle(.unified(showsTitle: false))
        .defaultSize(width: 1060, height: 680)
        .commands {
            TextEditingCommands()
            CommandMenu("Debug") {
                DispatchEventsCommand()
                EngineCommand()
                MetricsCommand()
                Divider()
                BossSessionInvocationCommand()
            }
        }

        Settings {
            SettingsView()
                .environmentObject(chatModel)
        }

        WindowGroup("Description", id: "markdown-viewer", for: MarkdownViewerContent.self) { $content in
            if let content {
                MarkdownViewerView(title: content.title, source: content.markdown)
                    .navigationTitle(content.title)
            }
        }
        .defaultSize(width: 760, height: 640)

        // Async variant of the markdown viewer: opens immediately in a
        // loading state when the user clicks a design-doc icon, then
        // transitions to loaded/failed when the raw-content fetch settles.
        // Uses [[ChatViewModel.asyncMarkdownViewerVM]] (injected via
        // environmentObject) rather than a value-type payload so the
        // window content can be updated after it opens.
        Window("Design Doc", id: "async-markdown-viewer") {
            AsyncMarkdownViewerView()
        }
        .environmentObject(chatModel)
        .defaultSize(width: 760, height: 640)

        // In-app renderer for a project's design-doc pointer. Wired to
        // the kanban project-card affordance via
        // [[ChatViewModel.designRendererOpener]] so SameProduct /
        // OtherProduct + workspace-available pointers render in this
        // window instead of dispatching to the OS-registered `.md`
        // handler — chore #12 of `project-design-doc-pointer.md`.
        WindowGroup("Design Doc", id: "design-renderer", for: DesignRendererContent.self) { $content in
            if let content {
                DesignRendererView(content: content)
                    .navigationTitle(content.title)
            }
        }
        .defaultSize(width: 880, height: 700)

        Window("Dispatch Events", id: "dispatch-events") {
            DispatchEventsViewer()
        }
        .defaultSize(width: 1040, height: 620)

        Window("Engine", id: "engine") {
            EngineView()
        }
        .environmentObject(chatModel)
        .defaultSize(width: 1040, height: 620)

        Window("Metrics", id: "metrics") {
            MetricsViewer()
        }
        .environmentObject(chatModel)
        .defaultSize(width: 720, height: 520)
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

private struct MetricsCommand: View {
    @Environment(\.openWindow) private var openWindow
    @Environment(\.dismissWindow) private var dismissWindow
    @AppStorage("boss.metricsViewer.visible") private var isOpen = false

    var body: some View {
        Button("Metrics") {
            if isOpen {
                isOpen = false
                dismissWindow(id: "metrics")
            } else {
                isOpen = true
                openWindow(id: "metrics")
            }
        }
        .keyboardShortcut("m", modifiers: [.command, .shift])
    }
}

/// Debug menu item: copies the Boss-session claude invocation to the
/// clipboard. Provides an out-of-scrollback way to confirm which flags
/// the Boss session was started with — mirrors the cube_command display
/// in the dispatch events drawer (T468) for worker invocations.
private struct BossSessionInvocationCommand: View {
    var body: some View {
        Button("Copy Boss Session Invocation") {
            let pb = NSPasteboard.general
            pb.clearContents()
            pb.setString(bossPaneClaudeInvocation, forType: .string)
        }
        .help("Copies the claude invocation used for the Boss session pane to the clipboard")
    }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    /// Set by BossMacApp once the main window has appeared. Nil only in the
    /// brief window between launch and first-render — treated as "no agents
    /// working" so a very-early Cmd-Q is never held hostage.
    var liveWorkerStates: LiveWorkerStateStore?

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

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        let count = liveWorkerStates?.activeAgentCount ?? 0
        guard count > 0 else { return .terminateNow }

        let alert = NSAlert()
        alert.messageText = "Quit Boss?"
        let agentWord = count == 1 ? "agent is" : "agents are"
        alert.informativeText =
            "\(count) \(agentWord) currently working. Quitting will terminate them and discard any unsaved progress."
        alert.addButton(withTitle: "Cancel")
        alert.addButton(withTitle: "Quit Anyway")
        alert.alertStyle = .warning

        // Make Cancel (index 0) the default so a stray Cmd-Q doesn't
        // accidentally confirm through the dialog.
        alert.buttons[0].keyEquivalent = "\r"
        alert.buttons[1].keyEquivalent = ""
        alert.buttons[1].hasDestructiveAction = true

        let response = alert.runModal()
        if response == .alertFirstButtonReturn {
            // Cancel — stay running.
            return .terminateCancel
        }
        // Quit Anyway
        return .terminateNow
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }
}
