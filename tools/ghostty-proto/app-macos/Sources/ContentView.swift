import AppKit
import SwiftUI

struct ContentView: View {
    let workspace: TerminalWorkspaceModel

    var body: some View {
        HStack(spacing: 1) {
            ForEach(workspace.panes) { pane in
                TerminalPaneView(
                    runtime: workspace.runtime,
                    session: pane
                )
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .separatorColor))
        .background(WindowFrameConfigurator())
    }
}

private struct TerminalPaneView: View {
    let runtime: GhosttyRuntime
    @ObservedObject var session: TerminalPaneSession

    var body: some View {
        VStack(spacing: 0) {
            paneHeader
            Divider()
            GhosttyTerminalView(
                runtime: runtime,
                session: session,
                launchSpec: session.launchSpec
            )
            .background(Color(nsColor: .black))
        }
        .background(Color(nsColor: .windowBackgroundColor))
    }

    private var paneHeader: some View {
        HStack(spacing: 8) {
            VStack(alignment: .leading, spacing: 1) {
                Text(session.title)
                    .font(.caption.weight(.semibold))
                    .lineLimit(1)

                Text(session.displayTitle)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }

            Spacer(minLength: 0)

            statusPill(session.claudeState.label, color: claudeStateColor)
            statusPill(
                session.rendererHealthy ? "Renderer OK" : "Renderer Pending",
                color: session.rendererHealthy ? .green : .orange
            )
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
    }

    private func statusPill(_ text: String, color: Color) -> some View {
        Text(text)
            .font(.caption2.weight(.medium))
            .lineLimit(1)
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .background(color.opacity(0.14))
            .foregroundStyle(color)
            .clipShape(Capsule())
    }

    private var claudeStateColor: Color {
        switch session.claudeState {
        case .working:
            .blue
        case .ready:
            .green
        case .notDetected:
            .secondary
        case .unavailable:
            .orange
        }
    }
}

private struct WindowFrameConfigurator: NSViewRepresentable {
    func makeNSView(context: Context) -> WindowFrameConfiguratorView {
        WindowFrameConfiguratorView()
    }

    func updateNSView(_ nsView: WindowFrameConfiguratorView, context: Context) {}
}

private final class WindowFrameConfiguratorView: NSView {
    private var configuredWindow = false

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()

        guard !configuredWindow, let window else { return }
        configuredWindow = true

        let visibleFrame = (window.screen ?? NSScreen.main)?.visibleFrame ?? window.frame
        let targetFrame = visibleFrame.insetBy(dx: 24, dy: 96)
        window.setFrame(targetFrame, display: true)
    }
}
