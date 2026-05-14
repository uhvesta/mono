import CryptoKit
import Darwin
import Foundation

final class EngineProcessController: @unchecked Sendable {
    private let pidFilePath: String
    private let lockFilePath: String
    private let launchDirectory: String
    private let forceRestart: Bool
    private let stopOnExit: Bool

    var onOutputLine: (@MainActor @Sendable (String) -> Void)?

    init(
        pidFilePath: String = ProcessInfo.processInfo.environment["BOSS_ENGINE_PID_PATH"]
            ?? "/tmp/boss-engine.pid",
        launchDirectory: String = ProcessInfo.processInfo.environment["BUILD_WORKSPACE_DIRECTORY"]
            ?? FileManager.default.currentDirectoryPath,
        forceRestart: Bool = ProcessInfo.processInfo.environment["BOSS_ENGINE_FORCE_RESTART"] == "1",
        stopOnExit: Bool = ProcessInfo.processInfo.environment["BOSS_ENGINE_STOP_ON_EXIT"] == "1"
    ) {
        self.pidFilePath = pidFilePath
        self.lockFilePath = "\(pidFilePath).lock"
        self.launchDirectory = launchDirectory
        self.forceRestart = forceRestart
        self.stopOnExit = stopOnExit
    }

    func start(socketPath: String) throws {
        try withStartLock {
            if forceRestart, let pid = currentEnginePID() {
                emit("[engine restart] terminating existing engine pid=\(pid)")
                terminateProcess(pid: pid)
                clearPIDFileIfOwned(pid: pid)
            }

            if let pid = currentEnginePID() {
                // An engine is already running. Check if its binary matches
                // the app's bundled engine. If not, replace it so the user
                // always gets the version that shipped with this app launch.
                // Skip the check when BOSS_ENGINE_CMD is set — the developer
                // is explicitly pointing at a custom engine binary.
                let skipCheck = ProcessInfo.processInfo.environment["BOSS_ENGINE_CMD"] != nil
                if !skipCheck,
                   let bundledPath = bundledEnginePath(),
                   let bundledFP = computeBinaryFingerprint(path: bundledPath),
                   let runningFP = queryRunningEngineFingerprint(
                       socketPath: socketPath, timeoutSeconds: 3.0
                   ),
                   bundledFP != runningFP
                {
                    emit("[engine upgrade] running=\(runningFP) bundled=\(bundledFP) — replacing engine pid=\(pid)")
                    terminateProcess(pid: pid)
                    clearPIDFileIfOwned(pid: pid)
                    let closed = waitForSocketClose(socketPath: socketPath, timeoutSeconds: 8.0)
                    if !closed {
                        emit("[engine upgrade] socket did not close within 8s after SIGTERM; SIGKILL should have fired already")
                    }
                    emit("[engine upgrade] old engine stopped — launching new engine from bundle")
                } else {
                    emit("[engine attach] using existing engine pid=\(pid)")
                    return
                }
            }

            let (command, bossBinDir) = resolveEngineCommand(socketPath: socketPath)

            try launchDetached(command: command, bossBinDir: bossBinDir)
            if let pid = waitForEnginePID(timeoutSeconds: 5.0) {
                emit("[engine launch] detached pid=\(pid) \(command)")
            } else {
                emit("[engine launch] started but pid file not observed yet: \(pidFilePath)")
            }
        }
    }

    // MARK: - Version-check helpers

    /// Path to the engine binary shipped inside the current app bundle.
    /// Returns `nil` in dev/bazel-run mode where no bundle engine exists.
    private func bundledEnginePath() -> String? {
        guard let resourcePath = Bundle.main.resourcePath else { return nil }
        let path = "\(resourcePath)/bin/engine"
        guard FileManager.default.fileExists(atPath: path) else { return nil }
        return path
    }

    /// Compute a binary fingerprint of `path` using the same algorithm
    /// as `boss_engine::build_info::binary_fingerprint`:
    ///   SHA-256 of up to 64 MiB of file content → first 6 bytes as
    ///   12 lowercase hex digits, optionally suffixed "-truncated".
    private func computeBinaryFingerprint(path: String) -> String? {
        guard let fh = FileHandle(forReadingAtPath: path) else { return nil }
        defer { try? fh.close() }

        let cap: Int = 64 * 1024 * 1024
        var hasher = SHA256()
        var readTotal = 0
        var truncated = false
        let chunkSize = 64 * 1024

        while true {
            let remaining = cap - readTotal
            guard remaining > 0 else {
                // Probe for more bytes to set the truncated flag.
                let probe = (try? fh.read(upToCount: 1)) ?? Data()
                if !probe.isEmpty { truncated = true }
                break
            }
            let toRead = min(chunkSize, remaining)
            guard let chunk = try? fh.read(upToCount: toRead), !chunk.isEmpty else { break }
            hasher.update(data: chunk)
            readTotal += chunk.count
            if chunk.count < toRead {
                // EOF before cap.
                break
            }
        }

        let digest = hasher.finalize()
        let firstSixBytes = digest.prefix(6)
        let hex = firstSixBytes.map { String(format: "%02x", $0) }.joined()
        return truncated ? "\(hex)-truncated" : hex
    }

    /// Open a synchronous Unix-domain connection to `socketPath`, send a
    /// `get_engine_version` request, and return the `binary_fingerprint`
    /// from the response. Returns `nil` on any error (timeout, parse
    /// failure, socket unavailable).
    private func queryRunningEngineFingerprint(
        socketPath: String,
        timeoutSeconds: Double
    ) -> String? {
        let sock = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard sock >= 0 else { return nil }
        defer { Darwin.close(sock) }

        // Apply send/recv timeouts so a hung engine doesn't block startup.
        var tv = timeval(
            tv_sec: Int(timeoutSeconds),
            tv_usec: Int32((timeoutSeconds.truncatingRemainder(dividingBy: 1.0)) * 1_000_000)
        )
        setsockopt(sock, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        setsockopt(sock, SOL_SOCKET, SO_SNDTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        // Capture the size of sun_path before the exclusive-access borrow.
        let sunPathMax = MemoryLayout.size(ofValue: addr.sun_path)
        _ = socketPath.withCString { cStr in
            withUnsafeMutablePointer(to: &addr.sun_path) { dst in
                memcpy(UnsafeMutableRawPointer(dst), cStr, min(strlen(cStr), sunPathMax - 1))
            }
        }
        let connectResult: Int32 = withUnsafePointer(to: addr) { ptr in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(sock, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard connectResult == 0 else { return nil }

        let requestJSON = "{\"request_id\":\"version-check\",\"payload\":{\"type\":\"get_engine_version\"}}\n"
        guard let requestData = requestJSON.data(using: .utf8) else { return nil }
        let sent = requestData.withUnsafeBytes { buf in
            Darwin.send(sock, buf.baseAddress!, buf.count, 0)
        }
        guard sent == requestData.count else { return nil }

        // Read newline-delimited JSON until we see our response.
        var responseBuffer = Data()
        var readBuf = [UInt8](repeating: 0, count: 4096)
        outer: while true {
            let n = Darwin.recv(sock, &readBuf, readBuf.count, 0)
            if n <= 0 { break }
            responseBuffer.append(contentsOf: readBuf[..<n])
            while let newlineIdx = responseBuffer.firstIndex(of: 0x0A) {
                let lineData = Data(responseBuffer[..<newlineIdx])
                responseBuffer.removeSubrange(...newlineIdx)
                guard
                    let json = try? JSONSerialization.jsonObject(with: lineData) as? [String: Any],
                    json["request_id"] as? String == "version-check",
                    let payload = json["payload"] as? [String: Any],
                    payload["type"] as? String == "engine_version_result",
                    let fp = payload["binary_fingerprint"] as? String
                else { continue }
                return fp
            }
            // Safety: if we've buffered a lot without finding a match, stop.
            if responseBuffer.count > 256 * 1024 { break outer }
        }
        return nil
    }

    /// Poll until `socketPath` is no longer connectable (the engine has
    /// closed it) or `timeoutSeconds` elapses. Returns `true` if the
    /// socket closed in time.
    private func waitForSocketClose(socketPath: String, timeoutSeconds: Double) -> Bool {
        let deadline = Date().addingTimeInterval(timeoutSeconds)
        while Date() < deadline {
            let sock = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
            guard sock >= 0 else { return true }
            var addr = sockaddr_un()
            addr.sun_family = sa_family_t(AF_UNIX)
            let sunPathMaxClose = MemoryLayout.size(ofValue: addr.sun_path)
            _ = socketPath.withCString { cStr in
                withUnsafeMutablePointer(to: &addr.sun_path) { dst in
                    memcpy(UnsafeMutableRawPointer(dst), cStr,
                           min(strlen(cStr), sunPathMaxClose - 1))
                }
            }
            let result = withUnsafePointer(to: addr) { ptr in
                ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                    Darwin.connect(sock, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
                }
            }
            Darwin.close(sock)
            if result != 0 {
                return true  // Connection refused — socket is closed.
            }
            Thread.sleep(forTimeInterval: 0.2)
        }
        return false
    }

    /// Resolve the engine command and the BOSS_BIN_DIR to export.
    ///
    /// Resolution order (per design doc Q3):
    ///   1. BOSS_ENGINE_CMD env override — wins unconditionally so a dev
    ///      running `bazel run //tools/boss/app-macos:Boss` against a custom
    ///      engine still works.
    ///   2. Bundle-relative path: `<Bundle.main.resourcePath>/bin/engine` —
    ///      the installed app path; BOSS_BIN_DIR is set to the bin/ dir so
    ///      the engine can resolve its sibling CLIs.
    ///   3. `bazel run` fallback — dev mode for when the bundle lacks the
    ///      pre-built engine (e.g. iterating on just the Swift side).
    private func resolveEngineCommand(socketPath: String) -> (command: String, bossBinDir: String?) {
        if let override = ProcessInfo.processInfo.environment["BOSS_ENGINE_CMD"] {
            return (override, nil)
        }
        if let resourcePath = Bundle.main.resourcePath {
            let enginePath = "\(resourcePath)/bin/engine"
            if FileManager.default.fileExists(atPath: enginePath) {
                let bossBinDir = "\(resourcePath)/bin"
                return ("\(enginePath) --socket-path \(socketPath)", bossBinDir)
            }
        }
        return ("bazel run //tools/boss/engine:engine -- --socket-path \(socketPath)", nil)
    }

    func stop() {
        guard stopOnExit else {
            return
        }

        guard let pid = currentEnginePID() else {
            return
        }

        terminateProcess(pid: pid)
        clearPIDFileIfOwned(pid: pid)
        emit("[engine stop] terminated pid=\(pid)")
    }

    private func launchDetached(command: String, bossBinDir: String? = nil) throws {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/bin/zsh")
        proc.arguments = ["-c", "nohup \(command) >/dev/null 2>&1 &"]
        proc.currentDirectoryURL = URL(fileURLWithPath: launchDirectory, isDirectory: true)
        // Tell the engine the app's pid explicitly. `bazel run`
        // daemonizes its server, which reparents the engine binary
        // away from the app's process tree, so `getppid()` and any
        // ancestor walk both miss the real app. The engine reads
        // BOSS_APP_PID to set its trust root for `RegisterAppSession`.
        var env = ProcessInfo.processInfo.environment
        env["BOSS_APP_PID"] = String(getpid())
        // BOSS_BIN_DIR tells the engine where its sibling CLIs live
        // (boss, bossctl, boss-event) in installed mode. The engine
        // propagates this to workers so they resolve the bundled copies
        // rather than any PATH match. Unset in dev mode (no bundle bin/).
        if let dir = bossBinDir {
            env["BOSS_BIN_DIR"] = dir
        }
        // When launched from Finder/Dock/launchctl, the app inherits a minimal
        // launchd GUI session PATH (/usr/bin:/bin:/usr/sbin:/sbin) that omits
        // developer tool directories. The engine and its cube subprocesses need
        // jj, mint, and other tools that live outside that minimal set.
        //
        // We prepend well-known locations rather than shelling out to read the
        // user's login-shell PATH (which would be more accurate but brittle — a
        // misbehaving shell init could hang the app or print garbage). Extra
        // segments that don't exist on a given machine are ignored by the kernel.
        env["PATH"] = augmentedPATH(current: env["PATH"] ?? "/usr/bin:/bin:/usr/sbin:/sbin")
        proc.environment = env
        proc.standardOutput = Pipe()
        proc.standardError = Pipe()

        try proc.run()
        proc.waitUntilExit()
        if proc.terminationStatus != 0 {
            throw NSError(
                domain: "Boss.EngineProcessController",
                code: Int(proc.terminationStatus),
                userInfo: [NSLocalizedDescriptionKey: "failed to launch detached engine process"]
            )
        }
    }

    /// Prepend standard developer-tool directories to PATH so the engine and its
    /// subprocesses (cube, jj, mint, cargo binaries) can be found when the app is
    /// launched from Finder/Dock/launchctl with a minimal launchd PATH.
    ///
    /// Order matches typical shell precedence: Apple Silicon Homebrew, Intel/manual
    /// Homebrew, LinkedIn corporate tools, Rust/Cargo, then user-local directories.
    /// Segments that don't exist on the current machine are harmless — the kernel
    /// skips non-existent PATH entries. The original launchd PATH is preserved at the
    /// end so system tools continue to resolve normally.
    private func augmentedPATH(current: String) -> String {
        let home = ProcessInfo.processInfo.environment["HOME"] ?? NSHomeDirectory()
        let extra = [
            "/opt/homebrew/bin",        // Apple Silicon Homebrew (jj, etc.)
            "/usr/local/bin",           // Intel Homebrew, manual installs
            "/usr/local/linkedin/bin",  // LinkedIn corporate tools (mint, etc.)
            "\(home)/.cargo/bin",       // Rust binaries (jj commonly installed here)
            "\(home)/bin",              // user-local binaries
            "\(home)/.local/bin",       // XDG-style user-local binaries
        ]
        // Deduplicate: keep the first occurrence of each segment.
        var seen = Set(current.split(separator: ":").map(String.init))
        let unique = extra.filter { seen.insert($0).inserted }
        let prefix = unique.joined(separator: ":")
        return prefix.isEmpty ? current : "\(prefix):\(current)"
    }

    private func waitForEnginePID(timeoutSeconds: TimeInterval) -> pid_t? {
        let deadline = Date().addingTimeInterval(timeoutSeconds)
        while Date() < deadline {
            if let pid = currentEnginePID() {
                return pid
            }
            Thread.sleep(forTimeInterval: 0.1)
        }
        return nil
    }

    private func currentEnginePID() -> pid_t? {
        guard let pid = readPIDFile() else {
            return nil
        }

        if !isProcessRunning(pid) {
            clearPIDFileIfOwned(pid: pid)
            return nil
        }

        guard isLikelyEngineProcess(pid) else {
            emit("[engine pid] pid file points to non-engine process pid=\(pid)")
            return nil
        }

        return pid
    }

    private func readPIDFile() -> pid_t? {
        guard let content = try? String(contentsOfFile: pidFilePath, encoding: .utf8) else {
            return nil
        }

        let trimmed = content.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let value = Int32(trimmed), value > 1 else {
            return nil
        }
        return value
    }

    private func clearPIDFileIfOwned(pid: pid_t) {
        guard let owner = readPIDFile(), owner == pid else {
            return
        }
        try? FileManager.default.removeItem(atPath: pidFilePath)
    }

    private func isProcessRunning(_ pid: pid_t) -> Bool {
        if kill(pid, 0) == 0 {
            return true
        }
        return errno == EPERM
    }

    private func isLikelyEngineProcess(_ pid: pid_t) -> Bool {
        guard let command = commandLine(for: pid) else {
            return false
        }

        return command.contains("/tools/boss/engine/engine")
            || command.contains("bazel run //tools/boss/engine:engine")
            || command.contains("Contents/Resources/bin/engine")
    }

    private func commandLine(for pid: pid_t) -> String? {
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: "/bin/ps")
        proc.arguments = ["-p", "\(pid)", "-o", "command="]
        let output = Pipe()
        proc.standardOutput = output
        proc.standardError = Pipe()

        do {
            try proc.run()
            proc.waitUntilExit()
        } catch {
            return nil
        }

        guard proc.terminationStatus == 0 else {
            return nil
        }

        let data = output.fileHandleForReading.readDataToEndOfFile()
        let text = String(data: data, encoding: .utf8)?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        if let text, !text.isEmpty {
            return text
        }
        return nil
    }

    private func terminateProcess(pid: pid_t) {
        guard isProcessRunning(pid) else {
            return
        }

        _ = kill(pid, SIGTERM)
        for _ in 0..<20 {
            if !isProcessRunning(pid) {
                return
            }
            Thread.sleep(forTimeInterval: 0.1)
        }
        _ = kill(pid, SIGKILL)
    }

    private func withStartLock<T>(_ body: () throws -> T) throws -> T {
        let fd = open(lockFilePath, O_CREAT | O_RDWR, 0o600)
        guard fd >= 0 else {
            throw NSError(
                domain: "Boss.EngineProcessController",
                code: Int(errno),
                userInfo: [NSLocalizedDescriptionKey: "failed to open lock file: \(lockFilePath)"]
            )
        }

        defer {
            close(fd)
        }

        guard flock(fd, LOCK_EX) == 0 else {
            throw NSError(
                domain: "Boss.EngineProcessController",
                code: Int(errno),
                userInfo: [NSLocalizedDescriptionKey: "failed to acquire engine start lock"]
            )
        }

        defer {
            _ = flock(fd, LOCK_UN)
        }

        return try body()
    }

    private func emit(_ line: String) {
        Task { @MainActor in
            self.onOutputLine?(line)
        }
    }
}
