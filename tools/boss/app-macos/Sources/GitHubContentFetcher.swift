import Foundation

/// Fetches raw markdown content from GitHub via the authenticated
/// `gh` CLI, never via unauthenticated `URLSession` requests against
/// `raw.githubusercontent.com`.
///
/// `raw.githubusercontent.com` returns HTTP 404 for any request that
/// can't see a given file — including the case where the requester
/// simply isn't authenticated against a private repo. That means an
/// unauthenticated fetch is indistinguishable from a missing file, so
/// the markdown viewer would silently show "not found" for every
/// LinkedIn-internal docs repo. Issue #732.
///
/// The contents API + `Accept: application/vnd.github.raw` returns
/// the raw file bytes (LFS-aware, large-file aware, branch-as-sha
/// resolved) authenticated as whichever account the user's `gh` is
/// currently logged in as. `gh` already routes per-host across
/// `gh auth switch` accounts, so the Boss app does not need to know
/// which token belongs to which repo.
enum GitHubContentFetcher {
    enum FetchError: Error, LocalizedError {
        /// The URL is not a `raw.githubusercontent.com` URL we can
        /// re-route through `gh`. The caller almost certainly
        /// constructed it from an engine-supplied `raw_content_url`,
        /// so reaching this case points at an engine/protocol drift.
        case unsupportedHost(URL)
        /// The URL was on the right host but didn't have the
        /// `/<owner>/<repo>/<ref>/<path>` shape.
        case malformedRawURL(URL)
        /// `Process.run()` failed before `gh` even started — usually
        /// because `gh` isn't on PATH. Surfacing this lets the UI
        /// tell the user to install/authenticate `gh` rather than
        /// showing a generic "404".
        case ghLaunchFailed(underlying: Error)
        /// `gh` ran but exited non-zero. Carries stderr so the user
        /// sees the actual reason (auth required, repo not found,
        /// etc.) rather than a stock message.
        case ghExitFailure(status: Int32, stderr: String)
        case nonUTF8Response

        var errorDescription: String? {
            switch self {
            case .unsupportedHost(let url):
                return "Unsupported design-doc URL host: \(url.absoluteString)"
            case .malformedRawURL(let url):
                return "Malformed raw-content URL: \(url.absoluteString)"
            case .ghLaunchFailed(let err):
                return "Failed to launch gh: \(err.localizedDescription)"
            case .ghExitFailure(_, let stderr):
                let trimmed = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
                return trimmed.isEmpty ? "gh api failed" : "gh api failed: \(trimmed)"
            case .nonUTF8Response:
                return "gh returned non-UTF8 content"
            }
        }
    }

    /// Components extracted from a `raw.githubusercontent.com` URL of
    /// the shape `https://raw.githubusercontent.com/<owner>/<repo>/<ref>/<path...>`.
    /// `ref` may be a branch (`main`), a tag, or a SHA; `path` can
    /// itself contain `/` separators (e.g. `tools/boss/docs/x.md`).
    struct RawContentRef: Equatable {
        var owner: String
        var repo: String
        var ref: String
        var path: String
    }

    /// Parse a GitHub raw-content URL. Returns `nil` for any URL that
    /// is not on the `raw.githubusercontent.com` host so callers can
    /// distinguish "wrong host" from "wrong shape".
    static func parseRawContentURL(_ url: URL) -> RawContentRef? {
        guard url.host?.lowercased() == "raw.githubusercontent.com" else {
            return nil
        }
        let segments = url.pathComponents.filter { $0 != "/" }
        guard segments.count >= 4 else { return nil }
        let owner = segments[0]
        let repo = segments[1]
        let ref = segments[2]
        let path = segments[3...].joined(separator: "/")
        guard !owner.isEmpty, !repo.isEmpty, !ref.isEmpty, !path.isEmpty else {
            return nil
        }
        return RawContentRef(owner: owner, repo: repo, ref: ref, path: path)
    }

    /// Build the `gh api` endpoint for a parsed raw-content ref. The
    /// path between `/contents/` and the `?ref=` query keeps its `/`
    /// separators because the contents API treats them as path
    /// segments; only the ref value is percent-encoded so branch
    /// names with `/` (e.g. `boss/exec_…`) round-trip through the
    /// query string.
    static func contentsAPIEndpoint(for ref: RawContentRef) -> String {
        let encodedRef = ref.ref
            .addingPercentEncoding(withAllowedCharacters: .urlPathAllowed)
            ?? ref.ref
        return "/repos/\(ref.owner)/\(ref.repo)/contents/\(ref.path)?ref=\(encodedRef)"
    }

    /// Fetch the raw bytes of `url` via `gh api`. The URL must be a
    /// `raw.githubusercontent.com` URL — that's what the engine
    /// produces in `raw_content_url` for any GitHub-hosted design
    /// doc. Throws on any failure so the markdown viewer surfaces
    /// the error rather than rendering a stub `404: Not Found` body.
    static func fetch(_ url: URL) async throws -> String {
        guard let host = url.host?.lowercased() else {
            throw FetchError.unsupportedHost(url)
        }
        guard host == "raw.githubusercontent.com" else {
            throw FetchError.unsupportedHost(url)
        }
        guard let ref = parseRawContentURL(url) else {
            throw FetchError.malformedRawURL(url)
        }
        return try await runGH(arguments: [
            "api",
            "-H", "Accept: application/vnd.github.raw",
            contentsAPIEndpoint(for: ref),
        ])
    }

    private static func runGH(arguments: [String]) async throws -> String {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<String, Error>) in
            DispatchQueue.global(qos: .userInitiated).async {
                let proc = Process()
                proc.executableURL = URL(fileURLWithPath: "/usr/bin/env")
                proc.arguments = ["gh"] + arguments
                var env = ProcessInfo.processInfo.environment
                env["PATH"] = augmentedPATH(current: env["PATH"] ?? "/usr/bin:/bin")
                proc.environment = env
                let stdoutPipe = Pipe()
                let stderrPipe = Pipe()
                proc.standardOutput = stdoutPipe
                proc.standardError = stderrPipe
                do {
                    try proc.run()
                } catch {
                    continuation.resume(throwing: FetchError.ghLaunchFailed(underlying: error))
                    return
                }
                let stdoutData = stdoutPipe.fileHandleForReading.readDataToEndOfFile()
                let stderrData = stderrPipe.fileHandleForReading.readDataToEndOfFile()
                proc.waitUntilExit()
                if proc.terminationStatus != 0 {
                    let stderrText = String(data: stderrData, encoding: .utf8) ?? "(non-utf8 stderr)"
                    continuation.resume(throwing: FetchError.ghExitFailure(
                        status: proc.terminationStatus,
                        stderr: stderrText
                    ))
                    return
                }
                guard let text = String(data: stdoutData, encoding: .utf8) else {
                    continuation.resume(throwing: FetchError.nonUTF8Response)
                    return
                }
                continuation.resume(returning: text)
            }
        }
    }

    /// macOS GUI apps launched from Finder/Dock/launchctl inherit a
    /// minimal launchd PATH that omits the directories where `gh`
    /// typically lives. Mirror the augmentation that
    /// `EngineProcessController` already applies for the engine
    /// subprocess so this fetcher resolves `gh` consistently.
    private static func augmentedPATH(current: String) -> String {
        let home = ProcessInfo.processInfo.environment["HOME"] ?? NSHomeDirectory()
        let extra = [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/local/linkedin/bin",
            "\(home)/.cargo/bin",
            "\(home)/bin",
            "\(home)/.local/bin",
        ]
        var seen = Set(current.split(separator: ":").map(String.init))
        let unique = extra.filter { seen.insert($0).inserted }
        let prefix = unique.joined(separator: ":")
        return prefix.isEmpty ? current : "\(prefix):\(current)"
    }
}
