import AppKit
import SwiftUI
import GhosttyKit

private extension NSScreen {
    var ghosttyDisplayID: UInt32 {
        guard let screenNumber = deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber else {
            return 0
        }
        return screenNumber.uint32Value
    }
}

/// Output of `GhosttyTerminalHostView.submissionPlan(for:)` — the body
/// to deliver via the paste path (`ghostty_surface_text`) plus a flag
/// for whether a Return keystroke should follow. Carried as a struct
/// (rather than a tuple) so the helper has a stable, testable shape.
struct PaneSubmissionPlan: Equatable {
    let body: String
    let sendReturn: Bool
}

struct GhosttyTerminalView: NSViewRepresentable {
    let runtime: GhosttyRuntime
    let session: TerminalPaneSession
    let launchSpec: TerminalLaunchSpec
    /// Gate for the per-pane 0.5s viewport screen-scrape that drives
    /// `claudeState`. Flows in as a plain SwiftUI input — `updateNSView`
    /// reconciles the timer against it — rather than a `@Published` on
    /// the session that a parent mutates mid-render. Worker panes pass
    /// `liveState == nil` (the scrape is the pre-hook fallback; once the
    /// engine pushes a `LiveWorkerState` the pill renders hook-driven
    /// activity and the scrape is redundant); Boss panes pass `false`
    /// (they never display `claudeState`).
    let claudeMonitorEnabled: Bool

    func makeNSView(context: Context) -> GhosttyTerminalHostView {
        GhosttyTerminalHostView(
            runtime: runtime,
            session: session,
            launchSpec: launchSpec,
            claudeMonitorEnabled: claudeMonitorEnabled
        )
    }

    func updateNSView(_ view: GhosttyTerminalHostView, context: Context) {
        view.syncGeometry()
        view.reconcileClaudeMonitor(enabled: claudeMonitorEnabled)
    }
}

final class GhosttyTerminalHostView: NSView {
    let runtime: GhosttyRuntime
    let session: TerminalPaneSession
    let launchSpec: TerminalLaunchSpec
    private(set) var surface: ghostty_surface_t?

    private var trackingAreaRef: NSTrackingArea?
    private var currentCursor: NSCursor = .iBeam
    private var cursorVisible = true
    private var backgroundColor = NSColor.black
    private var claudeMonitorTimer: Timer?
    /// Latest value of the SwiftUI `claudeMonitorEnabled` input. Stored
    /// so the internal reconcile callers (surface creation,
    /// `viewDidMoveToWindow`) can converge the timer without a value
    /// argument; `updateNSView` refreshes it whenever the input changes.
    private var claudeMonitorEnabled: Bool
    /// Token for the display-configuration observer installed only
    /// while surface creation has failed. libghostty's
    /// `ghostty_surface_new` returns NULL when the machine has no
    /// active display (lid closed with no external monitor, all
    /// monitors disconnected, display asleep) — the renderer's
    /// `CVDisplayLinkCreateWithCGDisplays` rejects a display count of
    /// 0. That is a transient/environmental condition, so instead of
    /// crashing the app we keep the pane in a surface-less placeholder
    /// state and retry when the display set changes (#800).
    private var screenObserver: NSObjectProtocol?

    private var lastSyncedBackingSize: CGSize = .zero
    private var lastSizeSyncTimestamp: TimeInterval = 0
    private var pendingGeometrySync: DispatchWorkItem?
    /// Cap on how often we forward layout-driven size changes to
    /// libghostty. Reflowing the scrollback inside
    /// `ghostty_surface_set_size` is O(history); without a cap, a
    /// pane-divider drag fires the call on every layout tick (60–
    /// 120 Hz) and blocks the main thread for the whole drag.
    private static let geometrySyncMinInterval: TimeInterval = 1.0 / 30.0
    /// Serial background queue for `ghostty_surface_set_focus` calls.
    ///
    /// libghostty's `focusCallback` acquires `renderer_state.mutex`
    /// (shared with the IO thread that processes PTY output) and pushes
    /// a message to the renderer thread's 64-slot mailbox with a
    /// `.forever` timeout. Both operations block the calling thread when
    /// contended: the mutex blocks while an IO thread is writing PTY
    /// output, and the mailbox push blocks when the renderer's
    /// CVDisplayLink is paused in background while active Claude sessions
    /// continue generating render requests.
    ///
    /// Blocking the main thread inside `becomeFirstResponder` causes the
    /// CMD-Tab-back beachball. Dispatching through this serial queue lets
    /// AppKit's first-responder switch return immediately while the
    /// potentially-blocking libghostty handshake completes off the main
    /// run-loop.
    ///
    /// Lifetime note: terminal views in the Boss macOS app are kept alive
    /// for the app's lifetime (ZStack opacity toggle, never removed), so
    /// the captured surface pointer is always valid when the queued block
    /// executes during normal operation.
    private static let focusQueue = DispatchQueue(
        label: "boss.terminal.focus",
        qos: .userInitiated
    )

    /// `@Sendable`-safe wrapper around a libghostty surface pointer so
    /// the dispatched focus call can capture it without producing a
    /// Swift 6 strict-concurrency warning. `ghostty_surface_t` is
    /// `void*` (UnsafeMutableRawPointer), which is intentionally not
    /// `Sendable`; the unchecked conformance is correct here because
    /// libghostty's surface pointer is opaque, immutable for the
    /// lifetime of the host view, and serial-queue dispatch already
    /// provides the required happens-before ordering between
    /// `ghostty_surface_set_focus` calls.
    private struct SurfaceBox: @unchecked Sendable {
        let surface: ghostty_surface_t
    }

    init(
        runtime: GhosttyRuntime,
        session: TerminalPaneSession,
        launchSpec: TerminalLaunchSpec,
        claudeMonitorEnabled: Bool
    ) {
        self.runtime = runtime
        self.session = session
        self.launchSpec = launchSpec
        self.claudeMonitorEnabled = claudeMonitorEnabled
        super.init(frame: NSRect(x: 0, y: 0, width: 320, height: 820))

        wantsLayer = true
        layer?.backgroundColor = backgroundColor.cgColor

        attemptSurfaceCreation()
    }

    /// Create the libghostty surface if we don't already have one, and
    /// finalize the view on success. Idempotent: a no-op once `surface`
    /// is set, so it is safe to call from `init`, the display-change
    /// observer, and `viewDidMoveToWindow`.
    ///
    /// On failure we do **not** `fatalError` — a NULL surface is most
    /// often the transient "no active display" condition (#800), which
    /// must not take the whole app down. Instead we leave the pane in a
    /// surface-less placeholder state (the view's black background) and
    /// arm a display-change observer so the next display reconfiguration
    /// retries.
    private func attemptSurfaceCreation() {
        guard surface == nil else { return }

        guard let surface = makeSurface() else {
            session.statusMessage = "Waiting for an active display…"
            installScreenObserverIfNeeded()
            return
        }

        self.surface = surface
        removeScreenObserver()
        session.statusMessage = nil
        session.attach(hostView: self)
        syncGeometry()
        reconcileClaudeMonitor()
    }

    /// Build the surface config from `launchSpec` and call
    /// `ghostty_surface_new`. Returns `nil` (after dumping a diagnostic
    /// to stderr) when libghostty rejects the surface, rather than
    /// trapping — see `attemptSurfaceCreation`.
    private func makeSurface() -> ghostty_surface_t? {
        // Build env_vars: each `ghostty_env_var_s` holds borrowed C
        // pointers, so we strdup every string and free them after
        // ghostty_surface_new returns (ghostty copies during init).
        var allocatedEnvStrings: [UnsafeMutablePointer<CChar>] = []
        var envVars: [ghostty_env_var_s] = launchSpec.env.map { (key, value) in
            let keyPtr = strdup(key)!
            let valPtr = strdup(value)!
            allocatedEnvStrings.append(keyPtr)
            allocatedEnvStrings.append(valPtr)
            return ghostty_env_var_s(key: keyPtr, value: valPtr)
        }
        defer {
            for ptr in allocatedEnvStrings {
                free(ptr)
            }
        }

        let surface = launchSpec.workingDirectory.withCString { workingDirectory in
            launchSpec.initialInput.withCString { initialInput in
                envVars.withUnsafeMutableBufferPointer { envBuf in
                    var config = ghostty_surface_config_new()
                    config.platform_tag = GHOSTTY_PLATFORM_MACOS
                    config.platform = ghostty_platform_u(macos: ghostty_platform_macos_s(
                        nsview: Unmanaged.passUnretained(self).toOpaque()
                    ))
                    config.userdata = Unmanaged.passUnretained(self).toOpaque()
                    config.scale_factor = Double(NSScreen.main?.backingScaleFactor ?? 2.0)
                    config.font_size = launchSpec.fontSize
                    config.working_directory = workingDirectory
                    config.initial_input = initialInput
                    config.env_vars = envBuf.baseAddress
                    config.env_var_count = envBuf.count
                    return ghostty_surface_new(runtime.app, &config)
                }
            }
        }

        guard let surface else {
            // libghostty's C API (as of 1.3.2) exposes no log callback and
            // ghostty_surface_new returns void* with no error code, so the
            // best we can do on failure is dump every input we control.
            // Without this, the only visible signal is a Sentry minidump,
            // which doesn't tell us which precondition libghostty rejected.
            // Print to stderr so it lands in the dev `swift run` log and in
            // os_log for bundled installs.
            let fm = FileManager.default
            var isDir: ObjCBool = false
            let cwdExists = fm.fileExists(atPath: launchSpec.workingDirectory, isDirectory: &isDir)
            let envSummary = launchSpec.env.prefix(8)
                .map { "\($0.0)=\($0.1.prefix(60))" }
                .joined(separator: ", ")
            let diagnostic = Self.surfaceFailureDiagnostic(
                appNonNil: runtime.app != nil,
                workingDirectory: launchSpec.workingDirectory,
                cwdExists: cwdExists,
                isDirectory: isDir.boolValue,
                fontSize: launchSpec.fontSize,
                scaleFactor: Double(NSScreen.main?.backingScaleFactor ?? 2.0),
                envVarCount: launchSpec.env.count,
                envSummary: envSummary,
                initialInputCount: launchSpec.initialInput.count
            )
            FileHandle.standardError.write(Data(diagnostic.utf8))
            return nil
        }

        return surface
    }

    /// Multi-line diagnostic block dumped to stderr when
    /// `ghostty_surface_new` returns NULL. Pure and `static` so the
    /// failure-context contract is unit-testable without standing up a
    /// libghostty surface.
    static func surfaceFailureDiagnostic(
        appNonNil: Bool,
        workingDirectory: String,
        cwdExists: Bool,
        isDirectory: Bool,
        fontSize: Float32,
        scaleFactor: Double,
        envVarCount: Int,
        envSummary: String,
        initialInputCount: Int
    ) -> String {
        """
        [GhosttyTerminalView] ghostty_surface_new returned NULL. Context:
          runtime.app != nil:    \(appNonNil)
          workingDirectory:      \(workingDirectory)
            exists:              \(cwdExists)
            isDirectory:         \(isDirectory)
          fontSize:              \(fontSize)
          scale_factor:          \(scaleFactor)
          env_var_count:         \(envVarCount)
          env (first 8):         \(envSummary)
          initialInput (chars):  \(initialInputCount)

        """
    }

    /// Arm the display-reconfiguration observer (idempotent). Installed
    /// only while we have no surface. `didChangeScreenParametersNotification`
    /// fires when a display is connected/disconnected, woken, or the lid
    /// is opened — exactly the events that flip the active-display count
    /// back above 0 and let `ghostty_surface_new` succeed on retry.
    private func installScreenObserverIfNeeded() {
        guard screenObserver == nil else { return }
        screenObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.didChangeScreenParametersNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            // addObserver(queue: .main) guarantees main-thread delivery,
            // so asserting MainActor isolation here is sound.
            MainActor.assumeIsolated {
                self?.attemptSurfaceCreation()
            }
        }
    }

    private func removeScreenObserver() {
        guard let screenObserver else { return }
        NotificationCenter.default.removeObserver(screenObserver)
        self.screenObserver = nil
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    // Isolated to MainActor so Swift 6.3 strict concurrency lets us
    // touch `pendingGeometrySync` (a non-Sendable `DispatchWorkItem?`)
    // from deinit. The host view is always created and torn down on
    // the main thread (NSView lifecycle), so MainActor isolation is
    // accurate, not just convenient. Without this annotation Swift
    // 6.3 errors with "cannot access property 'pendingGeometrySync'
    // with a non-Sendable type 'DispatchWorkItem?' from nonisolated
    // deinit" — the build break PR #215 left on main.
    @MainActor
    deinit {
        pendingGeometrySync?.cancel()
        claudeMonitorTimer?.invalidate()
        removeScreenObserver()
        if let surface {
            // Drain any pending async focus call before freeing. focusQueue
            // is a serial background queue so sync'ing to it here cannot
            // deadlock (different thread from MainActor). In practice terminal
            // views have app lifetime so this barrier is a no-op, but it
            // ensures correctness if a view is ever torn down mid-flight.
            Self.focusQueue.sync {}
            // Clear focus before freeing so libghostty's focused-surface
            // bookkeeping doesn't dangle into a freed surface. Action
            // callbacks fire on the main thread via ghostty_app_tick (see
            // PR #209), and the focusQueue barrier above guarantees no
            // async focus call is in progress, so the userdata pointer
            // (this view) stays valid until deinit returns.
            ghostty_surface_set_focus(surface, false)
            ghostty_surface_free(surface)
        }
    }

    override var acceptsFirstResponder: Bool { true }

    override func becomeFirstResponder() -> Bool {
        let accepted = super.becomeFirstResponder()
        if accepted, let surface {
            // Dispatch off the main thread — see focusQueue doc-comment above.
            let box = SurfaceBox(surface: surface)
            Self.focusQueue.async {
                ghostty_surface_set_focus(box.surface, true)
            }
        }
        return accepted
    }

    override func resignFirstResponder() -> Bool {
        let accepted = super.resignFirstResponder()
        if accepted, let surface {
            // Dispatch off the main thread for symmetry with becomeFirstResponder.
            let box = SurfaceBox(surface: surface)
            Self.focusQueue.async {
                ghostty_surface_set_focus(box.surface, false)
            }
        }
        return accepted
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()

        // If surface creation failed earlier (e.g. no active display at
        // init time), gaining a window is a good moment to retry — the
        // window carries a screen once one is available.
        if surface == nil, window != nil {
            attemptSurfaceCreation()
        }

        reconcileClaudeMonitor()
        syncGeometry()
    }

    override func layout() {
        super.layout()
        syncGeometry()
    }

    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        syncGeometry()
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()

        if let trackingAreaRef {
            removeTrackingArea(trackingAreaRef)
        }

        let trackingArea = NSTrackingArea(
            rect: bounds,
            options: [.activeAlways, .inVisibleRect, .mouseEnteredAndExited, .mouseMoved],
            owner: self,
            userInfo: nil
        )
        addTrackingArea(trackingArea)
        trackingAreaRef = trackingArea
    }

    override func resetCursorRects() {
        discardCursorRects()
        addCursorRect(bounds, cursor: currentCursor)
    }

    override func mouseEntered(with event: NSEvent) {
        super.mouseEntered(with: event)
        currentCursor.set()
    }

    override func mouseMoved(with event: NSEvent) {
        sendMousePosition(event)
    }

    override func mouseDragged(with event: NSEvent) {
        sendMousePosition(event)
    }

    override func rightMouseDragged(with event: NSEvent) {
        sendMousePosition(event)
    }

    override func otherMouseDragged(with event: NSEvent) {
        sendMousePosition(event)
    }

    override func mouseDown(with event: NSEvent) {
        window?.makeFirstResponder(self)
        sendMouseButton(event, state: GHOSTTY_MOUSE_PRESS, button: GHOSTTY_MOUSE_LEFT)
    }

    override func mouseUp(with event: NSEvent) {
        sendMouseButton(event, state: GHOSTTY_MOUSE_RELEASE, button: GHOSTTY_MOUSE_LEFT)
    }

    override func rightMouseDown(with event: NSEvent) {
        window?.makeFirstResponder(self)
        sendMouseButton(event, state: GHOSTTY_MOUSE_PRESS, button: GHOSTTY_MOUSE_RIGHT)
    }

    override func rightMouseUp(with event: NSEvent) {
        sendMouseButton(event, state: GHOSTTY_MOUSE_RELEASE, button: GHOSTTY_MOUSE_RIGHT)
    }

    override func otherMouseDown(with event: NSEvent) {
        window?.makeFirstResponder(self)
        sendMouseButton(event, state: GHOSTTY_MOUSE_PRESS, button: otherButton(for: event))
    }

    override func otherMouseUp(with event: NSEvent) {
        sendMouseButton(event, state: GHOSTTY_MOUSE_RELEASE, button: otherButton(for: event))
    }

    override func scrollWheel(with event: NSEvent) {
        guard let surface else { return }
        var mods = Int32(0)
        if event.hasPreciseScrollingDeltas {
            mods |= 1
        }
        ghostty_surface_mouse_scroll(surface, event.scrollingDeltaX, event.scrollingDeltaY, mods)
    }

    override func keyDown(with event: NSEvent) {
        sendKey(event, action: event.isARepeat ? GHOSTTY_ACTION_REPEAT : GHOSTTY_ACTION_PRESS)
    }

    override func keyUp(with event: NSEvent) {
        sendKey(event, action: GHOSTTY_ACTION_RELEASE, includeText: false)
    }

    func syncGeometry() {
        guard surface != nil else { return }

        let now = ProcessInfo.processInfo.systemUptime
        let elapsed = now - lastSizeSyncTimestamp

        if elapsed >= Self.geometrySyncMinInterval {
            pendingGeometrySync?.cancel()
            pendingGeometrySync = nil
            applyGeometryNow(timestamp: now)
            return
        }

        // We synced recently; coalesce until the throttle window
        // expires so a rapid drag results in one trailing reflow
        // rather than dozens.
        if pendingGeometrySync != nil {
            return
        }

        let work = DispatchWorkItem { [weak self] in
            guard let self else { return }
            self.pendingGeometrySync = nil
            self.applyGeometryNow(timestamp: ProcessInfo.processInfo.systemUptime)
        }
        pendingGeometrySync = work
        let delay = Self.geometrySyncMinInterval - elapsed
        DispatchQueue.main.asyncAfter(deadline: .now() + delay, execute: work)
    }

    private func applyGeometryNow(timestamp: TimeInterval) {
        guard let surface else { return }

        let size = convertToBacking(bounds.size)
        let target = CGSize(
            width: max(size.width, 1).rounded(.up),
            height: max(size.height, 1).rounded(.up)
        )

        if target != lastSyncedBackingSize {
            lastSyncedBackingSize = target
            ghostty_surface_set_size(surface, UInt32(target.width), UInt32(target.height))
        }

        if let window {
            ghostty_surface_set_content_scale(surface, window.backingScaleFactor, window.backingScaleFactor)
            if let screen = window.screen {
                ghostty_surface_set_display_id(surface, screen.ghosttyDisplayID)
            }
        }

        lastSizeSyncTimestamp = timestamp
    }

    func applyInitialSize(_ size: ghostty_action_initial_size_s) {
        session.statusMessage = "Initial size \(size.width)x\(size.height)"
    }

    /// Type `text` into the surface and submit it, as if the user had
    /// pasted the body and then pressed Return. Used by engine→app
    /// `SendToPane` requests (probe injection, `bossctl agents send`,
    /// the macOS intervene affordance).
    ///
    /// The submit step is essential: Claude Code's TUI reads input
    /// through libghostty's bracketed-paste path, which delivers the
    /// payload to the pty but does *not* synthesize an Enter keypress
    /// — a literal `\n` inside paste content lands as a newline in the
    /// input field, not as "submit". Without an explicit Return key
    /// after the paste the prompt sits in the worker's input buffer
    /// until a human focuses the pane, which defeats the whole point
    /// of an intervene. Trailing newline characters in the body are
    /// stripped first so the input field doesn't end up with a stray
    /// blank line before submission.
    func submitText(_ text: String) {
        guard let surface else { return }
        let plan = Self.submissionPlan(for: text)
        if !plan.body.isEmpty {
            plan.body.withCString { ptr in
                ghostty_surface_text(surface, ptr, UInt(strlen(ptr)))
            }
        }
        if plan.sendReturn {
            sendReturnKey()
        }
    }

    /// Pure helper that decides how to break a `SendToPane` payload
    /// into (a) the body that should be pasted via
    /// `ghostty_surface_text` and (b) whether a Return keystroke
    /// should follow. Factored out so the trailing-newline stripping
    /// is unit-testable without standing up a libghostty surface.
    static func submissionPlan(for raw: String) -> PaneSubmissionPlan {
        // Swift collapses a CRLF pair into a single extended grapheme
        // cluster, so working on `Character`s here would miss a lone
        // `\r` that a mid-pasted line had left behind. Step through
        // Unicode scalars instead so every trailing CR/LF byte gets
        // stripped — including a `\r` the grapheme view had stitched
        // together with a preceding `\n`.
        var scalars = raw.unicodeScalars
        while let last = scalars.last, last == "\n" || last == "\r" {
            scalars.removeLast()
        }
        return PaneSubmissionPlan(body: String(scalars), sendReturn: true)
    }

    /// Synthesise a Return keypress on the surface. Mirrors
    /// `sendInterrupt` in shape (programmatic `ghostty_surface_key`
    /// call with the macOS hardware keycode and the unshifted code
    /// point) so libghostty's keymap path produces the same byte
    /// sequence the TUI sees from a real keystroke. `ghostty_surface_text`
    /// is the paste pathway and intentionally drops control characters,
    /// so it cannot stand in for a real Enter.
    private func sendReturnKey() {
        guard let surface else { return }
        var keyEvent = ghostty_input_key_s()
        keyEvent.action = GHOSTTY_ACTION_PRESS
        keyEvent.mods = GHOSTTY_MODS_NONE
        keyEvent.consumed_mods = GHOSTTY_MODS_NONE
        // macOS hardware keycode for Return (kVK_Return = 0x24).
        keyEvent.keycode = 0x24
        keyEvent.text = nil
        keyEvent.composing = false
        // 0x0D is carriage return — what a TUI sees from a real Enter.
        keyEvent.unshifted_codepoint = 0x0D
        _ = ghostty_surface_key(surface, keyEvent)
    }

    /// Synthesise an Esc keypress on the surface — the same key path
    /// used by `keyDown(with:)`, just sourced from a programmatic
    /// caller instead of an NSEvent. libghostty translates the
    /// keycode and writes the ESC byte sequence to the pty so the
    /// child process (Claude) sees it as a real Esc. Used by the
    /// engine→app `InterruptWorkerPane` request (`bossctl agents
    /// interrupt`).
    ///
    /// `ghostty_surface_text` is *not* viable here — its docstring
    /// is explicit that it's the paste pathway and intentionally
    /// drops escape sequences.
    func sendInterrupt() {
        guard let surface else { return }
        var keyEvent = ghostty_input_key_s()
        keyEvent.action = GHOSTTY_ACTION_PRESS
        keyEvent.mods = GHOSTTY_MODS_NONE
        keyEvent.consumed_mods = GHOSTTY_MODS_NONE
        // macOS hardware keycode for Escape (kVK_Escape = 0x35).
        // libghostty's embedded apprt looks up the physical key by
        // matching its native-keycode table, so passing the raw
        // macOS keycode is the same shape `keyDown(with:)` produces.
        keyEvent.keycode = 0x35
        keyEvent.text = nil
        keyEvent.composing = false
        keyEvent.unshifted_codepoint = 0x1B
        _ = ghostty_surface_key(surface, keyEvent)
    }

    func setCellSize(_ size: ghostty_action_cell_size_s) {
        session.statusMessage = "Cell \(size.width)x\(size.height)"
    }

    func applyColorChange(_ change: ghostty_action_color_change_s) {
        guard change.kind == GHOSTTY_ACTION_COLOR_KIND_BACKGROUND else { return }
        backgroundColor = NSColor(
            calibratedRed: CGFloat(change.r) / 255.0,
            green: CGFloat(change.g) / 255.0,
            blue: CGFloat(change.b) / 255.0,
            alpha: 1.0
        )
        layer?.backgroundColor = backgroundColor.cgColor
    }

    func setCursorShape(_ shape: ghostty_action_mouse_shape_e) {
        currentCursor = switch shape {
        case GHOSTTY_MOUSE_SHAPE_POINTER:
            .pointingHand
        case GHOSTTY_MOUSE_SHAPE_TEXT:
            .iBeam
        case GHOSTTY_MOUSE_SHAPE_CROSSHAIR:
            .crosshair
        case GHOSTTY_MOUSE_SHAPE_NOT_ALLOWED:
            .operationNotAllowed
        case GHOSTTY_MOUSE_SHAPE_W_RESIZE, GHOSTTY_MOUSE_SHAPE_E_RESIZE, GHOSTTY_MOUSE_SHAPE_EW_RESIZE:
            .resizeLeftRight
        case GHOSTTY_MOUSE_SHAPE_N_RESIZE, GHOSTTY_MOUSE_SHAPE_S_RESIZE, GHOSTTY_MOUSE_SHAPE_NS_RESIZE:
            .resizeUpDown
        default:
            .arrow
        }

        window?.invalidateCursorRects(for: self)
    }

    func setCursorVisible(_ visible: Bool) {
        guard cursorVisible != visible else { return }
        cursorVisible = visible
        NSCursor.setHiddenUntilMouseMoves(!visible)
    }

    /// Converge `claudeMonitorTimer` on the desired state given the
    /// pane's window attachment and the `claudeMonitorEnabled` gate.
    /// `updateNSView` passes the latest SwiftUI input via `enabled:`;
    /// the internal callers (surface creation, `viewDidMoveToWindow`)
    /// omit it and reconcile against the last-known value. Idempotent:
    /// safe to call from `init`, `viewDidMoveToWindow`, and
    /// `NSViewRepresentable.updateNSView` without duplicating work. The
    /// screen-scrape is the only main-thread cost in this view that runs
    /// on a regular timer, so a worker pane drops to ~zero baseline once
    /// the engine starts pushing `LiveWorkerState`.
    func reconcileClaudeMonitor(enabled: Bool? = nil) {
        if let enabled {
            claudeMonitorEnabled = enabled
        }
        let shouldRun = window != nil && claudeMonitorEnabled
        if shouldRun {
            if claudeMonitorTimer == nil {
                startClaudeMonitor()
            }
        } else if claudeMonitorTimer != nil {
            stopClaudeMonitor()
        }
    }

    private func startClaudeMonitor() {
        claudeMonitorTimer?.invalidate()
        claudeMonitorTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            Task { @MainActor [weak self] in
                self?.updateClaudeMonitorState()
            }
        }
        claudeMonitorTimer?.tolerance = 0.1
        updateClaudeMonitorState()
    }

    private func stopClaudeMonitor() {
        claudeMonitorTimer?.invalidate()
        claudeMonitorTimer = nil
        session.updateClaudeMonitor(snapshot: nil)
    }

    private func updateClaudeMonitorState() {
        guard let surface else {
            session.updateClaudeMonitor(snapshot: nil)
            return
        }

        let visibleContents = readVisibleContents(from: surface)
        session.updateClaudeMonitor(snapshot: makeClaudeSnapshot(from: visibleContents))
    }

    private func readVisibleContents(from surface: ghostty_surface_t) -> String {
        var text = ghostty_text_s()
        let selection = ghostty_selection_s(
            top_left: ghostty_point_s(
                tag: GHOSTTY_POINT_VIEWPORT,
                coord: GHOSTTY_POINT_COORD_TOP_LEFT,
                x: 0,
                y: 0
            ),
            bottom_right: ghostty_point_s(
                tag: GHOSTTY_POINT_VIEWPORT,
                coord: GHOSTTY_POINT_COORD_BOTTOM_RIGHT,
                x: 0,
                y: 0
            ),
            rectangle: false
        )

        guard ghostty_surface_read_text(surface, selection, &text) else {
            return ""
        }

        defer { ghostty_surface_free_text(surface, &text) }
        return String(cString: text.text)
    }

    private func makeClaudeSnapshot(from visibleContents: String) -> ClaudeMonitorSnapshot? {
        let trimmedContents = visibleContents.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedContents.isEmpty else { return nil }

        let prompt = promptLine(in: visibleContents)
        return ClaudeMonitorSnapshot(
            tail: extractTail(from: visibleContents, keepLines: 24),
            claudeVisible: visibleContents.contains("Claude Code") ||
                visibleContents.contains("auto mode on") ||
                visibleContents.contains("/effort"),
            busy: visibleContents.localizedCaseInsensitiveContains("esc to interrupt"),
            promptVisible: prompt != nil,
            promptLine: prompt,
            starting: visibleContents.contains("Accessing workspace:") ||
                visibleContents.contains("Quick safety check:")
        )
    }

    private func extractTail(from text: String, keepLines: Int) -> String {
        let lines = text
            .split(whereSeparator: \.isNewline)
            .map(String.init)
            .filter { !$0.trimmingCharacters(in: .whitespaces).isEmpty }

        guard !lines.isEmpty else { return "" }
        return lines.suffix(keepLines).joined(separator: "\n")
    }

    private func promptLine(in text: String) -> String? {
        for line in text.split(whereSeparator: \.isNewline).reversed() {
            let value = String(line)
            if value.trimmingCharacters(in: .whitespaces).hasPrefix("❯") {
                return value
            }
        }

        return nil
    }

    private func sendMouseButton(
        _ event: NSEvent,
        state: ghostty_input_mouse_state_e,
        button: ghostty_input_mouse_button_e
    ) {
        guard let surface else { return }
        ghostty_surface_mouse_button(surface, state, button, ghosttyMods(event.modifierFlags))
        sendMousePosition(event)
    }

    private func sendMousePosition(_ event: NSEvent) {
        guard let surface else { return }
        let point = convert(event.locationInWindow, from: nil)
        ghostty_surface_mouse_pos(
            surface,
            point.x,
            bounds.height - point.y,
            ghosttyMods(event.modifierFlags)
        )
    }

    private func sendKey(
        _ event: NSEvent,
        action: ghostty_input_action_e,
        includeText: Bool = true
    ) {
        guard let surface else { return }

        let translationMods = ghostty_surface_key_translation_mods(
            surface,
            ghosttyMods(event.modifierFlags)
        )

        var keyEvent = ghostty_input_key_s()
        keyEvent.action = action
        keyEvent.mods = ghosttyMods(event.modifierFlags)
        keyEvent.consumed_mods = ghostty_input_mods_e(
            rawValue: translationMods.rawValue & ~ghosttyMods([.control, .command]).rawValue
        )
        keyEvent.keycode = UInt32(event.keyCode)
        keyEvent.text = nil
        keyEvent.composing = false

        if let characters = event.characters(byApplyingModifiers: []),
           let scalar = characters.unicodeScalars.first {
            keyEvent.unshifted_codepoint = scalar.value
        } else {
            keyEvent.unshifted_codepoint = 0
        }

        guard includeText, let text = ghosttyCharacters(for: event), !text.isEmpty else {
            _ = ghostty_surface_key(surface, keyEvent)
            return
        }

        if let firstByte = text.utf8.first, firstByte >= 0x20 {
            text.withCString { ptr in
                keyEvent.text = ptr
                _ = ghostty_surface_key(surface, keyEvent)
            }
        } else {
            _ = ghostty_surface_key(surface, keyEvent)
        }
    }

    private func ghosttyCharacters(for event: NSEvent) -> String? {
        guard let characters = event.characters else { return nil }

        if characters.count == 1, let scalar = characters.unicodeScalars.first {
            if scalar.value < 0x20 {
                return event.characters(byApplyingModifiers: event.modifierFlags.subtracting(.control))
            }

            if scalar.value >= 0xF700 && scalar.value <= 0xF8FF {
                return nil
            }
        }

        return characters
    }

    private func ghosttyMods(_ flags: NSEvent.ModifierFlags) -> ghostty_input_mods_e {
        var rawValue = GHOSTTY_MODS_NONE.rawValue
        if flags.contains(.shift) { rawValue |= GHOSTTY_MODS_SHIFT.rawValue }
        if flags.contains(.control) { rawValue |= GHOSTTY_MODS_CTRL.rawValue }
        if flags.contains(.option) { rawValue |= GHOSTTY_MODS_ALT.rawValue }
        if flags.contains(.command) { rawValue |= GHOSTTY_MODS_SUPER.rawValue }
        if flags.contains(.capsLock) { rawValue |= GHOSTTY_MODS_CAPS.rawValue }
        if flags.contains(.numericPad) { rawValue |= GHOSTTY_MODS_NUM.rawValue }
        return ghostty_input_mods_e(rawValue: rawValue)
    }

    private func otherButton(for event: NSEvent) -> ghostty_input_mouse_button_e {
        switch event.buttonNumber {
        case 2:
            return GHOSTTY_MOUSE_MIDDLE
        case 3:
            return GHOSTTY_MOUSE_FOUR
        case 4:
            return GHOSTTY_MOUSE_FIVE
        default:
            return GHOSTTY_MOUSE_UNKNOWN
        }
    }
}
