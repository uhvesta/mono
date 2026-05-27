import XCTest
@testable import Boss

/// Covers the URL-parsing and endpoint-construction helpers on
/// [[GitHubContentFetcher]]. The actual `gh` subprocess invocation
/// is not exercised here — the affordance tests inject a stub via
/// `ChatViewModel.rawContentFetcher` so neither suite ever shells
/// out during `bazel test`.
final class GitHubContentFetcherTests: XCTestCase {
    // MARK: - parseRawContentURL (new format — ref in ?ref= query param)

    /// Primary format emitted by the engine (≥ #805 fix): branch in `?ref=`,
    /// file path in URL path starting at segment 2. Supports slashed branch
    /// names because `URLComponents.queryItems` decodes `%2F` back to `/`.
    func testParsesNewFormatMainBranch() {
        let url = URL(string: "https://raw.githubusercontent.com/spinyfin/mono/tools/boss/docs/designs/foo.md?ref=main")!
        let parsed = GitHubContentFetcher.parseRawContentURL(url)
        XCTAssertEqual(parsed?.owner, "spinyfin")
        XCTAssertEqual(parsed?.repo, "mono")
        XCTAssertEqual(parsed?.ref, "main")
        XCTAssertEqual(parsed?.path, "tools/boss/docs/designs/foo.md")
    }

    /// Regression test for the core bug: a `boss/exec_*` branch name
    /// contains `/`. The engine percent-encodes it as `%2F` in the query
    /// string; `URLComponents` decodes it back to `/` so the full branch
    /// name is preserved when the endpoint is constructed.
    func testParsesSlashedRefFromQueryParam() {
        let url = URL(string: "https://raw.githubusercontent.com/spinyfin/mono/tools/boss/docs/designs/foo.md?ref=boss%2Fexec_18b07a506d2518d0_1b")!
        let parsed = GitHubContentFetcher.parseRawContentURL(url)
        XCTAssertEqual(parsed?.owner, "spinyfin")
        XCTAssertEqual(parsed?.repo, "mono")
        XCTAssertEqual(parsed?.ref, "boss/exec_18b07a506d2518d0_1b")
        XCTAssertEqual(parsed?.path, "tools/boss/docs/designs/foo.md")
    }

    func testParsesNewFormatNonMainBranch() {
        let url = URL(string: "https://raw.githubusercontent.com/foo/bar/docs/x.md?ref=release-1.2")!
        XCTAssertEqual(GitHubContentFetcher.parseRawContentURL(url)?.ref, "release-1.2")
        XCTAssertEqual(GitHubContentFetcher.parseRawContentURL(url)?.path, "docs/x.md")
    }

    // MARK: - parseRawContentURL (old format — ref in path, backward compat)

    /// Old-format URLs (engine before the #805 fix) have the branch as the
    /// third path segment. These are accepted for backward compatibility.
    func testParsesOldFormatStandardURL() {
        let url = URL(string: "https://raw.githubusercontent.com/foo/bar/main/docs/x.md")!
        let parsed = GitHubContentFetcher.parseRawContentURL(url)
        XCTAssertEqual(
            parsed,
            GitHubContentFetcher.RawContentRef(
                owner: "foo",
                repo: "bar",
                ref: "main",
                path: "docs/x.md"
            )
        )
    }

    // MARK: - parseRawContentURL (error cases)

    /// Wrong host returns `nil` so the caller can throw
    /// `unsupportedHost` instead of constructing a garbage endpoint.
    func testReturnsNilForNonRawHost() {
        let url = URL(string: "https://github.com/foo/bar/blob/main/docs/x.md")!
        XCTAssertNil(GitHubContentFetcher.parseRawContentURL(url))
    }

    func testReturnsNilWhenPathHasTooFewSegments() {
        // Old format needs ≥4 segments; new format needs ≥3. Two segments → nil.
        let url = URL(string: "https://raw.githubusercontent.com/foo/bar")!
        XCTAssertNil(GitHubContentFetcher.parseRawContentURL(url))
    }

    func testReturnsNilWhenOldFormatMissingPath() {
        // Three segments with no ?ref= → old format → needs ≥4 → nil.
        let url = URL(string: "https://raw.githubusercontent.com/foo/bar/main")!
        XCTAssertNil(GitHubContentFetcher.parseRawContentURL(url))
    }

    func testReturnsNilForEmptyOwnerOrRepo() {
        let url = URL(string: "https://raw.githubusercontent.com///main/x.md")!
        XCTAssertNil(GitHubContentFetcher.parseRawContentURL(url))
    }

    // MARK: - contentsAPIEndpoint

    /// The contents-API endpoint keeps `/` between path segments
    /// (the API treats them as a real path) but percent-encodes the
    /// ref value so branch names that contain `/` round-trip through
    /// the query string. Without this, `boss/exec_…` branches lose
    /// their slash and resolve to the wrong (or no) ref.
    func testEndpointKeepsPathSlashesAndEncodesRef() {
        let ref = GitHubContentFetcher.RawContentRef(
            owner: "foo",
            repo: "bar",
            ref: "boss/exec_abc",
            path: "tools/boss/docs/x.md"
        )
        let endpoint = GitHubContentFetcher.contentsAPIEndpoint(for: ref)
        XCTAssertEqual(
            endpoint,
            "/repos/foo/bar/contents/tools/boss/docs/x.md?ref=boss/exec_abc"
        )
    }

    func testEndpointForSimpleMainBranch() {
        let ref = GitHubContentFetcher.RawContentRef(
            owner: "spinyfin",
            repo: "mono",
            ref: "main",
            path: "tools/boss/docs/designs/foo.md"
        )
        XCTAssertEqual(
            GitHubContentFetcher.contentsAPIEndpoint(for: ref),
            "/repos/spinyfin/mono/contents/tools/boss/docs/designs/foo.md?ref=main"
        )
    }

    // MARK: - Full round-trip: new-format URL → parseRawContentURL → contentsAPIEndpoint

    /// End-to-end: the engine emits a URL with `%2F`-encoded branch, the
    /// parser recovers the full branch name, and the endpoint passes it
    /// unencoded to `gh api` — matching what the task's proof confirms works:
    /// `gh api repos/spinyfin/mono/contents/<doc>?ref=boss/exec_18b07a506d2518d0_1b → 200`.
    func testRoundTripSlashedRefNewFormat() {
        let rawURL = URL(string: "https://raw.githubusercontent.com/spinyfin/mono/tools/boss/docs/designs/foo.md?ref=boss%2Fexec_18b07a506d2518d0_1b")!
        guard let parsed = GitHubContentFetcher.parseRawContentURL(rawURL) else {
            XCTFail("parseRawContentURL returned nil for new-format slashed-ref URL")
            return
        }
        let endpoint = GitHubContentFetcher.contentsAPIEndpoint(for: parsed)
        XCTAssertEqual(
            endpoint,
            "/repos/spinyfin/mono/contents/tools/boss/docs/designs/foo.md?ref=boss/exec_18b07a506d2518d0_1b",
            "endpoint must pass the unencoded branch name to gh api"
        )
    }

    // MARK: - fetch dispatch

    /// Non-`raw.githubusercontent.com` URLs throw `unsupportedHost`
    /// rather than silently shelling out — the engine only ever emits
    /// `raw.githubusercontent.com` URLs in `raw_content_url`, so any
    /// other host points at a protocol drift the user should see.
    func testFetchThrowsUnsupportedHostForNonGitHubURL() async {
        let url = URL(string: "https://example.com/foo.md")!
        do {
            _ = try await GitHubContentFetcher.fetch(url)
            XCTFail("expected unsupportedHost error")
        } catch GitHubContentFetcher.FetchError.unsupportedHost {
            // expected
        } catch {
            XCTFail("unexpected error: \(error)")
        }
    }

    func testFetchThrowsMalformedForRawHostWithMissingPath() async {
        let url = URL(string: "https://raw.githubusercontent.com/foo/bar/main")!
        do {
            _ = try await GitHubContentFetcher.fetch(url)
            XCTFail("expected malformedRawURL error")
        } catch GitHubContentFetcher.FetchError.malformedRawURL {
            // expected
        } catch {
            XCTFail("unexpected error: \(error)")
        }
    }
}
