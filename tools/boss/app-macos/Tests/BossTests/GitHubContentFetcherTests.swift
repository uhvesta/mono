import XCTest
@testable import Boss

/// Covers the URL-parsing and endpoint-construction helpers on
/// [[GitHubContentFetcher]]. The actual `gh` subprocess invocation
/// is not exercised here — the affordance tests inject a stub via
/// `ChatViewModel.rawContentFetcher` so neither suite ever shells
/// out during `bazel test`.
final class GitHubContentFetcherTests: XCTestCase {
    // MARK: - parseRawContentURL

    func testParsesStandardRawContentURL() {
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

    func testParsesNestedPath() {
        let url = URL(string: "https://raw.githubusercontent.com/spinyfin/mono/main/tools/boss/docs/designs/foo.md")!
        let parsed = GitHubContentFetcher.parseRawContentURL(url)
        XCTAssertEqual(parsed?.owner, "spinyfin")
        XCTAssertEqual(parsed?.repo, "mono")
        XCTAssertEqual(parsed?.ref, "main")
        XCTAssertEqual(parsed?.path, "tools/boss/docs/designs/foo.md")
    }

    /// Refs that look like branch names with no `/` round-trip cleanly
    /// — these are the common case for merged docs (`main`) and for
    /// tag/SHA refs.
    func testParsesSimpleRefName() {
        let url = URL(string: "https://raw.githubusercontent.com/foo/bar/release-1.2/docs/x.md")!
        XCTAssertEqual(GitHubContentFetcher.parseRawContentURL(url)?.ref, "release-1.2")
    }

    /// Wrong host returns `nil` so the caller can throw
    /// `unsupportedHost` instead of constructing a garbage endpoint.
    func testReturnsNilForNonRawHost() {
        let url = URL(string: "https://github.com/foo/bar/blob/main/docs/x.md")!
        XCTAssertNil(GitHubContentFetcher.parseRawContentURL(url))
    }

    func testReturnsNilWhenPathHasTooFewSegments() {
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
