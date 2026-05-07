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
                emit("[engine attach] using existing engine pid=\(pid)")
                return
            }

            let command = ProcessInfo.processInfo.environment["BOSS_ENGINE_CMD"]
                ?? "bazel run //tools/boss/engine:engine -- --mode=server --socket-path \(socketPath)"

            try launchDetached(command: command)
            if let pid = waitForEnginePID(timeoutSeconds: 5.0) {
                emit("[engine launch] detached pid=\(pid) \(command)")
            } else {
                emit("[engine launch] started but pid file not observed yet: \(pidFilePath)")
            }
        }
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

    private func launchDetached(command: String) throws {
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

        return command.contains("/tools/boss/engine/engine --mode=server")
            || command.contains("bazel run //tools/boss/engine:engine -- --mode=server")
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
