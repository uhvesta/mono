import AppKit
import SwiftUI

/// A text editor that submits on plain Return and inserts a newline on Shift+Return.
/// Replaces SwiftUI's TextEditor (which treats all Return keys as newlines) for the
/// comment entry form.
struct CommentTextEditor: NSViewRepresentable {
    @Binding var text: String
    var onSubmit: () -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(self)
    }

    func makeNSView(context: Context) -> NSScrollView {
        let textView = NSTextView()
        textView.delegate = context.coordinator
        textView.isRichText = false
        textView.allowsUndo = true
        textView.isEditable = true
        textView.isSelectable = true
        textView.font = NSFont.systemFont(ofSize: NSFont.systemFontSize(for: .regular) - 1)
        textView.textContainerInset = NSSize(width: 6, height: 6)
        textView.isVerticallyResizable = true
        textView.autoresizingMask = [.width]
        textView.textContainer?.widthTracksTextView = true
        textView.backgroundColor = .clear
        textView.drawsBackground = false

        let scrollView = NSScrollView()
        scrollView.hasVerticalScroller = true
        scrollView.documentView = textView
        scrollView.drawsBackground = false
        scrollView.backgroundColor = .clear
        scrollView.borderType = .noBorder

        context.coordinator.textView = textView

        // Focus the text view after it's in the view hierarchy
        DispatchQueue.main.async {
            textView.window?.makeFirstResponder(textView)
        }

        return scrollView
    }

    func updateNSView(_ scrollView: NSScrollView, context: Context) {
        guard let textView = scrollView.documentView as? NSTextView else { return }
        if textView.string != text {
            let wasEmpty = textView.string.isEmpty
            let sel = textView.selectedRanges
            textView.string = text

            if wasEmpty && !text.isEmpty {
                // Initial text being set; position cursor at the end
                textView.setSelectedRange(NSRange(location: text.utf16.count, length: 0))
            } else {
                textView.selectedRanges = sel
            }
        }
    }

    final class Coordinator: NSObject, NSTextViewDelegate {
        var parent: CommentTextEditor
        weak var textView: NSTextView?

        init(_ parent: CommentTextEditor) {
            self.parent = parent
        }

        func textDidChange(_ notification: Notification) {
            guard let tv = notification.object as? NSTextView else { return }
            parent.text = tv.string
        }

        func textView(
            _ textView: NSTextView,
            doCommandBy commandSelector: Selector
        ) -> Bool {
            if commandSelector == #selector(NSResponder.insertNewline(_:)) {
                // Plain Return → submit
                parent.onSubmit()
                return true
            }
            if commandSelector == #selector(NSResponder.insertNewlineIgnoringFieldEditor(_:)) {
                // Shift+Return → insert literal newline
                textView.insertNewline(nil)
                return true
            }
            return false
        }
    }
}
