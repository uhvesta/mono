import AppKit
import SwiftUI
import UpdateCore
import os.log

private let appUpdateLog = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "updater")

@main
struct BossMacApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var chatModel = ChatViewModel(paths: BossEnginePaths.production())

    var body: some Scene {
        WindowGroup {
            ContentView()
                .task {
                    appDelegate.liveWorkerStates = chatModel.liveWorkerStates
                    appDelegate.updateModel.startPollingIfNeeded()
                }
        }
        .environmentObject(chatModel)
        .environmentObject(appDelegate.updateModel)
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
                CheckForUpdatesCommand(updateModel: appDelegate.updateModel)
            }
            CommandGroup(after: .windowList) {
                Divider()
                LogViewerCommand()
                MetricsCommand()
                UIStallsCommand()
                TerminalLoopCommand()
            }
        }

        Settings {
            SettingsView()
                .environmentObject(chatModel)
                .environmentObject(appDelegate.updateModel)
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
        .environmentObject(chatModel)
        .defaultSize(width: 880, height: 700)

        // Review-terminal window: opened from a Review-column card's
        // terminal button. Opens immediately in a loading state; transitions
        // to a live Ghostty surface once the engine finishes leasing the
        // workspace and checking out the PR branch (async-markdown-viewer
        // pattern).
        // Transcript viewer: shows all historical executions for one task
        // on the left and the selected execution's transcript on the right.
        // Keyed by TranscriptViewerRef (Hashable on taskId only) so
        // re-invoking "View transcripts" for the same task focuses the
        // existing window instead of spawning a duplicate.
        WindowGroup("Transcripts", id: "transcript-viewer", for: TranscriptViewerRef.self) { $ref in
            if let ref {
                TranscriptViewerView(ref: ref)
                    .environmentObject(chatModel)
            }
        }
        .defaultSize(width: 900, height: 640)

        Window("Review Terminal", id: "review-terminal") {
            ReviewTerminalView()
        }
        .environmentObject(chatModel)
        .environmentObject(chatModel.reviewTerminalVM)
        .defaultSize(width: 1000, height: 660)

        // Notifications (Attentions) window: opened from the bell toolbar item
        // (attentions.md — App UI). A singleton window driven by ChatViewModel
        // (mirrors Activity / Metrics) rather than a value-keyed WindowGroup —
        // there is one product-scoped notifications surface, not many.
        Window("Notifications", id: "attentions") {
            AttentionsView()
        }
        .environmentObject(chatModel)
        .defaultSize(width: 460, height: 560)

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

        Window("Terminal Loop", id: "terminal-loop") {
            TerminalLoopViewer()
        }
        .defaultSize(width: 720, height: 540)
    }
}

private struct CheckForUpdatesCommand: View {
    let updateModel: UpdateModel

    var body: some View {
        Button("Check for Updates…") {
            updateModel.presentUpdateSheet()
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

private struct TerminalLoopCommand: View {
    @Environment(\.openWindow) private var openWindow
    @Environment(\.dismissWindow) private var dismissWindow
    @AppStorage("boss.terminalLoopViewer.visible") private var isOpen = false

    var body: some View {
        Button("Terminal Loop") {
            if isOpen {
                isOpen = false
                dismissWindow(id: "terminal-loop")
            } else {
                isOpen = true
                openWindow(id: "terminal-loop")
            }
        }
        .keyboardShortcut("t", modifiers: [.command, .shift])
    }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    /// Set by BossMacApp once the main window has appeared. Nil only in the
    /// brief window between launch and first-render — treated as "no agents
    /// working" so a very-early Cmd-Q is never held hostage.
    var liveWorkerStates: LiveWorkerStateStore?
    /// Owned here so the App struct can inject it into CheckForUpdatesCommand and
    /// environment objects before any view renders or menu fires.
    let updateModel: UpdateModel = UpdateModel.makeForApp()

    func applicationDidFinishLaunching(_ notification: Notification) {
        // When launched outside a regular .app bundle (e.g. `swift run`
        // for local dev), macOS does not auto-promote the process to a
        // foreground UI app — the window opens but never becomes key,
        // so keystrokes go to whichever app was active before launch.
        // Forcing .regular + activate restores key-window status without
        // bringing back the manual NSWindow setup #417 removed.
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)

        // Self-updater: complete any pending bundle swap and write this version's
        // first-launch-OK flag (which the relaunch watchdog polls for). Runs before
        // anything else touches the update state so a relaunch helper from the swap
        // that brought us here sees a healthy launch promptly. The startup-swap
        // *fallback* (applying a not-yet-installed staged update) runs later, at the
        // engine-launch chokepoint in ChatViewModel.startIfNeeded(). See
        // [[UpdateLifecycle]] and design doc §4.
        UpdateLifecycle.reconcileAtLaunch()

        // Start the main-thread hang watchdog. Captures the main thread's
        // Mach port here (we are on the main thread), then runs a
        // background watchdog that records a stall + backtrace whenever
        // the main queue goes unresponsive. Surfaced via the "UI Stalls"
        // window (Cmd-Shift-U). See [[MainThreadStallMonitor]].
        MainThreadStallMonitor.shared.start()

        // Start the terminal event-loop diagnostics sampler (1 Hz). Counts
        // libghostty app-loop activity and probes each pane's pty/EOF/pid
        // liveness to verify/refute the busy-spin high-CPU hypothesis.
        // Surfaced via the "Terminal Loop" window (Cmd-Shift-T). See
        // [[TerminalLoopMonitor]].
        TerminalLoopMonitor.shared.start()
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

    /// Swap-on-quit (design doc §4): once termination is confirmed, apply any staged
    /// update in place so the next launch runs the new version. The agents-running
    /// gate is upstream in `applicationShouldTerminate(_:)` — reaching here means the
    /// user accepted the quit. Best-effort and non-blocking: a failed swap leaves the
    /// current bundle untouched, and the startup path retries next launch.
    func applicationWillTerminate(_ notification: Notification) {
        UpdateLifecycle.applyQuitSwapIfNeeded()
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
