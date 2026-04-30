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
        let result = ghostty_init(UInt(CommandLine.argc), CommandLine.unsafeArgv)
        guard result == GHOSTTY_SUCCESS else {
            fatalError("ghostty_init failed with status \(result)")
        }
    }()

    static func ensureInitialized() {
        _ = initialized
    }
}

final class GhosttyRuntime: @unchecked Sendable {
    private let config: ghostty_config_t
    private(set) var app: ghostty_app_t! = nil
    private var observers: [NSObjectProtocol] = []

    init() {
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
        OperationQueue.main.addOperation {
            MainActor.assumeIsolated {
                runtime.tick()
            }
        }
    }

    fileprivate static func action(target: ghostty_target_s, action: ghostty_action_s) -> Bool {
        OperationQueue.main.addOperation {
            MainActor.assumeIsolated {
                handleAction(target: target, action: action)
            }
        }
        return true
    }

    @MainActor
    private static func handleAction(target: ghostty_target_s, action: ghostty_action_s) {
        switch action.tag {
        case GHOSTTY_ACTION_SET_TITLE:
            hostView(for: target)?.session.setTitle(string(action.action.set_title.title))

        case GHOSTTY_ACTION_PWD:
            hostView(for: target)?.session.workingDirectory = string(action.action.pwd.pwd)

        case GHOSTTY_ACTION_RENDERER_HEALTH:
            hostView(for: target)?.session.rendererHealthy =
                action.action.renderer_health == GHOSTTY_RENDERER_HEALTH_HEALTHY

        case GHOSTTY_ACTION_MOUSE_SHAPE:
            hostView(for: target)?.setCursorShape(action.action.mouse_shape)

        case GHOSTTY_ACTION_MOUSE_VISIBILITY:
            hostView(for: target)?.setCursorVisible(
                action.action.mouse_visibility == GHOSTTY_MOUSE_VISIBLE
            )

        case GHOSTTY_ACTION_INITIAL_SIZE:
            hostView(for: target)?.applyInitialSize(action.action.initial_size)

        case GHOSTTY_ACTION_CELL_SIZE:
            hostView(for: target)?.setCellSize(action.action.cell_size)

        case GHOSTTY_ACTION_COLOR_CHANGE:
            hostView(for: target)?.applyColorChange(action.action.color_change)

        case GHOSTTY_ACTION_RING_BELL:
            NSSound.beep()

        case GHOSTTY_ACTION_OPEN_URL:
            let raw = string(action.action.open_url.url)
            if let url = URL(string: raw), url.scheme != nil {
                NSWorkspace.shared.open(url)
            } else {
                NSWorkspace.shared.open(URL(fileURLWithPath: raw))
            }

        case GHOSTTY_ACTION_SHOW_CHILD_EXITED:
            let exitCode = action.action.child_exited.exit_code
            hostView(for: target)?.session.statusMessage = "Command exited (\(exitCode))"

        default:
            break
        }
    }

    fileprivate static func readClipboard(
        _ userdata: UnsafeMutableRawPointer?,
        location _: ghostty_clipboard_e,
        state: UnsafeMutableRawPointer?
    ) -> Bool {
        guard let host = hostView(from: userdata), let surface = host.surface else {
            return false
        }
        guard let text = NSPasteboard.general.string(forType: .string) else {
            return false
        }

        text.withCString { ptr in
            ghostty_surface_complete_clipboard_request(surface, ptr, state, false)
        }
        return true
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
                host.session.statusMessage = processAlive ? "Surface requested close" : "Surface closed"
            }
        }
    }
}
