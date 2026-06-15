import AppKit
import Foundation
import GhosttyKit

private func ghosttyRuntimeWakeup(_ userdata: UnsafeMutableRawPointer?) {
    GhosttyRuntime.wakeup(userdata)
}

private func ghosttyRuntimeAction(
    _ app: ghostty_app_t?,
    _ target: ghostty_target_s,
    _ action: ghostty_action_s
) -> Bool {
    guard app != nil else { return false }
    return GhosttyRuntime.action(target: target, action: action)
}

private func ghosttyRuntimeReadClipboard(
    _ userdata: UnsafeMutableRawPointer?,
    _ location: ghostty_clipboard_e,
    _ state: UnsafeMutableRawPointer?
) -> Bool {
    GhosttyRuntime.readClipboard(userdata, location: location, state: state)
}

private func ghosttyRuntimeWriteClipboard(
    _ userdata: UnsafeMutableRawPointer?,
    _ location: ghostty_clipboard_e,
    _ content: UnsafePointer<ghostty_clipboard_content_s>?,
    _ len: Int,
    _ confirm: Bool
) {
    GhosttyRuntime.writeClipboard(
        userdata,
        location: location,
        content: content,
        len: len,
        confirm: confirm
    )
}

private func ghosttyRuntimeCloseSurface(_ userdata: UnsafeMutableRawPointer?, _ processAlive: Bool) {
    GhosttyRuntime.closeSurface(userdata, processAlive: processAlive)
}

enum GhosttyBootstrap {
    private static let initialized: Void = {
        // Strip `GHOSTTY_*` env vars from the process environment before
        // libghostty initializes. When Boss is launched from inside a
        // Ghostty.app terminal pane (the dev workflow with `swift run Boss`
        // from a Ghostty shell), the parent injects `GHOSTTY_RESOURCES_DIR`,
        // `GHOSTTY_BIN_DIR`, `GHOSTTY_SHELL_FEATURES` and friends. Those
        // point at the host Ghostty.app's resource tree, which can be a
        // different libghostty version than the one bundled in this app —
        // and `ghostty_init` / `ghostty_app_new` consume them. The observed
        // failure mode is `ghostty_surface_new` returning NULL after every
        // other input checks out (see #613 dump path). Removing the
        // pollution before we touch libghostty closes that surface.
        //
        // Subprocesses (the Claude panes) are unaffected — their env is
        // built separately and passed via `ghostty_surface_config_s.env_vars`.
        stripGhosttyEnvVars()

        let result = ghostty_init(UInt(CommandLine.argc), CommandLine.unsafeArgv)
        guard result == GHOSTTY_SUCCESS else {
            fatalError("ghostty_init failed with status \(result)")
        }
    }()

    static func ensureInitialized() {
        _ = initialized
    }

    private static func stripGhosttyEnvVars() {
        // Snapshot the names first; `unsetenv` mutates `environ`, so
        // iterating it in-place is unsafe.
        var toRemove: [String] = []
        var envp: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?> = environ
        while let raw = envp.pointee {
            if let entry = String(validatingCString: raw),
               let eq = entry.firstIndex(of: "="),
               entry[..<eq].hasPrefix("GHOSTTY_") {
                toRemove.append(String(entry[..<eq]))
            }
            envp = envp.successor()
        }
        for name in toRemove {
            unsetenv(name)
        }
    }
}

final class GhosttyRuntime: @unchecked Sendable {
    /// Singleton instance. libghostty's app handle (`ghostty_app_t`)
    /// owns global state — multiple instances would race on input
    /// dispatch and the wakeup callback. All callers share `shared`.
    static let shared = GhosttyRuntime()

    private let config: ghostty_config_t
    private(set) var app: ghostty_app_t! = nil
    private var observers: [NSObjectProtocol] = []

    private init() {
        GhosttyBootstrap.ensureInitialized()

        guard let config = ghostty_config_new() else {
            fatalError("ghostty_config_new failed")
        }
        self.config = config

        ghostty_config_load_default_files(config)
        ghostty_config_load_recursive_files(config)
        ghostty_config_finalize(config)

        var runtimeConfig = ghostty_runtime_config_s(
            userdata: Unmanaged.passUnretained(self).toOpaque(),
            supports_selection_clipboard: false,
            wakeup_cb: ghosttyRuntimeWakeup,
            action_cb: ghosttyRuntimeAction,
            read_clipboard_cb: ghosttyRuntimeReadClipboard,
            confirm_read_clipboard_cb: { _, _, _, _ in },
            write_clipboard_cb: ghosttyRuntimeWriteClipboard,
            close_surface_cb: ghosttyRuntimeCloseSurface
        )

        guard let app = ghostty_app_new(&runtimeConfig, config) else {
            fatalError("ghostty_app_new failed")
        }
        self.app = app

        ghostty_app_set_focus(app, false)
        installObservers()
    }

    deinit {
        for observer in observers {
            NotificationCenter.default.removeObserver(observer)
        }
        ghostty_app_free(app)
        ghostty_config_free(config)
    }

    func tick() {
        // Count the app-loop tick for the terminal event-loop diagnostics
        // (see [[TerminalLoopMonitor]]). Cheap, and on a path the spin
        // cannot bypass.
        TerminalLoopMonitor.shared.recordTick()
        ghostty_app_tick(app)
    }

    private func installObservers() {
        let center = NotificationCenter.default
        observers.append(
            center.addObserver(
                forName: NSApplication.didBecomeActiveNotification,
                object: nil,
                queue: .main
            ) { [weak self] _ in
                guard let self else { return }
                ghostty_app_set_focus(self.app, true)
            }
        )
        observers.append(
            center.addObserver(
                forName: NSApplication.didResignActiveNotification,
                object: nil,
                queue: .main
            ) { [weak self] _ in
                guard let self else { return }
                ghostty_app_set_focus(self.app, false)
            }
        )
    }

    fileprivate static func runtime(from userdata: UnsafeMutableRawPointer?) -> GhosttyRuntime? {
        guard let userdata else { return nil }
        return Unmanaged<GhosttyRuntime>.fromOpaque(userdata).takeUnretainedValue()
    }

    private static func hostView(from userdata: UnsafeMutableRawPointer?) -> GhosttyTerminalHostView? {
        guard let userdata else { return nil }
        return Unmanaged<GhosttyTerminalHostView>.fromOpaque(userdata).takeUnretainedValue()
    }

    private static func hostView(for target: ghostty_target_s) -> GhosttyTerminalHostView? {
        guard target.tag == GHOSTTY_TARGET_SURFACE else { return nil }
        guard let userdata = ghostty_surface_userdata(target.target.surface) else { return nil }
        return Unmanaged<GhosttyTerminalHostView>.fromOpaque(userdata).takeUnretainedValue()
    }

    private static func string(_ value: UnsafePointer<CChar>?) -> String {
        guard let value else { return "" }
        return String(cString: value)
    }

    fileprivate static func wakeup(_ userdata: UnsafeMutableRawPointer?) {
        guard let runtime = runtime(from: userdata) else { return }
        // Count every libghostty wakeup for the terminal event-loop
        // diagnostics. A surface IO loop spinning on a dead fd floods the
        // app mailbox and drives this callback, so a hot wakeup rate is
        // the Swift-side signature of the spin (see [[TerminalLoopMonitor]]).
        // The increment is an unfair-lock bump — negligible next to the
        // main-queue hop this path already performs.
        TerminalLoopMonitor.shared.recordWakeup()
        OperationQueue.main.addOperation {
            MainActor.assumeIsolated {
                runtime.tick()
            }
        }
    }

    fileprivate static func action(target: ghostty_target_s, action: ghostty_action_s) -> Bool {
        // Two-phase handling: *resolve* now (on whatever thread libghostty
        // called us from), *apply* later (on the main runloop).
        //
        // The `action` struct contains raw C pointers into memory
        // libghostty owns (e.g. `set_title.title`, `pwd.pwd`, `open_url.url`).
        // Those pointers are valid only for the duration of this callback —
        // libghostty's `performPreAction` in `apprt/embedded.zig` literally
        // `alloc.free()`s previous values when a follow-up action arrives.
        // The previous code deferred the *whole* action onto the main queue,
        // which let the pointers go stale before they were deref'd, producing
        // `EXC_BAD_ACCESS / EXC_ARM_DA_ALIGN` crashes inside `String(cString:)`
        // (fixed in PR #209 by switching to synchronous handling). So we
        // still read every pointer synchronously here — `resolve` copies the
        // C strings into owned Swift `String`s and resolves the target host
        // view before this callback returns. The deferred `apply` closure
        // touches *no* libghostty memory, so the stale-pointer hazard cannot
        // recur.
        //
        // Why defer `apply` at all, rather than handle inline as PR #209 did:
        //
        //  1. Off-main safety. libghostty *normally* invokes this from the
        //     macOS main runloop, but "normally" is not "always": a surface
        //     action delivered from a background (renderer / IO) thread would
        //     make a blind `MainActor.assumeIsolated` trip the fatal
        //     libdispatch main-thread assertion and `abort()` the process —
        //     issue #799, observed on background thread `114b158` during a
        //     Metal-renderer storm. Hopping to main *synchronously* would
        //     avoid the abort but risks deadlock (the renderer thread can be
        //     blocked inside a `ghostty_surface_*` call the main thread is
        //     simultaneously driving). An async hop is safe on any thread.
        //
        //  2. No publishing from within view updates. `apply` mutates
        //     `@Published` state on the pane's `TerminalPaneSession`. libghostty
        //     re-enters this callback synchronously from inside the
        //     `ghostty_surface_*` calls that `GhosttyTerminalView.updateNSView`
        //     makes during the SwiftUI view-update pass; mutating observed
        //     state there is the "Publishing changes from within view updates"
        //     violation (issue #799's runaway warning storm). Deferring the
        //     mutation to a fresh main-runloop turn moves it out of the update
        //     pass. Ordering is preserved: actions apply FIFO on the main queue.
        let resolved = resolve(target: target, action: action)
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                apply(resolved)
            }
        }
        return true
    }

    /// A surface action resolved into owned Swift values on the calling
    /// thread, so `apply` can run on a later main-runloop turn without
    /// dereferencing libghostty-owned pointers that are freed the moment
    /// this callback returns.
    ///
    /// `@unchecked Sendable`: the only reference it carries is a
    /// `GhosttyTerminalHostView` (an NSView), created and torn down on the
    /// main thread and kept alive for the app's lifetime; it is only ever
    /// read back inside `apply`, which runs on the main actor. The C
    /// payloads (sizes, color, mouse shape) are plain scalar structs.
    private struct ResolvedAction: @unchecked Sendable {
        enum Kind {
            case setTitle(String)
            case setWorkingDirectory(String)
            case rendererHealth(Bool)
            case mouseShape(ghostty_action_mouse_shape_e)
            case mouseVisibility(Bool)
            case initialSize(ghostty_action_initial_size_s)
            case cellSize(ghostty_action_cell_size_s)
            case colorChange(ghostty_action_color_change_s)
            case ringBell
            case openURL(String)
            case childExited(UInt32)
            case ignored
        }

        let host: GhosttyTerminalHostView?
        let kind: Kind
    }

    /// Reads every libghostty-owned pointer in `action` into owned Swift
    /// values. Safe to call from any thread — it only reads memory that is
    /// live for the duration of the action callback and touches no
    /// main-actor state.
    private static func resolve(target: ghostty_target_s, action: ghostty_action_s) -> ResolvedAction {
        let host = hostView(for: target)
        let kind: ResolvedAction.Kind = switch action.tag {
        case GHOSTTY_ACTION_SET_TITLE:
            .setTitle(string(action.action.set_title.title))
        case GHOSTTY_ACTION_PWD:
            .setWorkingDirectory(string(action.action.pwd.pwd))
        case GHOSTTY_ACTION_RENDERER_HEALTH:
            .rendererHealth(action.action.renderer_health == GHOSTTY_RENDERER_HEALTH_HEALTHY)
        case GHOSTTY_ACTION_MOUSE_SHAPE:
            .mouseShape(action.action.mouse_shape)
        case GHOSTTY_ACTION_MOUSE_VISIBILITY:
            .mouseVisibility(action.action.mouse_visibility == GHOSTTY_MOUSE_VISIBLE)
        case GHOSTTY_ACTION_INITIAL_SIZE:
            .initialSize(action.action.initial_size)
        case GHOSTTY_ACTION_CELL_SIZE:
            .cellSize(action.action.cell_size)
        case GHOSTTY_ACTION_COLOR_CHANGE:
            .colorChange(action.action.color_change)
        case GHOSTTY_ACTION_RING_BELL:
            .ringBell
        case GHOSTTY_ACTION_OPEN_URL:
            .openURL(string(action.action.open_url.url))
        case GHOSTTY_ACTION_SHOW_CHILD_EXITED:
            .childExited(action.action.child_exited.exit_code)
        default:
            .ignored
        }
        return ResolvedAction(host: host, kind: kind)
    }

    @MainActor
    private static func apply(_ resolved: ResolvedAction) {
        switch resolved.kind {
        case .setTitle(let title):
            resolved.host?.session.setTitle(title)

        case .setWorkingDirectory(let pwd):
            resolved.host?.session.workingDirectory = pwd

        case .rendererHealth(let healthy):
            resolved.host?.session.rendererHealthy = healthy

        case .mouseShape(let shape):
            resolved.host?.setCursorShape(shape)

        case .mouseVisibility(let visible):
            resolved.host?.setCursorVisible(visible)

        case .initialSize(let size):
            resolved.host?.applyInitialSize(size)

        case .cellSize(let size):
            resolved.host?.setCellSize(size)

        case .colorChange(let change):
            resolved.host?.applyColorChange(change)

        case .ringBell:
            NSSound.beep()

        case .openURL(let raw):
            if let url = URL(string: raw), url.scheme != nil {
                NSWorkspace.shared.open(url)
            } else {
                NSWorkspace.shared.open(URL(fileURLWithPath: raw))
            }

        case .childExited(let exitCode):
            // Suppress for sessions that handle their own restart (boss pane).
            if resolved.host?.session.onChildExited == nil {
                resolved.host?.session.statusMessage = "Command exited (\(exitCode))"
            }

        case .ignored:
            break
        }
    }

    fileprivate static func readClipboard(
        _ userdata: UnsafeMutableRawPointer?,
        location _: ghostty_clipboard_e,
        state: UnsafeMutableRawPointer?
    ) -> Bool {
        guard let host = hostView(from: userdata) else { return false }
        // libghostty invokes the clipboard-read callback synchronously on the
        // main runloop, so the surface is already main-actor isolated here.
        // The opaque `state` token is round-tripped through an integer so the
        // non-Sendable raw pointer is not captured across the actor boundary.
        let stateAddress = UInt(bitPattern: state)
        return MainActor.assumeIsolated {
            guard let surface = host.surface else { return false }
            guard let text = NSPasteboard.general.string(forType: .string) else {
                return false
            }

            let statePointer = UnsafeMutableRawPointer(bitPattern: stateAddress)
            text.withCString { ptr in
                ghostty_surface_complete_clipboard_request(surface, ptr, statePointer, false)
            }
            return true
        }
    }

    fileprivate static func writeClipboard(
        _ userdata: UnsafeMutableRawPointer?,
        location _: ghostty_clipboard_e,
        content: UnsafePointer<ghostty_clipboard_content_s>?,
        len: Int,
        confirm _: Bool
    ) {
        guard hostView(from: userdata) != nil else { return }
        guard let content, len > 0 else { return }

        let pasteboard = NSPasteboard.general
        pasteboard.clearContents()

        for index in 0..<len {
            let item = content[index]
            guard let mime = item.mime, String(cString: mime) == "text/plain" else {
                continue
            }
            pasteboard.setString(String(cString: item.data), forType: .string)
            break
        }
    }

    fileprivate static func closeSurface(_ userdata: UnsafeMutableRawPointer?, processAlive: Bool) {
        guard let host = hostView(from: userdata) else { return }
        OperationQueue.main.addOperation {
            MainActor.assumeIsolated {
                if !processAlive, let onExit = host.session.onChildExited {
                    // Boss pane: delegate to the restart callback instead of
                    // showing a bare "Surface closed" message.
                    onExit()
                } else {
                    host.session.statusMessage = processAlive ? "Surface requested close" : "Surface closed"
                }
            }
        }
    }
}
