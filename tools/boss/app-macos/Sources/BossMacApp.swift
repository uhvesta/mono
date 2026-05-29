import AppKit
import SwiftUI
import UpdateCore

@main
struct BossMacApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var chatModel = ChatViewModel(paths: BossEnginePaths.production())
    @StateObject private var updateModel: UpdateModel = UpdateModel.makeForApp()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .task {
                    appDelegate.liveWorkerStates = chatModel.liveWorkerStates
                    appDelegate.updateModel = updateModel
                    updateModel.startPollingIfNeeded()
                }
        }
        .environmentObject(chatModel)
        .environmentObject(updateModel)
        .windowToolbarStyle(.unified(showsTitle: false))
        .defaultSize(width: 1060, height: 680)
        .commands {
            TextEditingCommands()
            // Show BossFullVersion (e.g. "1.0.4-dev-f3be785") in the About panel
            // rather than CFBundleShortVersionString (numeric-only — plisttool
            // enforces Apple's format requirement for that key).
            CommandGroup(replacing: .appInfo) {
                Button("About Boss") {
                    let full = Bundle.main.object(forInfoDictionaryKey: "BossFullVersion")
                        as? String ?? Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? ""
                    NSApplication.shared.orderFrontStandardAboutPanel(options: [
                        .applicationVersion: full,
                    ])
                }
            }
            CommandGroup(after: .appInfo) {
                CheckForUpdatesCommand()
            }
            CommandGroup(after: .windowList) {
                Divider()
                LogViewerCommand()
                MetricsCommand()
                UIStallsCommand()
            }
        }

        Settings {
            SettingsView()
                .environmentObject(chatModel)
                .environmentObject(updateModel)
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
        // Inject the viewer VM directly so the window observes its
        // `.loading -> .loaded` transition immediately, rather than waiting
        // for the next incidental `chatModel` publish (see
        // [[AsyncMarkdownViewerView]]). `chatModel` stays injected for the
        // comment layer and other descendants.
        .environmentObject(chatModel)
        .environmentObject(chatModel.asyncMarkdownViewerVM)
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

        // Review-terminal window: opened from a Review-column card's
        // terminal button. Opens immediately in a loading state; transitions
        // to a live Ghostty surface once the engine finishes leasing the
        // workspace and checking out the PR branch (async-markdown-viewer
        // pattern).
        Window("Review Terminal", id: "review-terminal") {
            ReviewTerminalView()
        }
        .environmentObject(chatModel)
        .environmentObject(chatModel.reviewTerminalVM)
        .defaultSize(width: 1000, height: 660)

        Window("Activity", id: "activity") {
            ActivityView()
        }
        .environmentObject(chatModel)
        .defaultSize(width: 1100, height: 640)

        Window("Metrics", id: "metrics") {
            MetricsViewer()
        }
        .environmentObject(chatModel)
        .defaultSize(width: 720, height: 520)

        Window("UI Stalls", id: "ui-stalls") {
            UIStallsViewer()
        }
        .defaultSize(width: 720, height: 520)
    }
}

private struct CheckForUpdatesCommand: View {
    var body: some View {
        Button("Check for Updates…") {
            guard let model = (NSApp.delegate as? AppDelegate)?.updateModel else { return }
            model.presentUpdateSheet()
        }
    }
}

private struct LogViewerCommand: View {
    @Environment(\.openWindow) private var openWindow
    @Environment(\.dismissWindow) private var dismissWindow
    @AppStorage("boss.activity.visible") private var isOpen = false

    var body: some View {
        Button("Show Activity") {
            if isOpen {
                isOpen = false
                dismissWindow(id: "activity")
            } else {
                isOpen = true
                openWindow(id: "activity")
            }
        }
        .keyboardShortcut("l", modifiers: [.command, .shift])
    }
}

private struct ActivityView: View {
    @AppStorage("boss.activity.visible") private var isOpen = false

    var body: some View {
        TabView {
            ActivityLogView()
                .tabItem { Label("Activity", systemImage: "list.bullet") }
            LogViewer()
                .tabItem { Label("Logs", systemImage: "doc.text.magnifyingglass") }
        }
        .onAppear { isOpen = true }
        .onDisappear { isOpen = false }
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

private struct UIStallsCommand: View {
    @Environment(\.openWindow) private var openWindow
    @Environment(\.dismissWindow) private var dismissWindow
    @AppStorage("boss.uiStalls.visible") private var isOpen = false

    var body: some View {
        Button("UI Stalls") {
            if isOpen {
                isOpen = false
                dismissWindow(id: "ui-stalls")
            } else {
                isOpen = true
                openWindow(id: "ui-stalls")
            }
        }
        .keyboardShortcut("u", modifiers: [.command, .shift])
    }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    /// Set by BossMacApp once the main window has appeared. Nil only in the
    /// brief window between launch and first-render — treated as "no agents
    /// working" so a very-early Cmd-Q is never held hostage.
    var liveWorkerStates: LiveWorkerStateStore?
    /// Set by BossMacApp on first ContentView appear. Accessed by CheckForUpdatesCommand
    /// (commands cannot reliably use @EnvironmentObject; AppDelegate is always reachable).
    var updateModel: UpdateModel?

    func applicationDidFinishLaunching(_ notification: Notification) {
        // When launched outside a regular .app bundle (e.g. `swift run`
        // for local dev), macOS does not auto-promote the process to a
        // foreground UI app — the window opens but never becomes key,
        // so keystrokes go to whichever app was active before launch.
        // Forcing .regular + activate restores key-window status without
        // bringing back the manual NSWindow setup #417 removed.
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)

        // Start the main-thread hang watchdog. Captures the main thread's
        // Mach port here (we are on the main thread), then runs a
        // background watchdog that records a stall + backtrace whenever
        // the main queue goes unresponsive. Surfaced via the "UI Stalls"
        // window (Cmd-Shift-U). See [[MainThreadStallMonitor]].
        MainThreadStallMonitor.shared.start()
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

    /// When the last window is closed and workers are still alive, keep
    /// the app running instead of quitting. The window-close path
    /// (red traffic light / Cmd-W) does not consistently route through
    /// `applicationShouldTerminate(_:)` under SwiftUI's `WindowGroup`
    /// lifecycle, so returning `true` here let macOS exit silently —
    /// killing every running Claude pane underneath. Returning `false`
    /// while workers are active leaves the process alive (workers keep
    /// running); the user can re-open the window from the Dock or
    /// explicitly Cmd-Q to hit the confirmation modal.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        (liveWorkerStates?.activeAgentCount ?? 0) == 0
    }
}
