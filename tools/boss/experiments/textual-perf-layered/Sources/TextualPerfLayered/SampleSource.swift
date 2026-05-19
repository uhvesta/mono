import Foundation

/// Resolves the markdown sample. Defaults to the same 47 KB design doc the
/// textual-perf rig uses (`tools/boss/docs/designs/installable-distribution-package-for-boss.md`)
/// so timing numbers compare directly with PR #686, without duplicating
/// the file into this experiment's Resources folder.
///
/// Resolution order:
///   1. `BOSS_SAMPLE_MD` environment variable, if set, as an absolute path.
///   2. Walk up from the current working directory looking for
///      `tools/boss/docs/designs/installable-distribution-package-for-boss.md`.
///      Lets `swift run` / `swift run --package-path …` and `cd
///      tools/boss/experiments/textual-perf-layered && swift run` both work.
///   3. Fail with a clear message so the user knows how to fix it.
struct SampleSource {
    let text: String
    let errorMessage: String?

    static func load() -> SampleSource {
        if let envPath = ProcessInfo.processInfo.environment["BOSS_SAMPLE_MD"],
           !envPath.isEmpty {
            return read(at: URL(fileURLWithPath: envPath))
        }
        let needle = "tools/boss/docs/designs/installable-distribution-package-for-boss.md"
        let fm = FileManager.default
        var dir = URL(fileURLWithPath: fm.currentDirectoryPath)
        for _ in 0..<8 {
            let candidate = dir.appendingPathComponent(needle)
            if fm.fileExists(atPath: candidate.path) {
                return read(at: candidate)
            }
            let parent = dir.deletingLastPathComponent()
            if parent.path == dir.path { break }
            dir = parent
        }
        return SampleSource(
            text: """
                # Sample not found

                Could not locate the 47 KB design-doc sample. Set
                `BOSS_SAMPLE_MD` to the absolute path of the markdown
                file you want to measure, or run this rig from inside
                a `mono` workspace so it can walk up to
                `tools/boss/docs/designs/installable-distribution-package-for-boss.md`.
                """,
            errorMessage: "Sample not found — see in-app message; numbers below are for a 1 KB placeholder."
        )
    }

    private static func read(at url: URL) -> SampleSource {
        do {
            let text = try String(contentsOf: url, encoding: .utf8)
            return SampleSource(text: text, errorMessage: nil)
        } catch {
            return SampleSource(
                text: "# Read failed\n\n\(error.localizedDescription)",
                errorMessage: "Failed to read \(url.path): \(error.localizedDescription)"
            )
        }
    }
}
