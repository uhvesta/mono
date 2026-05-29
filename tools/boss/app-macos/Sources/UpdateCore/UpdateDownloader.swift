import CryptoKit
import Foundation

// MARK: - Manifest

/// On-disk record of a staged update's lifecycle, written to
/// `Updates/<version>/manifest.json` (or, mid-flight, `staging/<version>/`).
///
/// `state` is the source of truth on next launch: any directory whose manifest
/// is not `.ready` is garbage-collected by `UpdateDownloader.cleanup()`. The
/// atomic-rename rule (a complete, verified version is renamed into place only
/// once its manifest says `.ready`) guarantees a crash mid-download never leaves
/// a partial directory masquerading as a ready version.
///
/// Mirrors the field list in design doc §3:
/// `{ version, tag, etag, sha256, sourceURL, verifiedAt, state }`.
struct UpdateManifest: Codable, Equatable, Sendable {
    enum State: String, Codable, Sendable {
        /// The asset is being fetched into `staging/<version>/`.
        case downloading
        /// The asset landed; size/unzip/codesign/Team-ID checks are running.
        case verifying
        /// All checks passed, quarantine stripped, promoted to `Updates/<version>/`.
        case ready
        /// A check failed; the directory is slated for cleanup.
        case failed
    }

    /// `"1.0.28"` — the `major.minor.patch` form, matching the directory name.
    let version: String
    /// `"boss-v1.0.28"` — the GitHub release tag.
    let tag: String
    /// The asset `browser_download_url` the bundle was fetched from.
    let sourceURL: String
    /// The release-list ETag in effect when this version was staged. Bookkeeping
    /// only in v1 (the live conditional-request cache lives in `UpdateChecker`).
    var etag: String?
    /// SHA-256 of the downloaded zip, lowercase hex. Recorded for diagnostics and
    /// future checksum-asset verification; there is no published checksum today.
    var sha256: String?
    /// ISO-8601 timestamp set when `state` transitions to `.ready`.
    var verifiedAt: String?
    /// Current lifecycle state.
    var state: State
    /// Human-readable reason, populated only when `state == .failed`.
    var failureReason: String?
}

// MARK: - Result types

/// A fully verified, quarantine-stripped bundle staged under `Updates/<version>/`
/// and ready for `UpdateInstaller` (T7) to swap in.
struct StagedUpdate: Equatable, Sendable {
    let version: VersionTuple
    let tag: String
    /// `Updates/<version>/Boss.app`.
    let bundleURL: URL
    /// `Updates/<version>/manifest.json`.
    let manifestURL: URL
}

enum DownloadOutcome: Equatable, Sendable {
    /// A fresh download completed verification and was promoted to `Updates/<version>/`.
    case ready(StagedUpdate)
    /// A previously staged, still-`ready` bundle for this version already existed; no
    /// network fetch was performed.
    case alreadyStaged(StagedUpdate)
    /// The download or one of the integrity checks failed. The staging work directory
    /// is left marked `.failed` for the next `cleanup()` to reap; no swap is possible.
    case failed(reason: String)
}

// MARK: - Injectable asset downloader

/// Downloads a remote asset to a local file. Injectable so tests run without a
/// network. The `.live` implementation uses `URLSession`.
struct AssetDownloader: Sendable {
    /// Download `url`, reporting fractional progress (`0...1`). Returns the local
    /// file URL of the completed download. The returned file is owned by the
    /// caller (`UpdateDownloader` moves it into the staging directory).
    let download: @Sendable (_ url: URL, _ onProgress: @Sendable @escaping (Double) -> Void) async throws -> URL

    /// Foreground `URLSession` download to a temp file.
    ///
    /// The async `download(from:)` writes to a URLSession-managed temporary
    /// location that may be reclaimed once this closure returns, so we move it to
    /// a temp file we own before handing it back. A background-configured session
    /// (survives app suspension, resumable) can be slotted in here later without
    /// touching `UpdateDownloader` — that lifecycle concern belongs to the app
    /// layer (T2), not this leaf module.
    static let live = AssetDownloader { url, onProgress in
        let (tempURL, _) = try await URLSession.shared.download(from: url)
        let stable = FileManager.default.temporaryDirectory
            .appendingPathComponent("boss-update-\(UUID().uuidString).zip")
        try? FileManager.default.removeItem(at: stable)
        try FileManager.default.moveItem(at: tempURL, to: stable)
        onProgress(1.0)
        return stable
    }
}

// MARK: - Injectable bundle operations

/// Error from one of the external verification/preparation tools.
struct BundleOperationError: Error, CustomStringConvertible, Equatable {
    let tool: String
    let status: Int32
    let message: String
    var description: String {
        let trimmed = message.trimmingCharacters(in: .whitespacesAndNewlines)
        return "\(tool) failed (status \(status))\(trimmed.isEmpty ? "" : ": \(trimmed)")"
    }
}

/// The external tool operations needed to verify and prepare a staged bundle.
/// Injectable so tests never shell out; the `.live` implementation runs `ditto`,
/// `codesign`, and `xattr` via `Process`.
struct BundleOperations: Sendable {
    /// Extract a zip archive into `destinationDir` using `ditto -x -k` — the same
    /// tool the release pipeline uses to build the zip. Throws on any error.
    let extract: @Sendable (_ zip: URL, _ destinationDir: URL) async throws -> Void
    /// `codesign --verify --deep --strict` the bundle. Throws if the signature is
    /// invalid or absent.
    let verifyCodeSignature: @Sendable (_ bundle: URL) async throws -> Void
    /// Read the signing Team Identifier (`codesign -d -vv`). Returns `nil` for an
    /// ad-hoc-signed or unsigned bundle (no team), which is the current reality for
    /// `bazel build`-produced releases.
    let readTeamID: @Sendable (_ bundle: URL) async throws -> String?
    /// `xattr -dr com.apple.quarantine` on the bundle. This is the final step
    /// before a bundle is marked `.ready`; it is what lets an un-notarized release
    /// launch (Gatekeeper only assesses notarization when the quarantine xattr is
    /// present) and prevents App Translocation.
    let stripQuarantine: @Sendable (_ bundle: URL) async throws -> Void

    static let live = BundleOperations(
        extract: { zip, destinationDir in
            try await ProcessTool.run(
                "/usr/bin/ditto",
                ["-x", "-k", zip.path, destinationDir.path]
            )
        },
        verifyCodeSignature: { bundle in
            try await ProcessTool.run(
                "/usr/bin/codesign",
                ["--verify", "--deep", "--strict", bundle.path]
            )
        },
        readTeamID: { bundle in
            // `codesign -d -vv` writes its report to stderr. An unsigned bundle
            // exits non-zero ("code object is not signed at all") → no team.
            let result = try? await ProcessTool.capture(
                "/usr/bin/codesign",
                ["-d", "-vv", bundle.path]
            )
            guard let result, result.status == 0 else { return nil }
            return ProcessTool.parseTeamIdentifier(from: result.stdout + result.stderr)
        },
        stripQuarantine: { bundle in
            try await ProcessTool.run(
                "/usr/bin/xattr",
                ["-dr", "com.apple.quarantine", bundle.path]
            )
        }
    )
}

/// Thin `Process` runner shared by the `.live` `BundleOperations`. Mirrors the
/// continuation-on-a-background-queue shape of `GitHubContentFetcher.runGH`.
enum ProcessTool {
    struct Output: Sendable {
        let status: Int32
        let stdout: String
        let stderr: String
    }

    /// Run a tool and return its captured output regardless of exit status.
    static func capture(_ path: String, _ arguments: [String]) async throws -> Output {
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Output, Error>) in
            DispatchQueue.global(qos: .userInitiated).async {
                let proc = Process()
                proc.executableURL = URL(fileURLWithPath: path)
                proc.arguments = arguments
                let outPipe = Pipe()
                let errPipe = Pipe()
                proc.standardOutput = outPipe
                proc.standardError = errPipe
                do {
                    try proc.run()
                } catch {
                    continuation.resume(throwing: error)
                    return
                }
                let outData = outPipe.fileHandleForReading.readDataToEndOfFile()
                let errData = errPipe.fileHandleForReading.readDataToEndOfFile()
                proc.waitUntilExit()
                continuation.resume(returning: Output(
                    status: proc.terminationStatus,
                    stdout: String(data: outData, encoding: .utf8) ?? "",
                    stderr: String(data: errData, encoding: .utf8) ?? ""
                ))
            }
        }
    }

    /// Run a tool and throw `BundleOperationError` on a non-zero exit.
    static func run(_ path: String, _ arguments: [String]) async throws {
        let output = try await capture(path, arguments)
        guard output.status == 0 else {
            throw BundleOperationError(
                tool: URL(fileURLWithPath: path).lastPathComponent,
                status: output.status,
                message: output.stderr.isEmpty ? output.stdout : output.stderr
            )
        }
    }

    /// Extract `TeamIdentifier=XXXX` from `codesign -d -vv` output. Returns `nil`
    /// for `not set` (ad-hoc) or when no such line is present.
    static func parseTeamIdentifier(from text: String) -> String? {
        for line in text.split(whereSeparator: \.isNewline) {
            guard line.hasPrefix("TeamIdentifier=") else { continue }
            let value = line.dropFirst("TeamIdentifier=".count)
                .trimmingCharacters(in: .whitespaces)
            return (value.isEmpty || value == "not set") ? nil : value
        }
        return nil
    }
}

// MARK: - UpdateDownloader actor

/// Downloads, verifies, stages, and prunes Boss release bundles per design doc §3.
///
/// Pipeline (each step must pass before the next; any failure marks the staging
/// directory `.failed` and returns `.failed` without ever promoting a bundle):
///
/// 1. Download the asset into `staging/<version>/` (background-capable session).
/// 2. **Size** — bytes received == the asset `size` from the API.
/// 3. **Unzip** — `ditto -x -k`.
/// 4. **Code signature** — `codesign --verify --deep --strict`, and the staged
///    bundle's Team ID must equal the *running* bundle's Team ID (so a swap can
///    never move us to a differently-signed bundle; equal `nil`s — today's
///    ad-hoc-signed reality — match).
/// 5. **Quarantine strip** — `xattr -dr com.apple.quarantine`, the final step
///    before `.ready`.
///
/// Only then is the manifest marked `.ready` and the whole directory atomically
/// `rename(2)`- d into `Updates/<version>/`. A successful stage triggers cleanup
/// (delete every other staged version ≤ the newest ready one; sweep non-`ready`
/// leftovers). `spctl --assess` is intentionally omitted in v1 — it would fail on
/// un-notarized releases and is unnecessary given the quarantine-strip guarantee.
actor UpdateDownloader {
    private let updatesDirectory: URL
    private let currentVersion: VersionTuple
    private let runningTeamID: String?
    private let assetDownloader: AssetDownloader
    private let bundleOps: BundleOperations

    /// `Updates/staging/` — in-progress work; never holds a complete version.
    var stagingDirectory: URL {
        updatesDirectory.appendingPathComponent("staging")
    }

    init(
        updatesDirectory: URL,
        currentVersion: VersionTuple,
        runningTeamID: String?,
        assetDownloader: AssetDownloader,
        bundleOps: BundleOperations
    ) {
        self.updatesDirectory = updatesDirectory
        self.currentVersion = currentVersion
        self.runningTeamID = runningTeamID
        self.assetDownloader = assetDownloader
        self.bundleOps = bundleOps
    }

    /// The canonical staging root: `~/Library/Application Support/Boss/Updates`.
    static func defaultUpdatesDirectory() -> URL {
        let home = ProcessInfo.processInfo.environment["HOME"] ?? NSHomeDirectory()
        return URL(fileURLWithPath: home)
            .appendingPathComponent("Library/Application Support/Boss/Updates", isDirectory: true)
    }

    /// Live downloader rooted at the default Updates directory.
    static func live(currentVersion: VersionTuple, runningTeamID: String?) -> UpdateDownloader {
        UpdateDownloader(
            updatesDirectory: defaultUpdatesDirectory(),
            currentVersion: currentVersion,
            runningTeamID: runningTeamID,
            assetDownloader: .live,
            bundleOps: .live
        )
    }

    // MARK: Download + verify + stage

    /// Download and stage `update`. Returns `.alreadyStaged` without fetching if a
    /// verified bundle for this version is already present, `.ready` on success, or
    /// `.failed` (with a reason) if any step fails.
    func download(
        _ update: AvailableUpdate,
        etag: String? = nil,
        onProgress: @Sendable @escaping (Double) -> Void = { _ in }
    ) async -> DownloadOutcome {
        let version = update.version
        let versionStr = version.description
        let finalDir = updatesDirectory.appendingPathComponent(versionStr, isDirectory: true)

        // Short-circuit if a verified bundle for this version is already staged.
        if let existing = readyStagedUpdate(at: finalDir, expected: version) {
            return .alreadyStaged(existing)
        }

        let workDir = stagingDirectory.appendingPathComponent(versionStr, isDirectory: true)
        do {
            try ensureDirectory(updatesDirectory)
            try resetDirectory(workDir)
        } catch {
            return .failed(reason: "could not prepare staging directory: \(error.localizedDescription)")
        }

        var manifest = UpdateManifest(
            version: versionStr,
            tag: update.tagName,
            sourceURL: update.assetURL.absoluteString,
            etag: etag,
            sha256: nil,
            verifiedAt: nil,
            state: .downloading,
            failureReason: nil
        )
        writeManifest(manifest, to: workDir)

        // 1. Download.
        let downloadedURL: URL
        do {
            downloadedURL = try await assetDownloader.download(update.assetURL, onProgress)
        } catch {
            return fail(workDir, &manifest, "download failed: \(error.localizedDescription)")
        }

        let zipURL = workDir.appendingPathComponent("Boss-\(versionStr).zip")
        do {
            try? FileManager.default.removeItem(at: zipURL)
            try FileManager.default.moveItem(at: downloadedURL, to: zipURL)
        } catch {
            return fail(workDir, &manifest, "could not move download into staging: \(error.localizedDescription)")
        }

        // 2. Size.
        guard let actualSize = fileSize(of: zipURL) else {
            return fail(workDir, &manifest, "could not stat downloaded asset")
        }
        guard actualSize == update.assetSize else {
            return fail(workDir, &manifest, "size mismatch: expected \(update.assetSize) bytes, got \(actualSize)")
        }

        // Record the digest (bookkeeping / future checksum-asset verification).
        manifest.sha256 = sha256Hex(of: zipURL)
        manifest.state = .verifying
        writeManifest(manifest, to: workDir)

        // 3. Unzip.
        let extractDir = workDir.appendingPathComponent("extract", isDirectory: true)
        do {
            try resetDirectory(extractDir)
            try await bundleOps.extract(zipURL, extractDir)
        } catch {
            return fail(workDir, &manifest, "extraction failed: \(error.localizedDescription)")
        }

        guard let extractedBundle = locateBundle(in: extractDir) else {
            return fail(workDir, &manifest, "no Boss.app found in archive")
        }

        // 4. Code signature + Team-ID match.
        do {
            try await bundleOps.verifyCodeSignature(extractedBundle)
        } catch {
            return fail(workDir, &manifest, "code signature verification failed: \(error.localizedDescription)")
        }

        let stagedTeamID: String?
        do {
            stagedTeamID = try await bundleOps.readTeamID(extractedBundle)
        } catch {
            return fail(workDir, &manifest, "could not read staged bundle Team ID: \(error.localizedDescription)")
        }
        guard stagedTeamID == runningTeamID else {
            return fail(
                workDir, &manifest,
                "Team ID mismatch: staged \(stagedTeamID ?? "none"), running \(runningTeamID ?? "none")"
            )
        }

        // Move the verified bundle to its canonical slot, drop the scratch extract dir.
        let bundleURL = workDir.appendingPathComponent("Boss.app", isDirectory: true)
        do {
            try? FileManager.default.removeItem(at: bundleURL)
            try FileManager.default.moveItem(at: extractedBundle, to: bundleURL)
            try? FileManager.default.removeItem(at: extractDir)
        } catch {
            return fail(workDir, &manifest, "could not place verified bundle: \(error.localizedDescription)")
        }

        // 5. Quarantine strip — the last step before `.ready`.
        do {
            try await bundleOps.stripQuarantine(bundleURL)
        } catch {
            return fail(workDir, &manifest, "quarantine strip failed: \(error.localizedDescription)")
        }

        // Mark ready, then atomically promote.
        manifest.state = .ready
        manifest.verifiedAt = ISO8601DateFormatter().string(from: Date())
        manifest.failureReason = nil
        do {
            try writeManifestThrowing(manifest, to: workDir)
        } catch {
            return fail(workDir, &manifest, "could not write ready manifest: \(error.localizedDescription)")
        }

        do {
            // rename(2) is atomic within the shared Updates/ filesystem; clear any
            // stale same-version directory (e.g. a prior failed attempt) first.
            try? FileManager.default.removeItem(at: finalDir)
            try FileManager.default.moveItem(at: workDir, to: finalDir)
        } catch {
            return .failed(reason: "atomic promotion to \(finalDir.path) failed: \(error.localizedDescription)")
        }

        cleanup()

        return .ready(StagedUpdate(
            version: version,
            tag: update.tagName,
            bundleURL: finalDir.appendingPathComponent("Boss.app", isDirectory: true),
            manifestURL: finalDir.appendingPathComponent("manifest.json")
        ))
    }

    // MARK: Cleanup

    /// Prune the Updates directory:
    /// - delete the entire staging working area (in-progress temp);
    /// - delete any `Updates/<version>/` whose manifest is not `.ready` (interrupted
    ///   or failed leftovers);
    /// - delete any `ready` version `<= currentVersion` (superseded by the running app);
    /// - among remaining `ready` versions, keep only the newest and delete the rest.
    ///
    /// Returns the versions deleted (for tests / logging). Safe to call at launch.
    @discardableResult
    func cleanup() -> [VersionTuple] {
        var deleted: [VersionTuple] = []

        // Staging holds only in-progress temp; the actor serialises access, so when
        // cleanup runs nothing is mid-verify and the whole area can be cleared.
        removeChildren(of: stagingDirectory)

        let fm = FileManager.default
        guard let entries = try? fm.contentsOfDirectory(
            at: updatesDirectory,
            includingPropertiesForKeys: [.isDirectoryKey],
            options: [.skipsHiddenFiles]
        ) else {
            return deleted
        }

        var readyDirs: [(version: VersionTuple, url: URL)] = []
        for entry in entries {
            let name = entry.lastPathComponent
            if name == "staging" { continue }
            guard isDirectory(entry) else { continue }
            // Leave directories we don't recognise as version dirs untouched.
            guard let version = VersionTuple.parse(name) else { continue }

            let manifest = readManifest(at: entry)
            if manifest?.state != .ready {
                if (try? fm.removeItem(at: entry)) != nil { deleted.append(version) }
                continue
            }
            if version <= currentVersion {
                if (try? fm.removeItem(at: entry)) != nil { deleted.append(version) }
                continue
            }
            readyDirs.append((version, entry))
        }

        if let newest = readyDirs.map(\.version).max() {
            for dir in readyDirs where dir.version < newest {
                if (try? fm.removeItem(at: dir.url)) != nil { deleted.append(dir.version) }
            }
        }

        return deleted
    }

    // MARK: - Private helpers

    /// Return a `StagedUpdate` if `dir` holds a still-`.ready` manifest for the
    /// expected version and the bundle is present; otherwise `nil`.
    private func readyStagedUpdate(at dir: URL, expected: VersionTuple) -> StagedUpdate? {
        guard let manifest = readManifest(at: dir),
              manifest.state == .ready,
              VersionTuple.parse(manifest.version) == expected else { return nil }
        let bundleURL = dir.appendingPathComponent("Boss.app", isDirectory: true)
        guard FileManager.default.fileExists(atPath: bundleURL.path) else { return nil }
        return StagedUpdate(
            version: expected,
            tag: manifest.tag,
            bundleURL: bundleURL,
            manifestURL: dir.appendingPathComponent("manifest.json")
        )
    }

    /// Mark the staging directory `.failed`, persist the reason for the next
    /// `cleanup()` to reap, and return a `.failed` outcome.
    private func fail(_ workDir: URL, _ manifest: inout UpdateManifest, _ reason: String) -> DownloadOutcome {
        manifest.state = .failed
        manifest.failureReason = reason
        writeManifest(manifest, to: workDir)
        return .failed(reason: reason)
    }

    private func manifestURL(in dir: URL) -> URL {
        dir.appendingPathComponent("manifest.json")
    }

    private func writeManifest(_ manifest: UpdateManifest, to dir: URL) {
        try? writeManifestThrowing(manifest, to: dir)
    }

    private func writeManifestThrowing(_ manifest: UpdateManifest, to dir: URL) throws {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        let data = try encoder.encode(manifest)
        try ensureDirectory(dir)
        try data.write(to: manifestURL(in: dir), options: .atomic)
    }

    private func readManifest(at dir: URL) -> UpdateManifest? {
        guard let data = try? Data(contentsOf: manifestURL(in: dir)) else { return nil }
        return try? JSONDecoder().decode(UpdateManifest.self, from: data)
    }

    /// Find the `Boss.app` bundle a `ditto` extraction produced. `ditto -x -k`
    /// preserves the archived top-level entry, which is `Boss.app`, but we tolerate
    /// it being one level down (some archives wrap the bundle in a folder).
    private func locateBundle(in dir: URL) -> URL? {
        let direct = dir.appendingPathComponent("Boss.app", isDirectory: true)
        if isDirectory(direct) { return direct }
        guard let entries = try? FileManager.default.contentsOfDirectory(
            at: dir,
            includingPropertiesForKeys: [.isDirectoryKey],
            options: [.skipsHiddenFiles]
        ) else { return nil }
        // Prefer an exact Boss.app, then any *.app.
        if let bundle = entries.first(where: { $0.lastPathComponent == "Boss.app" && isDirectory($0) }) {
            return bundle
        }
        return entries.first(where: { $0.pathExtension == "app" && isDirectory($0) })
    }

    private func ensureDirectory(_ url: URL) throws {
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    }

    /// Remove `url` if present, then recreate it empty.
    private func resetDirectory(_ url: URL) throws {
        try? FileManager.default.removeItem(at: url)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    }

    private func removeChildren(of dir: URL) {
        guard let entries = try? FileManager.default.contentsOfDirectory(
            at: dir,
            includingPropertiesForKeys: nil,
            options: []
        ) else { return }
        for entry in entries {
            try? FileManager.default.removeItem(at: entry)
        }
    }

    private func isDirectory(_ url: URL) -> Bool {
        var isDir: ObjCBool = false
        return FileManager.default.fileExists(atPath: url.path, isDirectory: &isDir) && isDir.boolValue
    }

    private func fileSize(of url: URL) -> Int? {
        guard let attrs = try? FileManager.default.attributesOfItem(atPath: url.path),
              let size = attrs[.size] as? Int else { return nil }
        return size
    }

    private func sha256Hex(of url: URL) -> String? {
        guard let handle = FileHandle(forReadingAtPath: url.path) else { return nil }
        defer { try? handle.close() }
        var hasher = SHA256()
        while let chunk = try? handle.read(upToCount: 1 << 20), !chunk.isEmpty {
            hasher.update(data: chunk)
        }
        return hasher.finalize().map { String(format: "%02x", $0) }.joined()
    }
}
