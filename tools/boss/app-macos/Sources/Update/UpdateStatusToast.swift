import SwiftUI
import UpdateCore

/// Transient HUD shown top-trailing after a manual "Check for Updates" action.
/// Auto-dismissed by ``UpdateModel``; no user interaction required.
struct UpdateStatusToast: View {
    let feedback: ManualUpdateFeedback

    var body: some View {
        HStack(spacing: 8) {
            leadingIcon
            Text(label)
                .font(.callout)
                .lineLimit(1)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10))
        .shadow(color: .black.opacity(0.15), radius: 8, y: 2)
    }

    @ViewBuilder
    private var leadingIcon: some View {
        switch feedback {
        case .checking:
            ProgressView().controlSize(.small)
        case .upToDate:
            Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
        case .networkError:
            Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
        case .rateLimited:
            Image(systemName: "clock.fill").foregroundStyle(.orange)
        }
    }

    private var label: String {
        switch feedback {
        case .checking:
            return "Checking for updates…"
        case .upToDate:
            let version = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "?"
            return "You're up to date (v\(version))"
        case .networkError(let message):
            return "Update check failed: \(message)"
        case .rateLimited(let retryAfter):
            return "Rate limit — try after \(retryAfter.formatted(.dateTime.hour().minute()))"
        }
    }
}
