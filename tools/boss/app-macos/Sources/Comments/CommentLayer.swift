@preconcurrency import AppKit
import SwiftUI

/// Owns the in-memory comment array for a single markdown viewer instance
/// and coordinates the selection → authoring → sidebar → highlight flow.
///
/// Phase 1: all state is in-memory; no engine RPCs; closing the viewer
/// loses all comments. This is intentional and surfaced to the user in
/// the sidebar header.
@MainActor
final class CommentLayer: ObservableObject {
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

    // MARK: - Monitor lifecycle

    func installMonitors() {
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self else { return event }
            // Extract Sendable values before crossing into the MainActor isolation context.
            let chars = event.charactersIgnoringModifiers
            let mods = event.modifierFlags
            let consume = MainActor.assumeIsolated {
                self.shouldConsumeKeyEvent(chars: chars, mods: mods)
            }
            return consume ? nil : event
        }
        rightClickMonitor = NSEvent.addLocalMonitorForEvents(matching: .rightMouseDown) { [weak self] event in
            guard let self else { return event }
            // Extract window and location; NSWindow/NSView are also not Sendable, but
            // we wrap them so they stay on the main thread inside assumeIsolated.
            let loc = event.locationInWindow
            let win = event.window
            let consume = MainActor.assumeIsolated {
                self.handleRightClick(locationInWindow: loc, window: win)
            }
            return consume ? nil : event
        }
    }

    func removeMonitors() {
        if let m = keyMonitor { NSEvent.removeMonitor(m); keyMonitor = nil }
        if let m = rightClickMonitor { NSEvent.removeMonitor(m); rightClickMonitor = nil }
    }

    // MARK: - Authoring

    func requestNewComment(firstChar: Character? = nil) {
        pendingQuotedText = captureCurrentSelection() ?? ""
        pendingFirstChar = firstChar
        isShowingPopover = true
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
        isShowingPopover = false
        pendingQuotedText = ""
        pendingFirstChar = nil
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

        menu.popUp(positioning: nil, at: locationInWindow, in: view)
        return true
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
            ZStack(alignment: .topLeading) {
                content
                    .environment(\.commentedTexts, commentedTexts)
                    .environment(\.commentFlashText, flashingText)

                // Zero-size anchor for the popover; appears near the top of the content.
                Color.clear
                    .frame(width: 0, height: 0)
                    .padding(.top, 48)
                    .padding(.leading, 8)
                    .popover(isPresented: $layer.isShowingPopover, arrowEdge: .leading) {
                        CommentPopover(layer: layer)
                    }
            }

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
