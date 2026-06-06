import Foundation

extension ChatViewModel {
    // MARK: - Host registry

    /// Ask the engine for the full host registry. Called by the Hosts
    /// settings pane on appear.
    func refreshHosts() {
        engine.sendListHosts()
    }

    /// Enable or disable a host. Optimistically updates the cached list
    /// so the toggle feels instant; `host_updated` reconciles once the
    /// engine confirms.
    func setHostEnabled(id: String, enabled: Bool) {
        updateCachedHost(id: id) { h in
            EngineHost(
                hostId: h.hostId,
                sshTarget: h.sshTarget,
                poolSize: h.poolSize,
                enabled: enabled,
                lastSeenAt: h.lastSeenAt,
                lastErrorText: h.lastErrorText,
                createdAt: h.createdAt,
                capabilities: h.capabilities
            )
        }
        engine.sendSetHostEnabled(id: id, enabled: enabled)
    }

    /// Register a new remote SSH host.
    func addHost(id: String, sshTarget: String, poolSize: Int = 1, tags: [String] = []) {
        engine.sendAddHost(id: id, sshTarget: sshTarget, poolSize: poolSize, tags: tags)
    }

    /// Remove a registered host.
    func removeHost(id: String) {
        engine.sendRemoveHost(id: id)
    }

    /// Add a user-defined capability tag to a host.
    func addHostTag(hostId: String, tag: String) {
        engine.sendAddHostTag(hostId: hostId, tag: tag)
    }

    /// Remove a user-defined capability tag from a host.
    func removeHostTag(hostId: String, tag: String) {
        engine.sendRemoveHostTag(hostId: hostId, tag: tag)
    }

    private func updateCachedHost(id: String, transform: (EngineHost) -> EngineHost) {
        if let idx = registeredHosts.firstIndex(where: { $0.hostId == id }) {
            registeredHosts[idx] = transform(registeredHosts[idx])
        }
    }
}
