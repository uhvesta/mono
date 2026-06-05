import Foundation

extension ChatViewModel {
    // MARK: GitHub OAuth device-flow bridges (OAuth device-flow design §4)
    //
    // Thin pass-throughs to the engine RPCs. The engine owns the flow and
    // the token; these just kick state transitions. The resulting
    // `gitHubAuthState` updates arrive via `git_hub_auth_state` events.

    /// Begin the device flow (the "Connect" / "Start over" action).
    func gitHubAuthConnect() {
        engine.sendGitHubAuthStart()
    }

    /// Abort an in-progress device flow (the "Cancel" action).
    func gitHubAuthCancel() {
        engine.sendGitHubAuthCancel()
    }

    /// Delete the stored token and return to disconnected.
    func gitHubAuthDisconnect() {
        engine.sendGitHubAuthDisconnect()
    }

    /// Re-run the device flow, overwriting the stored token. Identical to
    /// `gitHubAuthConnect` at the wire level (the engine restarts the flow
    /// from `Authorized`); named separately so the call site reads clearly.
    func gitHubAuthReauthorize() {
        engine.sendGitHubAuthStart()
    }

    /// Re-request the current state, which re-runs the engine's org/SSO
    /// probe when connected (the "Re-check" affordance, design §7).
    func gitHubAuthRecheck() {
        engine.sendGitHubAuthStatus()
    }
}
