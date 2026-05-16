import AppKit
import Foundation
import SwiftUI

// MARK: - DispatchEvent model

/// Mirror of `DispatchEvent` from
/// `tools/boss/engine/src/dispatch_events.rs`. Decoded with relaxed
/// rules — unknown fields are ignored and missing optionals are
/// tolerated so the viewer keeps working if the engine adds fields.
struct DispatchEvent: Identifiable, Hashable {
    let id: UUID = UUID()
    let tsEpochMs: UInt64
    let stage: String
    let outcome: String
    let executionId: String
    let workItemId: String?
    let workerId: String?
    let cubeRepoId: String?
    let cubeLeaseId: String?
    let cubeWorkspaceId: String?
    let errorMessage: String?
    let cubeCommand: String?
    let cubeCwd: String?
    let detailsJSON: String?

    var timestamp: Date {
        Date(timeIntervalSince1970: TimeInterval(tsEpochMs) / 1000.0)
    }

    /// Short tail of an opaque id like `exec_18ae9d258b5872e8_48` →
    /// the trailing component after the last `_`, suitable for a
    /// compact table column.
    static func shortId(_ id: String) -> String {
        guard let underscore = id.lastIndex(of: "_") else { return id }
        return String(id[id.index(after: underscore)...])
    }
}

enum DispatchEventDecoder {
    /// Parse one JSONL line. Returns `nil` for blank lines and lines
    /// that don't decode (the viewer drops them quietly — best-effort
    /// matches the writer's best-effort emit contract).
    static func decode(line: String) -> DispatchEvent? {
        let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, let data = trimmed.data(using: .utf8) else { return nil }
        guard let any = try? JSONSerialization.jsonObject(with: data, options: []),
              let dict = any as? [String: Any] else { return nil }

        let ts: UInt64
        if let n = dict["ts_epoch_ms"] as? NSNumber {
            ts = n.uint64Value
        } else { return nil }
        guard let stage = dict["stage"] as? String,
              let outcome = dict["outcome"] as? String,
              let executionId = dict["execution_id"] as? String else { return nil }

        let detailsJSON: String?
        if let details = dict["details"], !(details is NSNull) {
            if let detailsData = try? JSONSerialization.data(
                withJSONObject: details,
                options: [.prettyPrinted, .sortedKeys]
            ) {
                detailsJSON = String(data: detailsData, encoding: .utf8)
            } else {
                detailsJSON = nil
            }
        } else {
            detailsJSON = nil
        }

        return DispatchEvent(
            tsEpochMs: ts,
            stage: stage,
            outcome: outcome,
            executionId: executionId,
            workItemId: dict["work_item_id"] as? String,
            workerId: dict["worker_id"] as? String,
            cubeRepoId: dict["cube_repo_id"] as? String,
            cubeLeaseId: dict["cube_lease_id"] as? String,
            cubeWorkspaceId: dict["cube_workspace_id"] as? String,
            errorMessage: dict["error_message"] as? String,
            cubeCommand: dict["cube_command"] as? String,
            cubeCwd: dict["cube_cwd"] as? String,
            detailsJSON: detailsJSON
        )
    }
}

// MARK: - Filesystem paths

enum DispatchEventsPaths {
    /// Boss state root — `~/Library/Application Support/Boss`. The
    /// engine code in `engine/src/main.rs` resolves the same path.
    static func stateRoot() -> URL {
        let fm = FileManager.default
        let appSupport = fm
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)
            .first ?? URL(fileURLWithPath: NSHomeDirectory())
                .appendingPathComponent("Library/Application Support")
        return appSupport.appendingPathComponent("Boss", isDirectory: true)
    }

    /// Source-of-truth JSONL stream.
    static func currentJsonl() -> URL {
        stateRoot()
            .appendingPathComponent("dispatch-events", isDirectory: true)
            .appendingPathComponent("current.jsonl")
    }

    /// Per-execution mirror.
    static func executionJsonl(executionId: String) -> URL {
        stateRoot()
            .appendingPathComponent("executions", isDirectory: true)
            .appendingPathComponent(executionId, isDirectory: true)
            .appendingPathComponent("dispatch.jsonl")
    }
}

// MARK: - Live tail

/// Tails an append-only JSONL file. Re-opens cleanly when the file
/// is rotated (inode changes) or truncated. Read I/O happens on a
/// background queue; new events are delivered to `onEvents` /
/// `onEmpty` after a `DispatchQueue.main.async` hop.
final class DispatchEventsTailer: @unchecked Sendable {
    private let url: URL
    private let onEvents: ([DispatchEvent]) -> Void
    private let onEmpty: () -> Void

    private let ioQueue = DispatchQueue(label: "boss.dispatch-events.tail", qos: .utility)
    private var fileHandle: FileHandle?
    private var source: DispatchSourceFileSystemObject?
    private var readOffset: UInt64 = 0
    private var pollTimer: DispatchSourceTimer?
    private var monitoredInode: UInt64?
    private var running = false

    init(
        url: URL,
        onEvents: @escaping ([DispatchEvent]) -> Void,
        onEmpty: @escaping () -> Void
    ) {
        self.url = url
        self.onEvents = onEvents
        self.onEmpty = onEmpty
    }

    func start() {
        ioQueue.async { [weak self] in
            guard let self else { return }
            if self.running { return }
            self.running = true
            self.openAndDrain(initial: true)
            self.startPollTimer()
        }
    }

    func stop() {
        ioQueue.async { [weak self] in
            guard let self else { return }
            self.running = false
            self.pollTimer?.cancel()
            self.pollTimer = nil
            self.source?.cancel()
            self.source = nil
            try? self.fileHandle?.close()
            self.fileHandle = nil
            self.readOffset = 0
            self.monitoredInode = nil
        }
    }

    private func startPollTimer() {
        let timer = DispatchSource.makeTimerSource(queue: ioQueue)
        timer.schedule(deadline: .now() + 2, repeating: 2)
        timer.setEventHandler { [weak self] in
            self?.reconcile()
        }
        pollTimer = timer
        timer.resume()
    }

    private func openAndDrain(initial: Bool) {
        let path = url.path
        var statBuf = stat()
        if stat(path, &statBuf) != 0 {
            if initial {
                let cb = onEmpty
                DispatchQueue.main.async { cb() }
            }
            return
        }

        let handle: FileHandle
        do {
            handle = try FileHandle(forReadingFrom: url)
        } catch {
            return
        }

        try? fileHandle?.close()
        source?.cancel()

        fileHandle = handle
        readOffset = 0
        monitoredInode = UInt64(statBuf.st_ino)

        let src = DispatchSource.makeFileSystemObjectSource(
            fileDescriptor: handle.fileDescriptor,
            eventMask: [.extend, .write, .delete, .rename],
            queue: ioQueue
        )
        src.setEventHandler { [weak self] in
            self?.handleSourceEvent(events: src.data)
        }
        src.resume()
        source = src

        drain()
    }

    private func handleSourceEvent(events: DispatchSource.FileSystemEvent) {
        if events.contains(.delete) || events.contains(.rename) {
            openAndDrain(initial: false)
            return
        }
        drain()
    }

    private func drain() {
        guard let handle = fileHandle else { return }
        let currentSize: UInt64
        do {
            currentSize = try handle.seekToEnd()
        } catch {
            return
        }
        if currentSize < readOffset {
            readOffset = 0
        }
        if currentSize == readOffset { return }

        do {
            try handle.seek(toOffset: readOffset)
        } catch {
            return
        }

        guard let data = try? handle.readToEnd(), !data.isEmpty else { return }
        readOffset = currentSize

        guard let chunk = String(data: data, encoding: .utf8) else { return }
        var parsed: [DispatchEvent] = []
        for line in chunk.split(separator: "\n", omittingEmptySubsequences: true) {
            if let event = DispatchEventDecoder.decode(line: String(line)) {
                parsed.append(event)
            }
        }
        if parsed.isEmpty { return }
        let cb = onEvents
        DispatchQueue.main.async { cb(parsed) }
    }

    /// Catch the rotate/replace case where our monitored fd points
    /// at an inode that's been unlinked.
    private func reconcile() {
        let path = url.path
        var statBuf = stat()
        if stat(path, &statBuf) != 0 { return }
        let currentInode = UInt64(statBuf.st_ino)
        if currentInode != monitoredInode {
            openAndDrain(initial: false)
        }
    }
}
