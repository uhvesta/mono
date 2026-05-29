import XCTest
@testable import Boss

/// Round-trip / wire-decode tests for the GitHub OAuth device-flow protocol
/// mirrors (`GitHubAuthState`, `OrgAuthState`), plus pure rendering tests for
/// `GitHubAuthPresentation`. Mirrors `ExternalTrackerTests` (DTO decode) and
/// `ExternalTrackerAttentionTests` (presentation rendering).
///
/// The Swift mirrors must decode the same internally-tagged wire JSON the
/// Rust `#[serde(tag = "type", rename_all = "snake_case")]` types produce
/// (snake_case keys; `verification_uri_complete` omitted when nil).
final class GitHubAuthCodableTests: XCTestCase {
    private let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        return e
    }()
    private let decoder = JSONDecoder()

    private func roundtrip<T: Codable & Equatable>(_ value: T) throws -> T {
        let data = try encoder.encode(value)
        return try decoder.decode(T.self, from: data)
    }

    // MARK: GitHubAuthState wire decode

    func testDecodeDisconnected() throws {
        let state = try decoder.decode(GitHubAuthState.self, from: Data(#"{"type":"disconnected"}"#.utf8))
        XCTAssertEqual(state, .disconnected)
    }

    func testDecodeRequestingCode() throws {
        let state = try decoder.decode(GitHubAuthState.self, from: Data(#"{"type":"requesting_code"}"#.utf8))
        XCTAssertEqual(state, .requestingCode)
    }

    func testDecodePendingUserAuthAllFields() throws {
        let wire = """
        {
          "type": "pending_user_auth",
          "user_code": "WDJB-MJHT",
          "verification_uri": "https://github.com/login/device",
          "verification_uri_complete": "https://github.com/login/device?user_code=WDJB-MJHT",
          "expires_at": 1747000900,
          "interval_seconds": 5
        }
        """
        let state = try decoder.decode(GitHubAuthState.self, from: Data(wire.utf8))
        guard case let .pendingUserAuth(userCode, verificationURI, complete, expiresAt, interval) = state else {
            return XCTFail("expected pendingUserAuth, got \(state)")
        }
        XCTAssertEqual(userCode, "WDJB-MJHT")
        XCTAssertEqual(verificationURI, "https://github.com/login/device")
        XCTAssertEqual(complete, "https://github.com/login/device?user_code=WDJB-MJHT")
        XCTAssertEqual(expiresAt, 1747000900)
        XCTAssertEqual(interval, 5)
    }

    func testDecodePendingUserAuthWithoutComplete() throws {
        let wire = """
        {
          "type": "pending_user_auth",
          "user_code": "WDJB-MJHT",
          "verification_uri": "https://github.com/login/device",
          "expires_at": 1747000900,
          "interval_seconds": 5
        }
        """
        let state = try decoder.decode(GitHubAuthState.self, from: Data(wire.utf8))
        guard case let .pendingUserAuth(_, _, complete, _, _) = state else {
            return XCTFail("expected pendingUserAuth, got \(state)")
        }
        XCTAssertNil(complete, "absent verification_uri_complete must decode as nil")
    }

    func testDecodeAuthorizedWithOrgOk() throws {
        let wire = """
        {
          "type": "authorized",
          "login": "octocat",
          "granted_scopes": ["repo", "project"],
          "org_state": {"type": "ok"}
        }
        """
        let state = try decoder.decode(GitHubAuthState.self, from: Data(wire.utf8))
        guard case let .authorized(login, scopes, orgState) = state else {
            return XCTFail("expected authorized, got \(state)")
        }
        XCTAssertEqual(login, "octocat")
        XCTAssertEqual(scopes, ["repo", "project"])
        XCTAssertEqual(orgState, .ok)
    }

    func testDecodeAuthorizedWithNeedsOrgApproval() throws {
        let wire = """
        {
          "type": "authorized",
          "login": "octocat",
          "granted_scopes": ["repo", "project"],
          "org_state": {"type": "needs_org_approval", "request_url": "https://github.com/orgs/spinyfin/policies/applications"}
        }
        """
        let state = try decoder.decode(GitHubAuthState.self, from: Data(wire.utf8))
        guard case let .authorized(_, _, orgState) = state else {
            return XCTFail("expected authorized, got \(state)")
        }
        XCTAssertEqual(orgState, .needsOrgApproval(requestURL: "https://github.com/orgs/spinyfin/policies/applications"))
    }

    func testDecodeExpiredDeniedError() throws {
        XCTAssertEqual(
            try decoder.decode(GitHubAuthState.self, from: Data(#"{"type":"expired"}"#.utf8)),
            .expired
        )
        XCTAssertEqual(
            try decoder.decode(GitHubAuthState.self, from: Data(#"{"type":"denied"}"#.utf8)),
            .denied
        )
        let err = try decoder.decode(
            GitHubAuthState.self,
            from: Data(#"{"type":"error","message":"network unreachable"}"#.utf8)
        )
        XCTAssertEqual(err, .error(message: "network unreachable"))
    }

    func testUnknownStateTypeThrows() {
        XCTAssertThrowsError(
            try decoder.decode(GitHubAuthState.self, from: Data(#"{"type":"who_knows"}"#.utf8))
        )
    }

    // MARK: OrgAuthState wire decode

    func testDecodeOrgAuthStateVariants() throws {
        XCTAssertEqual(
            try decoder.decode(OrgAuthState.self, from: Data(#"{"type":"ok"}"#.utf8)),
            .ok
        )
        XCTAssertEqual(
            try decoder.decode(OrgAuthState.self, from: Data(#"{"type":"unknown"}"#.utf8)),
            .unknown
        )
        XCTAssertEqual(
            try decoder.decode(
                OrgAuthState.self,
                from: Data(#"{"type":"needs_sso","sso_url":"https://github.com/orgs/spinyfin/sso"}"#.utf8)
            ),
            .needsSso(ssoURL: "https://github.com/orgs/spinyfin/sso")
        )
    }

    // MARK: Round-trips (Swift encode → Swift decode)

    func testRoundtripPendingWithComplete() throws {
        let value = GitHubAuthState.pendingUserAuth(
            userCode: "ABCD-1234",
            verificationURI: "https://github.com/login/device",
            verificationURIComplete: "https://github.com/login/device?user_code=ABCD-1234",
            expiresAt: 1747001000,
            intervalSeconds: 7
        )
        XCTAssertEqual(try roundtrip(value), value)
    }

    func testRoundtripPendingWithoutCompleteOmitsKey() throws {
        let value = GitHubAuthState.pendingUserAuth(
            userCode: "ABCD-1234",
            verificationURI: "https://github.com/login/device",
            verificationURIComplete: nil,
            expiresAt: 1747001000,
            intervalSeconds: 7
        )
        let json = String(data: try encoder.encode(value), encoding: .utf8) ?? ""
        XCTAssertFalse(json.contains("verification_uri_complete"), "nil complete must be omitted, got: \(json)")
        XCTAssertTrue(json.contains("\"type\":\"pending_user_auth\""))
        XCTAssertEqual(try roundtrip(value), value)
    }

    func testRoundtripAuthorizedAllOrgStates() throws {
        let orgStates: [OrgAuthState] = [
            .ok,
            .needsOrgApproval(requestURL: "https://example.com/approve"),
            .needsSso(ssoURL: "https://example.com/sso"),
            .unknown,
        ]
        for orgState in orgStates {
            let value = GitHubAuthState.authorized(
                login: "octocat",
                grantedScopes: ["repo", "project"],
                orgState: orgState
            )
            XCTAssertEqual(try roundtrip(value), value, "round-trip failed for org state \(orgState)")
        }
    }
}

/// Pure rendering tests for `GitHubAuthPresentation` — the state→display
/// mapping behind the "GitHub account" settings subsection. No SwiftUI host
/// required (mirrors `ExternalTrackerAttentionTests`).
final class GitHubAuthPresentationTests: XCTestCase {

    func testDisconnectedOffersConnectAndMentionsGhFallback() {
        let p = GitHubAuthPresentation.forState(.disconnected)
        XCTAssertEqual(p.actions, [.connect])
        XCTAssertFalse(p.isBusy)
        XCTAssertFalse(p.connectIsRestart)
        XCTAssertTrue(p.banners.isEmpty)
        XCTAssertNil(p.pendingPrompt)
        XCTAssertTrue(
            p.statusLine.lowercased().contains("gh login"),
            "disconnected status should mention the gh fallback; got: \(p.statusLine)"
        )
    }

    func testRequestingCodeIsBusyAndCancelable() {
        let p = GitHubAuthPresentation.forState(.requestingCode)
        XCTAssertTrue(p.isBusy)
        XCTAssertEqual(p.actions, [.cancel])
        XCTAssertNil(p.pendingPrompt)
    }

    func testPendingPromptUsesCompleteURLWhenPresent() {
        let p = GitHubAuthPresentation.forState(.pendingUserAuth(
            userCode: "WDJB-MJHT",
            verificationURI: "https://github.com/login/device",
            verificationURIComplete: "https://github.com/login/device?user_code=WDJB-MJHT",
            expiresAt: 1747000900,
            intervalSeconds: 5
        ))
        XCTAssertTrue(p.isBusy)
        XCTAssertEqual(p.actions, [.cancel])
        XCTAssertEqual(p.pendingPrompt?.userCode, "WDJB-MJHT")
        XCTAssertEqual(p.pendingPrompt?.verificationURL, "https://github.com/login/device")
        XCTAssertEqual(
            p.pendingPrompt?.openURL,
            "https://github.com/login/device?user_code=WDJB-MJHT",
            "open URL should prefer verification_uri_complete"
        )
    }

    func testPendingPromptFallsBackToBareVerificationURI() {
        let p = GitHubAuthPresentation.forState(.pendingUserAuth(
            userCode: "WDJB-MJHT",
            verificationURI: "https://github.com/login/device",
            verificationURIComplete: nil,
            expiresAt: 1747000900,
            intervalSeconds: 5
        ))
        XCTAssertEqual(p.pendingPrompt?.openURL, "https://github.com/login/device")
    }

    func testAuthorizedOkStatusLineAndActions() {
        let p = GitHubAuthPresentation.forState(.authorized(
            login: "octocat",
            grantedScopes: ["repo", "project"],
            orgState: .ok
        ))
        XCTAssertEqual(p.statusLine, "Connected as @octocat · scopes: repo, project")
        XCTAssertEqual(p.actions, [.reauthorize, .disconnect])
        XCTAssertTrue(p.banners.isEmpty)
        XCTAssertFalse(p.isBusy)
    }

    func testAuthorizedNeedsOrgApprovalBanner() {
        let p = GitHubAuthPresentation.forState(.authorized(
            login: "octocat",
            grantedScopes: ["repo", "project"],
            orgState: .needsOrgApproval(requestURL: "https://github.com/orgs/spinyfin/policies/applications")
        ))
        let banner = p.banners.first { $0.kind == .needsOrgApproval }
        XCTAssertNotNil(banner)
        XCTAssertEqual(banner?.actionURL, "https://github.com/orgs/spinyfin/policies/applications")
        XCTAssertEqual(banner?.offersRecheck, true)
        // Still connected — the row keeps re-authorize / disconnect.
        XCTAssertEqual(p.actions, [.reauthorize, .disconnect])
    }

    func testAuthorizedNeedsSsoBanner() {
        let p = GitHubAuthPresentation.forState(.authorized(
            login: "octocat",
            grantedScopes: ["repo", "project"],
            orgState: .needsSso(ssoURL: "https://github.com/orgs/spinyfin/sso?authorization_request=abc")
        ))
        let banner = p.banners.first { $0.kind == .needsSso }
        XCTAssertNotNil(banner)
        XCTAssertEqual(banner?.actionURL, "https://github.com/orgs/spinyfin/sso?authorization_request=abc")
        XCTAssertEqual(banner?.offersRecheck, true)
    }

    func testAuthorizedUnknownOrgBannerOffersRecheck() {
        let p = GitHubAuthPresentation.forState(.authorized(
            login: "octocat",
            grantedScopes: ["repo", "project"],
            orgState: .unknown
        ))
        let banner = p.banners.first { $0.kind == .unknownOrg }
        XCTAssertNotNil(banner)
        XCTAssertNil(banner?.actionURL)
        XCTAssertEqual(banner?.offersRecheck, true)
    }

    func testAuthorizedLimitedScopesBanner() {
        let p = GitHubAuthPresentation.forState(.authorized(
            login: "octocat",
            grantedScopes: ["repo"],
            orgState: .ok
        ))
        XCTAssertEqual(p.statusLine, "Connected as @octocat · scopes: repo")
        XCTAssertNotNil(
            p.banners.first { $0.kind == .limitedScopes },
            "missing the project scope should raise a limited-scopes banner"
        )
    }

    func testAuthorizedFullScopesHasNoLimitedBanner() {
        let p = GitHubAuthPresentation.forState(.authorized(
            login: "octocat",
            grantedScopes: ["project", "repo"],
            orgState: .ok
        ))
        XCTAssertNil(p.banners.first { $0.kind == .limitedScopes })
    }

    func testEmptyScopesRenderAsNone() {
        let p = GitHubAuthPresentation.forState(.authorized(
            login: "octocat",
            grantedScopes: [],
            orgState: .ok
        ))
        XCTAssertEqual(p.statusLine, "Connected as @octocat · scopes: none")
        XCTAssertNotNil(p.banners.first { $0.kind == .limitedScopes })
    }

    func testExpiredIsRestartWithBanner() {
        let p = GitHubAuthPresentation.forState(.expired)
        XCTAssertTrue(p.connectIsRestart)
        XCTAssertEqual(p.actions, [.connect])
        XCTAssertEqual(p.banners.first?.kind, .expired)
    }

    func testDeniedBanner() {
        let p = GitHubAuthPresentation.forState(.denied)
        XCTAssertTrue(p.connectIsRestart)
        XCTAssertEqual(p.actions, [.connect])
        XCTAssertEqual(p.banners.first?.kind, .denied)
    }

    func testErrorBannerCarriesMessage() {
        let p = GitHubAuthPresentation.forState(.error(message: "device/code request failed"))
        XCTAssertEqual(p.actions, [.connect])
        XCTAssertTrue(p.connectIsRestart)
        XCTAssertEqual(p.banners.first?.kind, .error)
        XCTAssertEqual(p.banners.first?.message, "device/code request failed")
    }
}
