@preconcurrency import AppKit
import os.log
import SwiftUI

private let anchorLog = Logger(subsystem: "com.boss.app", category: "CommentPopupAnchor")
/// Stable category for window↔view coordinate-bridge instrumentation. Both the comment
/// popover anchor and the right-click context menu funnel their NSEvent.locationInWindow
/// through `windowPointToView(_:in:)`, which logs here. Keep these lines in place — they
/// are the regression tripwire for the SwiftUI/AppKit isFlipped mismatch that produced
/// the FOURTH-report top↔bottom mirror in the markdown viewer.
///
/// Stream live with:
///   log stream --predicate 'subsystem == "com.boss.markdown" AND category == "coordinates"' --style compact
private let coordLog = Logger(subsystem: "com.boss.markdown", category: "coordinates")

/// Owns the in-memory comment array for a single markdown viewer instance
/// and coordinates the selection → authoring → sidebar → highlight flow.
///
/// Phase 1: all state is in-memory; no engine RPCs; closing the viewer
/// loses all comments. This is intentional and surfaced to the user in
/// the sidebar header.
@MainActor
final class CommentLayer: NSObject, ObservableObject {
    @Published var comments: [Comment] = []
    @Published var isShowingPopover: Bool = false
    @Published var pendingQuotedText: String = ""
    /// Character that seeded the form via type-to-comment entry path.
    @Published var pendingFirstChar: Character? = nil
    /// Quoted text of the comment just clicked in the sidebar; clears after the flash.
    @Published var flashingText: String? = nil

    // NSEvent monitor tokens; stored nonisolated(unsafe) because the opaque Any
    // tokens are installed/removed only on the main actor.
    nonisolated(unsafe) private var keyMonitor: Any?
    nonisolated(unsafe) private var rightClickMonitor: Any?
    nonisolated(unsafe) private var mouseUpMonitor: Any?

    /// The NSTextView whose selection seeded the pending comment request.
    /// Captured from NSTextView.didChangeSelectionNotification (the object is the text view).
    /// Queried at present-time via firstRect(forCharacterRange:) — never cached as screen coords.
    private weak var anchorTextView: NSTextView?

    // MARK: - Textual/NSTextInteractionView anchor
    //
    // Textual's NSTextInteractionView does NOT post NSTextView.didChangeSelectionNotification,
    // so anchorTextView is never populated from StructuredText selections. Instead we install
    // a leftMouseUp monitor and capture the mouse position (in screen coords) at the moment
    // the user finishes dragging/clicking to make a selection. resolveAnchor() uses this as
    // the popup anchor when anchorTextView is nil.

    /// Screen-space point saved on leftMouseUp while a non-NSTextView first-responder has a selection.
    private var anchorInteractionScreenPoint: NSPoint?
    /// The NSView (Textual's NSTextInteractionView) that owned the selection at mouseUp time.
    private weak var anchorInteractionView: NSView?

    /// The live NSPopover, if one is currently visible.
    private var activePopover: NSPopover?

    // MARK: - Monitor lifecycle

    func installMonitors() {
        // ObjC selector form avoids the @Sendable closure constraint on the block-based
        // addObserver API, which would make `notification` a sending parameter and prevent
        // capturing the non-Sendable NSTextView inside assumeIsolated.
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(textViewSelectionDidChange(_:)),
            name: NSTextView.didChangeSelectionNotification,
            object: nil
        )
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self else { return event }
            let chars = event.charactersIgnoringModifiers
            let mods = event.modifierFlags
            let consume = MainActor.assumeIsolated {
                self.shouldConsumeKeyEvent(chars: chars, mods: mods)
            }
            return consume ? nil : event
        }
        rightClickMonitor = NSEvent.addLocalMonitorForEvents(matching: .rightMouseDown) { [weak self] event in
            guard let self else { return event }
            let loc = event.locationInWindow
            let win = event.window
            let consume = MainActor.assumeIsolated {
                self.handleRightClick(locationInWindow: loc, window: win)
            }
            return consume ? nil : event
        }
        // Capture selection endpoint for Textual's NSTextInteractionView, which does not
        // post NSTextView.didChangeSelectionNotification.
        mouseUpMonitor = NSEvent.addLocalMonitorForEvents(matching: .leftMouseUp) { [weak self] event in
            guard let self else { return event }
            let loc = event.locationInWindow
            let win = event.window
            MainActor.assumeIsolated {
                self.captureInteractionAnchor(locationInWindow: loc, window: win)
            }
            return event
        }
    }

    func removeMonitors() {
        if let m = keyMonitor { NSEvent.removeMonitor(m); keyMonitor = nil }
        if let m = rightClickMonitor { NSEvent.removeMonitor(m); rightClickMonitor = nil }
        if let m = mouseUpMonitor { NSEvent.removeMonitor(m); mouseUpMonitor = nil }
        NotificationCenter.default.removeObserver(
            self, name: NSTextView.didChangeSelectionNotification, object: nil)
        activePopover?.close()
        anchorInteractionScreenPoint = nil
        anchorInteractionView = nil
    }

    /// Single coordinate-bridge between an NSEvent's window-space location and a target
    /// view's coordinate system. Both popover-anchor capture and the right-click context
    /// menu call this — any future surface that opens a popover/menu in response to a
    /// click MUST also route through here, never pass `event.locationInWindow` directly
    /// to APIs whose point argument is documented in view coords.
    ///
    /// Why this helper exists: when the window's contentView is SwiftUI's NSHostingView
    /// (`isFlipped == true`) but the NSWindow is bottom-left origin, passing
    /// `event.locationInWindow` straight to APIs like `NSMenu.popUp(at:in:)` — whose
    /// `at:` is in the receiving view's coordinate system — produces a `viewHeight - y`
    /// inversion. A click near the top of the document opens the menu near the bottom
    /// and vice versa (the top↔bottom mirror reported four times against the markdown
    /// viewer). `view.convert(_, from: nil)` is the AppKit-blessed bridge that walks
    /// the view hierarchy and applies every `isFlipped` along the way.
    private func windowPointToView(_ pointInWindow: NSPoint, in view: NSView) -> NSPoint {
        let pointInView = view.convert(pointInWindow, from: nil)
        coordLog.info("windowPointToView: window=\(NSStringFromPoint(pointInWindow)) → view=\(NSStringFromPoint(pointInView)) (\(NSStringFromClass(type(of: view))) isFlipped=\(view.isFlipped) bounds=\(NSStringFromRect(view.bounds)))")
        return pointInView
    }

    /// Called on leftMouseUp. If the current first responder is a non-NSTextView view
    /// (Textual's NSTextInteractionView) whose bounds contain the mouseUp point and there
    /// is a live selection, saves the screen-space anchor for use by resolveAnchor().
    /// The bounds-containment guard prevents the "Add Comment" button click (which lands
    /// outside the text view) from overwriting a valid earlier anchor.
    private func captureInteractionAnchor(locationInWindow: NSPoint, window: NSWindow?) {
        guard !isShowingPopover, let window else { return }
        guard let responder = window.firstResponder as? NSView,
              !(responder is NSTextView) else { return }
        let pointInResponder = windowPointToView(locationInWindow, in: responder)
        guard responder.bounds.contains(pointInResponder) else {
            anchorLog.info("captureInteractionAnchor: mouseUp outside responder bounds (\(NSStringFromClass(type(of: responder))) bounds=\(NSStringFromRect(responder.bounds)) point=\(NSStringFromPoint(pointInResponder))) — anchor not updated")
            return
        }
        guard hasCurrentSelection() else { return }
        let screenOrigin = window.convertToScreen(
            NSRect(origin: locationInWindow, size: CGSize(width: 1, height: 1))
        ).origin
        anchorInteractionScreenPoint = screenOrigin
        anchorInteractionView = responder
        anchorLog.info("captureInteractionAnchor: stored screen anchor \(NSStringFromPoint(screenOrigin)) responder=\(NSStringFromClass(type(of: responder))) bounds=\(NSStringFromRect(responder.bounds))")
    }

    /// Called by NotificationCenter on the main thread when any NSTextView changes selection.
    /// Using the ObjC selector form avoids @Sendable parameter constraints that prevent
    /// capturing the non-Sendable NSTextView across a @Sendable closure boundary.
    @objc nonisolated private func textViewSelectionDidChange(_ notification: Notification) {
        let textView = notification.object as? NSTextView
        MainActor.assumeIsolated { [weak self] in
            guard let self, !self.isShowingPopover else { return }
            // Only update while the popover is closed; the comment form's own
            // NSTextView (CommentTextEditor) would otherwise overwrite the anchor.
            self.anchorTextView = textView
            // An NSTextView firing means focus moved to an NSTextView area; clear the
            // Textual interaction anchor so the NSTextView path runs in resolveAnchor().
            self.anchorInteractionScreenPoint = nil
            self.anchorInteractionView = nil
            if let tv = textView {
                let range = tv.selectedRange()
                anchorLog.info("textViewSelectionDidChange: NSTextView \(NSStringFromClass(type(of: tv))) range=\(NSStringFromRange(range))")
            }
        }
    }

    // MARK: - Authoring

    func requestNewComment(firstChar: Character? = nil) {
        pendingQuotedText = captureCurrentSelection() ?? ""
        pendingFirstChar = firstChar

        guard let (posRect, posView) = resolveAnchor() else {
            anchorLog.error("requestNewComment: resolveAnchor returned nil — popover not shown")
            return
        }

        anchorLog.info("requestNewComment: showing popover relativeTo=\(NSStringFromRect(posRect)) of=\(NSStringFromClass(type(of: posView))) isFlipped=\(posView.isFlipped)")
        coordLog.info("requestNewComment: popover relativeTo \(NSStringFromRect(posRect)) of \(NSStringFromClass(type(of: posView))) isFlipped=\(posView.isFlipped) bounds=\(NSStringFromRect(posView.bounds))")

        let popover = NSPopover()
        popover.contentViewController = NSHostingController(
            rootView: CommentPopover(layer: self)
        )
        // Transient: clicks outside the popover dismiss it automatically, matching
        // the previous SwiftUI .popover default behaviour.
        popover.behavior = .transient
        // NSPopover.delegate is weak; self outlives the popover so this is safe.
        popover.delegate = self
        activePopover = popover
        isShowingPopover = true

        popover.show(relativeTo: posRect, of: posView, preferredEdge: .maxY)
    }

    func addComment(quoted: String, body: String) {
        guard !body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        let comment = Comment(
            id: UUID(),
            quotedText: quoted,
            body: body.trimmingCharacters(in: .whitespacesAndNewlines),
            createdAt: Date()
        )
        comments.append(comment)
        activePopover?.close()
        activePopover = nil
        isShowingPopover = false
        pendingQuotedText = ""
        pendingFirstChar = nil
    }

    func cancelNewComment() {
        activePopover?.close()
    }

    func dismiss(_ comment: Comment) {
        comments.removeAll { $0.id == comment.id }
    }

    // MARK: - Click-to-jump

    func jumpTo(_ comment: Comment) {
        let text = comment.quotedText
        flashingText = text
        Task {
            try? await Task.sleep(for: .milliseconds(900))
            if flashingText == text { flashingText = nil }
        }
    }

    // MARK: - Selection helpers

    /// Non-destructively checks whether the current first responder has a text selection
    /// by asking it to validate the "Copy" UI item. Textual's NSTextInteractionView
    /// implements NSUserInterfaceValidations: validateUserInterfaceItem returns true for
    /// "copy" only when there is a non-empty selection.
    func hasCurrentSelection() -> Bool {
        guard let firstResponder = NSApp.keyWindow?.firstResponder else { return false }
        let copyItem = NSMenuItem(
            title: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        var responder: NSResponder? = firstResponder
        while let current = responder {
            if let validator = current as? NSUserInterfaceValidations {
                return validator.validateUserInterfaceItem(copyItem)
            }
            responder = current.nextResponder
        }
        return false
    }

    /// Returns the positioning rect (in positioningView coords) and view for NSPopover.
    ///
    /// Priority order:
    ///   1. Textual NSTextInteractionView anchor — mouseUp screen point captured at
    ///      selection-end. Textual does not post NSTextView.didChangeSelectionNotification,
    ///      so the NSTextView path below never fires for StructuredText.
    ///   2. NSTextView anchor — standard path for non-Textual text views (header Text fields).
    ///   3. Fallback near the top of the key window.
    private func resolveAnchor() -> (NSRect, NSView)? {
        // --- 1. Textual NSTextInteractionView anchor ---
        if let view = anchorInteractionView,
           let window = view.window,
           let screenPoint = anchorInteractionScreenPoint {
            // Convert saved screen point → window coords → view (flipped) coords.
            let windowOrigin = window.convertFromScreen(
                NSRect(origin: screenPoint, size: CGSize(width: 1, height: 1))
            ).origin
            let viewPoint = view.convert(windowOrigin, from: nil)
            // A 1-pt-high rect so the popover arrow tip lands at the character baseline.
            let viewRect = NSRect(x: viewPoint.x - 20, y: viewPoint.y, width: 40, height: 1)
            anchorLog.info("resolveAnchor: Textual path — screenPoint=\(NSStringFromPoint(screenPoint)) windowOrigin=\(NSStringFromPoint(windowOrigin)) viewPoint=\(NSStringFromPoint(viewPoint)) viewRect=\(NSStringFromRect(viewRect)) view.isFlipped=\(view.isFlipped)")
            return (viewRect, view)
        }

        // --- 2. NSTextView anchor ---
        if let tv = anchorTextView, let window = tv.window {
            let range = tv.selectedRange()
            anchorLog.info("resolveAnchor: NSTextView path — range=\(NSStringFromRange(range))")
            if range.length > 0, range.location != NSNotFound {
                let lastCharRange = NSRange(location: range.upperBound - 1, length: 1)
                var actualRange = NSRange()
                let screenRect = tv.firstRect(forCharacterRange: lastCharRange, actualRange: &actualRange)
                if screenRect != .zero {
                    // screen → window → text-view coordinates; AppKit handles the conversion
                    // correctly for any display arrangement without explicit screen lookup.
                    let windowRect = window.convertFromScreen(screenRect)
                    let viewRect = tv.convert(windowRect, from: nil)
                    anchorLog.info("resolveAnchor: NSTextView anchor screenRect=\(NSStringFromRect(screenRect)) viewRect=\(NSStringFromRect(viewRect))")
                    return (viewRect, tv)
                }
            }
        }

        // --- 3. Fallback ---
        // Place anchor near the top-left of the key window's content view.
        // Uses minY (top in flipped SwiftUI hosting views) + small offset.
        if let contentView = NSApp.keyWindow?.contentView {
            let topY: CGFloat = contentView.isFlipped
                ? 60                              // flipped: y=0 is top, increase down
                : contentView.bounds.maxY - 60    // non-flipped: maxY is top
            let fallback = NSRect(
                x: contentView.bounds.midX - 8,
                y: topY,
                width: 16,
                height: 16
            )
            anchorLog.warning("resolveAnchor: fallback — anchorTextView=\(String(describing: self.anchorTextView)) anchorInteractionView=\(String(describing: self.anchorInteractionView)) fallback=\(NSStringFromRect(fallback)) isFlipped=\(contentView.isFlipped)")
            return (fallback, contentView)
        }
        return nil
    }

    /// Reads the selection via pasteboard copy. Acceptable Phase 1 trade-off:
    /// called only when the user explicitly opens the comment form.
    private func captureCurrentSelection() -> String? {
        let before = NSPasteboard.general.changeCount
        NSApp.sendAction(#selector(NSText.copy(_:)), to: nil, from: nil)
        guard NSPasteboard.general.changeCount != before else { return nil }
        return NSPasteboard.general.string(forType: .string)
    }

    // MARK: - Event handling (called from monitor closures via MainActor.assumeIsolated)

    /// Returns true if the key event should be consumed (opens the comment form).
    private func shouldConsumeKeyEvent(
        chars: String?,
        mods: NSEvent.ModifierFlags
    ) -> Bool {
        guard !isShowingPopover else { return false }
        let cleanMods = mods.intersection(.deviceIndependentFlagsMask)
        guard cleanMods.isSubset(of: [.shift, .capsLock]) else { return false }
        guard
            let str = chars,
            str.count == 1,
            let char = str.first,
            char.isLetter || char.isNumber || char.isPunctuation || char.isSymbol
        else { return false }
        guard hasCurrentSelection() else { return false }
        requestNewComment(firstChar: char)
        return true
    }

    /// Returns true if the right-click event is consumed (shows our custom context menu).
    ///
    /// `NSMenu.popUp(positioning:at:in:)` interprets `at:` in the *receiving view's*
    /// coordinate system. Under SwiftUI, `window.contentView` is an NSHostingView whose
    /// `isFlipped` is true, while `event.locationInWindow` is in the window's bottom-left
    /// space — passing the window point directly inverts Y and pops the menu at
    /// `viewHeight - clickY`. Route through `windowPointToView` to apply the flip.
    private func handleRightClick(locationInWindow: NSPoint, window: NSWindow?) -> Bool {
        guard !isShowingPopover, hasCurrentSelection() else { return false }
        guard let window, let view = window.contentView else { return false }

        let menu = NSMenu()
        let target = CommentMenuTarget(layer: self)
        let addItem = NSMenuItem(
            title: "Add Comment",
            action: #selector(CommentMenuTarget.addCommentAction),
            keyEquivalent: ""
        )
        addItem.target = target
        addItem.representedObject = target   // keep target alive during menu
        menu.addItem(addItem)
        menu.addItem(.separator())
        menu.addItem(
            NSMenuItem(title: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c"))

        let pointInView = windowPointToView(locationInWindow, in: view)
        coordLog.info("handleRightClick: popUp at view-coords \(NSStringFromPoint(pointInView)) in \(NSStringFromClass(type(of: view))) (was: locationInWindow \(NSStringFromPoint(locationInWindow)) — passing window coords to a flipped contentView produced the top↔bottom mirror)")
        menu.popUp(positioning: nil, at: pointInView, in: view)
        return true
    }
}

// MARK: - NSPopoverDelegate

extension CommentLayer: NSPopoverDelegate {
    /// Called by AppKit when the popover finishes closing, whether by user dismissal or
    /// programmatic close. Resets authoring state. The extension lives in the same file
    /// so it can access private members directly.
    nonisolated func popoverDidClose(_ notification: Notification) {
        Task { @MainActor [weak self] in
            guard let self else { return }
            self.isShowingPopover = false
            self.pendingFirstChar = nil
            self.pendingQuotedText = ""
            self.activePopover = nil
            self.anchorInteractionScreenPoint = nil
            self.anchorInteractionView = nil
        }
    }
}

// MARK: - Menu action target

private final class CommentMenuTarget: NSObject, @unchecked Sendable {
    let layer: CommentLayer
    init(layer: CommentLayer) { self.layer = layer }

    @objc func addCommentAction(_ sender: Any?) {
        Task { @MainActor in layer.requestNewComment() }
    }
}

// MARK: - View modifier

/// Wraps a markdown viewer with the full comment affordance:
/// sidebar (when comments exist), "Add Comment" button, popover authoring form,
/// and three entry paths (type-to-comment, right-click context menu, ⌘⇧K).
///
/// Usage:
/// ```swift
/// MarkdownViewerView(...)
///     .withComments()
/// ```
struct WithCommentsModifier: ViewModifier {
    @StateObject private var layer = CommentLayer()

    func body(content: Content) -> some View {
        let commentedTexts = layer.comments.map(\.quotedText).filter { !$0.isEmpty }
        let flashingText = layer.flashingText

        HStack(spacing: 0) {
            content
                .environment(\.commentedTexts, commentedTexts)
                .environment(\.commentFlashText, flashingText)

            if !layer.comments.isEmpty {
                Divider()
                CommentSidebar(layer: layer)
                    .frame(width: 280)
            }
        }
        .overlay(alignment: .topTrailing) {
            if layer.comments.isEmpty {
                addCommentButton
                    .padding(.trailing, 16)
                    .padding(.top, 20)
            }
        }
        // Hidden button for ⌘⇧K shortcut (⌘⇧M is already the Metrics panel shortcut).
        .background {
            Button("") {
                if layer.hasCurrentSelection() { layer.requestNewComment() }
            }
            .keyboardShortcut("k", modifiers: [.command, .shift])
            .frame(width: 0, height: 0)
            .hidden()
        }
        .onAppear { layer.installMonitors() }
        .onDisappear { layer.removeMonitors() }
    }

    private var addCommentButton: some View {
        Button {
            layer.requestNewComment()
        } label: {
            Label("Add Comment", systemImage: "bubble.left.and.text.bubble.right")
                .font(.callout)
        }
        .buttonStyle(.bordered)
        .controlSize(.small)
        .help("Select text, then click or press ⌘⇧K to add a comment")
    }
}

extension View {
    func withComments() -> some View {
        modifier(WithCommentsModifier())
    }
}

// MARK: - Environment keys

private struct CommentedTextsKey: EnvironmentKey {
    static var defaultValue: [String] { [] }
}

private struct CommentFlashTextKey: EnvironmentKey {
    static var defaultValue: String? { nil }
}

extension EnvironmentValues {
    var commentedTexts: [String] {
        get { self[CommentedTextsKey.self] }
        set { self[CommentedTextsKey.self] = newValue }
    }

    var commentFlashText: String? {
        get { self[CommentFlashTextKey.self] }
        set { self[CommentFlashTextKey.self] = newValue }
    }
}
