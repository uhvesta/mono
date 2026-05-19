import CryptoKit
import Darwin
import Foundation

final class EngineProcessController: @unchecked Sendable {
    private let paths: BossEnginePaths
    private let lockFilePath: String
    private let launchDirectory: String
    private let forceRestart: Bool
    private let stopOnExit: Bool

    var onOutputLine: (@MainActor @Sendable (String) -> Void)?

    init(
        paths: BossEnginePaths,
        launchDirectory: String = ProcessInfo.processInfo.environment["BUILD_WORKSPACE_DIRECTORY"]
            ?? FileManager.default.currentDirectoryPath,
        forceRestart: Bool = ProcessInfo.processInfo.environment["BOSS_ENGINE_FORCE_RESTART"] == "1",
        stopOnExit: Bool = ProcessInfo.processInfo.environment["BOSS_ENGINE_STOP_ON_EXIT"] == "1"
    ) {
        self.paths = paths
        self.lockFilePath = "\(paths.pidPath).lock"
        self.launchDirectory = launchDirectory
        self.forceRestart = forceRestart
        self.stopOnExit = stopOnExit
    }

    func start() throws {
        let socketPath = paths.socketPath
        try withStartLock {
            if forceRestart, let pid = currentEnginePID() {
                emit("[engine restart] terminating existing engine pid=\(pid)")
                terminateEngine(pid: pid)
                clearPIDFileIfOwned(pid: pid)
            }

            if let pid = currentEnginePID() {
                // An engine is already running. Check if its binary matches
                // the app's bundled engine. If not, replace it so the user
                // always gets the version that shipped with this app launch.
                //
                // Each branch emits a distinct log line so a user reporting
                // "engine wasn't restarted after I updated Boss" can grep
                // the system messages and tell exactly which path fired:
                //   [engine version-check skipped: <reason>] — check didn't run
                //   [engine version-check ok] — ran, fingerprints matched
                //   [engine upgrade] — ran, fingerprints differed, restarting
                let reasonToSkip: String? = {
                    if ProcessInfo.processInfo.environment["BOSS_ENGINE_CMD"] != nil {
                        return "BOSS_ENGINE_CMD is set (developer custom engine)"
                    }
                    if bundledEnginePath() == nil {
                        return "no bundled engine in app resources (dev/bazel-run mode)"
                    }
                    return nil
                }()

                if let reason = reasonToSkip {
                    emit("[engine version-check skipped: \(reason)] attaching to pid=\(pid)")
                    return
                }

                // We know bundledEnginePath() is non-nil from the guard above.
                guard let bundledPath = bundledEnginePath(),
                      let bundledFP = computeBinaryFingerprint(path: bundledPath)
                else {
                    emit("[engine version-check skipped: could not fingerprint bundled engine] attaching to pid=\(pid)")
                    return
                }

                guard let runningFP = queryRunningEngineFingerprint(
                    socketPath: socketPath, timeoutSeconds: 3.0
                ) else {
                    // Engine either pre-dates GetEngineVersion or didn't
                    // answer in time. Safe fallback is to keep it rather
                    // than restart blindly; flag the reason explicitly so
                    // a user investigating "did the version check fire?"
                    // can tell the query failed versus the check was a
                    // no-op for some other reason.
                    emit("[engine version-check skipped: running engine did not respond to get_engine_version within 3s; likely pre-T460 binary] attaching to pid=\(pid)")
                    return
                }

                if bundledFP == runningFP {
                    emit("[engine version-check ok] running=\(runningFP) matches bundled — attaching to pid=\(pid)")
                    return
                }

                emit("[engine upgrade] running=\(runningFP) bundled=\(bundledFP) — replacing engine pid=\(pid)")
                terminateEngine(pid: pid)
                clearPIDFileIfOwned(pid: pid)
                let closed = waitForSocketClose(socketPath: socketPath, timeoutSeconds: 8.0)
                if !closed {
                    emit("[engine upgrade] socket did not close within 8s after shutdown rpc; SIGKILL should have fired already")
                }
                emit("[engine upgrade] old engine stopped — launching new engine from bundle")
            }

            let (command, bossBinDir) = resolveEngineCommand(socketPath: socketPath)

            try launchDetached(command: command, bossBinDir: bossBinDir)
            if let pid = waitForEnginePID(timeoutSeconds: 5.0) {
                emit("[engine launch] detached pid=\(pid) \(command)")
            } else {
                emit("[engine launch] started but pid file not observed yet: \(paths.pidPath)")
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

        terminateEngine(pid: pid)
        clearPIDFileIfOwned(pid: pid)
        emit("[engine stop] terminated pid=\(pid)")
    }

    /// User-initiated recovery for a stale engine: terminate whatever
    /// engine is bound to the pid file (token-auth RPC first, then
    /// SIGTERM/SIGKILL — same authority `stop()` uses) and relaunch
    /// from the same binary `start()` would. Used by the "Restart
    /// engine" affordance on the unreachable banner so a hung or
    /// orphaned engine no longer requires a shell `pkill` (issue #697).
    ///
    /// Holds the start lock for the whole terminate + launch sequence
    /// so a concurrent `start()` can't race and end up with two
    /// engines fighting over the same socket. Safe to call when no
    /// engine is running — falls through to the launch step.
    func restart() throws {
        try withStartLock {
            if let pid = currentEnginePID() {
                emit("[engine restart] terminating existing engine pid=\(pid)")
                terminateEngine(pid: pid)
                clearPIDFileIfOwned(pid: pid)
                // The engine itself unlinks its socket on graceful
                // exit, and the new engine's bind step will retry-
                // delete any leftover. Wait briefly for the socket
                // to drop so the new bind doesn't trip the unlink
                // race in `UnixListener::bind`.
                _ = waitForSocketClose(socketPath: paths.socketPath, timeoutSeconds: 3.0)
            }

            let socketPath = paths.socketPath
            let (command, bossBinDir) = resolveEngineCommand(socketPath: socketPath)
            try launchDetached(command: command, bossBinDir: bossBinDir)
            if let pid = waitForEnginePID(timeoutSeconds: 5.0) {
                emit("[engine restart] detached pid=\(pid) \(command)")
            } else {
                emit("[engine restart] started but pid file not observed yet: \(paths.pidPath)")
            }
        }
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
        guard let content = try? String(contentsOfFile: paths.pidPath, encoding: .utf8) else {
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
        try? FileManager.default.removeItem(atPath: paths.pidPath)
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

    /// Tear down the running engine. Preferred path is the
    /// token-authenticated shutdown RPC — same authority the CLI
    /// uses (issue #705). Falls through to SIGTERM only when the
    /// token is unavailable, the socket isn't reachable, or the
    /// engine refused the token; the everyday case never sends
    /// SIGTERM, so a test that ends up here without a valid token
    /// is rejected by the engine rather than killing it.
    private func terminateEngine(pid: pid_t) {
        guard isProcessRunning(pid) else {
            return
        }

        if attemptRpcShutdown() {
            // Wait for the engine to actually exit before returning so
            // the caller can `clearPIDFileIfOwned` and then re-spawn
            // without racing the still-alive process.
            for _ in 0..<50 {
                if !isProcessRunning(pid) {
                    return
                }
                Thread.sleep(forTimeInterval: 0.1)
            }
            emit("[engine stop] rpc accepted but pid=\(pid) still alive after 5s; falling back to SIGKILL")
            _ = kill(pid, SIGKILL)
            return
        }

        emit("[engine stop] rpc shutdown unavailable; falling back to SIGTERM pid=\(pid)")
        _ = kill(pid, SIGTERM)
        for _ in 0..<20 {
            if !isProcessRunning(pid) {
                return
            }
            Thread.sleep(forTimeInterval: 0.1)
        }
        _ = kill(pid, SIGKILL)
    }

    /// Try the token-authenticated shutdown RPC. Returns `true` when
    /// the engine acknowledged `ShutdownAccepted`. Any failure
    /// (no token file, socket unreachable, token rejected, malformed
    /// reply) returns `false` so the caller can fall back to SIGTERM.
    private func attemptRpcShutdown() -> Bool {
        let tokenPath = paths.controlTokenPath
        guard FileManager.default.fileExists(atPath: tokenPath) else {
            return false
        }
        guard let raw = try? String(contentsOfFile: tokenPath, encoding: .utf8),
              let data = raw.data(using: .utf8),
              let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let token = json["token"] as? String
        else {
            return false
        }
        // Prefer the socket path the engine wrote into the token file
        // so we always dial whichever process actually minted the
        // token, even if the env override has since changed.
        let socketPath = (json["socket_path"] as? String) ?? paths.socketPath
        return sendShutdownRequest(socketPath: socketPath, token: token, timeoutSeconds: 5.0)
    }

    /// Open a synchronous Unix-domain connection, send a `shutdown`
    /// request with the supplied token, and wait for either
    /// `shutdown_accepted` or `shutdown_rejected`. Returns `true`
    /// only on `shutdown_accepted`.
    private func sendShutdownRequest(
        socketPath: String,
        token: String,
        timeoutSeconds: Double
    ) -> Bool {
        let sock = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard sock >= 0 else { return false }
        defer { Darwin.close(sock) }

        var tv = timeval(
            tv_sec: Int(timeoutSeconds),
            tv_usec: Int32((timeoutSeconds.truncatingRemainder(dividingBy: 1.0)) * 1_000_000)
        )
        setsockopt(sock, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))
        setsockopt(sock, SOL_SOCKET, SO_SNDTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size))

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
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
        guard connectResult == 0 else { return false }

        let request: [String: Any] = [
            "request_id": "engine-stop",
            "payload": [
                "type": "shutdown",
                "token": token,
            ],
        ]
        guard let body = try? JSONSerialization.data(withJSONObject: request) else { return false }
        var line = body
        line.append(0x0A)
        let sent = line.withUnsafeBytes { buf in
            Darwin.send(sock, buf.baseAddress!, buf.count, 0)
        }
        guard sent == line.count else { return false }

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
                    json["request_id"] as? String == "engine-stop",
                    let payload = json["payload"] as? [String: Any],
                    let kind = payload["type"] as? String
                else { continue }
                if kind == "shutdown_accepted" {
                    return true
                }
                if kind == "shutdown_rejected" {
                    let reason = (payload["reason"] as? String) ?? "unknown"
                    emit("[engine stop] shutdown rpc rejected: \(reason)")
                    return false
                }
            }
            if responseBuffer.count > 256 * 1024 { break outer }
        }
        return false
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
