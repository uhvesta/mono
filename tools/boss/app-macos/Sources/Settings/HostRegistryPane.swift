import SwiftUI

/// "Hosts" settings pane — lists registered SSH hosts and surfaces add /
/// enable / disable / remove / tag controls. Mirrors the capability of
/// `bossctl hosts` through the engine's host-registry RPCs so the app is
/// always a thin client and never writes `state.db` directly.
struct HostRegistryPane: View {
    @EnvironmentObject private var chatModel: ChatViewModel

    @State private var showAddSheet = false
    @State private var hostToRemove: EngineHost?
    @State private var showRemoveConfirm = false
    @State private var selectedHostID: String?

    var body: some View {
        VStack(spacing: 0) {
            if chatModel.registeredHosts.isEmpty {
                ContentUnavailableView(
                    "No Hosts Registered",
                    systemImage: "server.rack",
                    description: Text("Add a remote SSH host to distribute work across machines.")
                )
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                List(selection: $selectedHostID) {
                    ForEach(chatModel.registeredHosts) { host in
                        HostRow(host: host)
                            .tag(host.hostId)
                    }
                }
                .listStyle(.inset)
            }

            Divider()

            HStack {
                Button {
                    showAddSheet = true
                } label: {
                    Label("Add Host", systemImage: "plus")
                }
                .buttonStyle(.borderless)
                .padding(.leading, 8)

                Spacer()

                if let selectedID = selectedHostID,
                   let host = chatModel.registeredHosts.first(where: { $0.hostId == selectedID }),
                   !host.isLocal {
                    Button(role: .destructive) {
                        hostToRemove = host
                        showRemoveConfirm = true
                    } label: {
                        Label("Remove", systemImage: "minus")
                    }
                    .buttonStyle(.borderless)
                    .padding(.trailing, 8)
                }
            }
            .padding(.vertical, 6)
        }
        .onAppear {
            chatModel.refreshHosts()
        }
        .sheet(isPresented: $showAddSheet) {
            AddHostSheet()
                .environmentObject(chatModel)
        }
        .confirmationDialog(
            "Remove host \"\(hostToRemove?.hostId ?? "")\"?",
            isPresented: $showRemoveConfirm,
            titleVisibility: .visible
        ) {
            Button("Remove", role: .destructive) {
                if let host = hostToRemove {
                    chatModel.removeHost(id: host.hostId)
                    if selectedHostID == host.hostId {
                        selectedHostID = nil
                    }
                }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This permanently removes the host from the registry. Running workers on this host are not affected.")
        }
    }
}

// ── Host list row ─────────────────────────────────────────────────────────────

private struct HostRow: View {
    @EnvironmentObject private var chatModel: ChatViewModel
    let host: EngineHost

    @State private var showDetail = false

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: iconName)
                .foregroundStyle(iconColor)
                .frame(width: 18)

            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(host.hostId)
                        .font(.body.weight(.medium))
                    if host.isLocal {
                        Text("built-in")
                            .font(.caption2)
                            .padding(.horizontal, 5)
                            .padding(.vertical, 1)
                            .background(.quaternary)
                            .clipShape(RoundedRectangle(cornerRadius: 4))
                    }
                    if !host.enabled {
                        Text("disabled")
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                            .padding(.horizontal, 5)
                            .padding(.vertical, 1)
                            .background(Color.orange.opacity(0.15))
                            .clipShape(RoundedRectangle(cornerRadius: 4))
                    }
                }
                HStack(spacing: 6) {
                    if let target = host.sshTarget {
                        Text(target)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    Text("\(host.capabilities.count) capabilities")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                if let errorText = host.lastErrorText {
                    Text(errorText)
                        .font(.caption)
                        .foregroundStyle(.red)
                        .lineLimit(1)
                }
            }

            Spacer()

            if !host.isLocal {
                Toggle("", isOn: Binding(
                    get: { host.enabled },
                    set: { chatModel.setHostEnabled(id: host.hostId, enabled: $0) }
                ))
                .toggleStyle(.switch)
                .labelsHidden()
            }

            Button {
                showDetail = true
            } label: {
                Image(systemName: "info.circle")
            }
            .buttonStyle(.borderless)
            .help("Show host details")
        }
        .padding(.vertical, 2)
        .sheet(isPresented: $showDetail) {
            HostDetailSheet(host: host)
                .environmentObject(chatModel)
        }
    }

    private var iconName: String {
        if host.isLocal { return "desktopcomputer" }
        return host.enabled ? "server.rack" : "server.rack"
    }

    private var iconColor: Color {
        guard host.enabled else { return .secondary }
        return host.lastErrorText != nil ? .orange : .green
    }
}

// ── Host detail sheet ─────────────────────────────────────────────────────────

private struct HostDetailSheet: View {
    @EnvironmentObject private var chatModel: ChatViewModel
    @Environment(\.dismiss) private var dismiss

    let host: EngineHost

    @State private var newTag = ""
    @State private var tagError: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text(host.hostId)
                    .font(.title2.weight(.semibold))
                Spacer()
                Button("Done") { dismiss() }
                    .keyboardShortcut(.return)
            }
            .padding()

            Divider()

            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    // ── Identity ──────────────────────────────────────────
                    Section("Identity") {
                        LabeledContent("ID", value: host.hostId)
                        if let target = host.sshTarget {
                            LabeledContent("SSH target", value: target)
                        }
                        LabeledContent("Pool size", value: "\(host.poolSize) slot\(host.poolSize == 1 ? "" : "s")")
                        LabeledContent("Registered", value: host.createdAt)
                        if let seen = host.lastSeenAt {
                            LabeledContent("Last seen", value: seen)
                        }
                        if let err = host.lastErrorText {
                            LabeledContent("Last error") {
                                Text(err)
                                    .foregroundStyle(.red)
                                    .fixedSize(horizontal: false, vertical: true)
                            }
                        }
                    }

                    Divider()

                    // ── Capabilities ──────────────────────────────────────
                    Section("Capabilities") {
                        if host.capabilities.isEmpty {
                            Text("None discovered yet.")
                                .foregroundStyle(.secondary)
                                .font(.caption)
                        } else {
                            ForEach(host.capabilities) { cap in
                                HStack {
                                    Text(cap.capability)
                                        .font(.system(.caption, design: .monospaced))
                                    Spacer()
                                    Text(cap.source)
                                        .font(.caption2)
                                        .foregroundStyle(.secondary)
                                    if cap.source == "user" && !host.isLocal {
                                        Button {
                                            chatModel.removeHostTag(
                                                hostId: host.hostId,
                                                tag: cap.capability
                                            )
                                        } label: {
                                            Image(systemName: "minus.circle")
                                        }
                                        .buttonStyle(.borderless)
                                        .foregroundStyle(.red)
                                    }
                                }
                            }
                        }

                        if !host.isLocal {
                            HStack {
                                TextField("Add tag (e.g. os=macos)", text: $newTag)
                                    .textFieldStyle(.roundedBorder)
                                    .onSubmit { addTag() }
                                Button("Add") { addTag() }
                                    .disabled(newTag.trimmingCharacters(in: .whitespaces).isEmpty)
                            }
                            if let err = tagError {
                                Text(err)
                                    .font(.caption)
                                    .foregroundStyle(.red)
                            }
                        }
                    }
                }
                .padding()
            }
        }
        .frame(minWidth: 440, minHeight: 380)
    }

    private func addTag() {
        let trimmed = newTag.trimmingCharacters(in: .whitespaces)
        guard !trimmed.isEmpty else { return }
        chatModel.addHostTag(hostId: host.hostId, tag: trimmed)
        newTag = ""
        tagError = nil
    }
}

// ── Add host sheet ────────────────────────────────────────────────────────────

private struct AddHostSheet: View {
    @EnvironmentObject private var chatModel: ChatViewModel
    @Environment(\.dismiss) private var dismiss

    @State private var hostId = ""
    @State private var sshTarget = ""
    @State private var poolSizeString = "1"
    @State private var tagsString = ""
    @State private var isAdding = false

    private var poolSize: Int { Int(poolSizeString) ?? 1 }

    private var canAdd: Bool {
        !hostId.trimmingCharacters(in: .whitespaces).isEmpty
            && !sshTarget.trimmingCharacters(in: .whitespaces).isEmpty
            && !isAdding
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text("Add Remote Host")
                    .font(.title2.weight(.semibold))
                Spacer()
                Button("Cancel") { dismiss() }
            }
            .padding()

            Divider()

            Form {
                Section {
                    TextField("e.g. zakalwe", text: $hostId)
                        .autocorrectionDisabled()
                    TextField("user@hostname or SSH alias", text: $sshTarget)
                        .autocorrectionDisabled()
                } header: {
                    Text("Required")
                }

                Section {
                    TextField("Worker pool size", text: $poolSizeString, prompt: Text("1"))
                        .onChange(of: poolSizeString) { _, v in
                            if v.isEmpty { return }
                            poolSizeString = v.filter { $0.isNumber }
                            if poolSizeString.isEmpty { poolSizeString = "1" }
                        }
                        .help("Number of concurrent worker slots Boss may run on this host. Defaults to 1.")
                    TextField("os=macos arch=arm64 …", text: $tagsString)
                        .autocorrectionDisabled()
                        .help("Space-separated capability tags. You can add or remove tags later.")
                } header: {
                    Text("Optional")
                }
            }
            .formStyle(.grouped)

            Divider()

            HStack {
                Spacer()
                if isAdding {
                    ProgressView()
                        .controlSize(.small)
                    Text("Registering and pushing wrapper…")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Button("Add Host") {
                    submitAdd()
                }
                .keyboardShortcut(.return)
                .disabled(!canAdd)
            }
            .padding()
        }
        .frame(minWidth: 480, minHeight: 340)
    }

    private func submitAdd() {
        let id = hostId.trimmingCharacters(in: .whitespaces)
        let target = sshTarget.trimmingCharacters(in: .whitespaces)
        let tags = tagsString
            .trimmingCharacters(in: .whitespaces)
            .split(separator: " ")
            .map(String.init)
            .filter { !$0.isEmpty }

        isAdding = true
        chatModel.addHost(id: id, sshTarget: target, poolSize: poolSize, tags: tags)
        // The engine reply (host_result) updates registeredHosts; dismiss
        // immediately so the user can watch the row appear in the list.
        dismiss()
    }
}
