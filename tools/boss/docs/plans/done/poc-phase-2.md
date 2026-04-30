# Plan: PoC Phase 2 — Multi-Agent

## Goal

Prove out interaction with multiple agents simultaneously. The user should be
able to spin up more than one agent, see them listed in a sidebar, switch
between them, and interact with each independently.

This phase validates the multi-agent architecture end-to-end: engine support
for concurrent agents, frontend protocol changes, and a sidebar-based UI for
navigating between them.

## What "Done" Looks Like

1. User launches the app and sees a sidebar with an initial agent.
2. User can create additional agents from the sidebar.
3. Each agent has its own independent chat timeline.
4. Selecting an agent in the sidebar shows that agent's conversation.
5. Multiple agents can be active concurrently (one can be processing while the
   user interacts with another).

## UI Design

### Sidebar Navigation

Add a source-list sidebar to the left side of the window, styled after the
macOS system sidebar convention (Xcode / Finder):

- Window traffic light buttons (close/minimize/zoom) sit inside the sidebar
  area in the top-left, consistent with standard macOS split-view apps.
- The sidebar has a translucent material background (`.sidebar` material).
- Sidebar items represent agents. Each shows the agent's name and status
  (active, idle, error).
- The selected agent is highlighted with the standard system selection style.
- A "+" button at the bottom (or top) of the sidebar creates a new agent.
- The main content area (chat view, composer) shows the selected agent's
  conversation.

### Implementation Approach

Use SwiftUI `NavigationSplitView` with a `.sidebar` column. This gives us
standard macOS sidebar behavior: traffic lights in sidebar, resize handle,
collapse/expand, and the correct material/selection styling out of the box.

### Layout

```
┌──────────────┬─────────────────────────────────┐
│ ● ● ●        │  Agent Name              Status │
│              │─────────────────────────────────│
│  Agent 1  ◀─│                                 │
│  Agent 2     │   Chat messages...              │
│  Agent 3     │                                 │
│              │                                 │
│              │                                 │
│              │                                 │
│              │─────────────────────────────────│
│   [+] New    │  [Type a message...]      [▶]  │
└──────────────┴─────────────────────────────────┘
```

## Engine Changes

### Current State

The engine currently supports a single agent per frontend connection:

- `handle_frontend_connection()` in `app.rs` creates one `AcpClient`, one
  session, and one `prompt_lock`.
- Terminal IDs, permission IDs, and events are all globally scoped.
- The frontend protocol has no concept of agent identity — messages like
  `prompt`, `chunk`, `done` implicitly refer to the single agent.

### Required Changes

#### 1. Agent Registry

Introduce an `Agent` struct and a registry that manages multiple agents within
a single frontend connection:

```rust
struct Agent {
    id: String,
    name: String,
    acp_client: Arc<AcpClient>,
    session_id: String,
    prompt_lock: Arc<Mutex<()>>,
}
```

The registry holds a `HashMap<String, Agent>` and supports creating, listing,
and removing agents.

Each agent gets its own `AcpClient` instance (its own ACP adapter subprocess),
its own session, and its own prompt lock. This avoids complex multiplexing and
keeps agents fully isolated.

#### 2. Frontend Protocol Extensions

Add an `agent_id` field to all messages, and add new message types for agent
lifecycle:

**New frontend → engine messages:**
```json
{"type": "create_agent", "name": "Agent 1"}
{"type": "list_agents"}
{"type": "remove_agent", "agent_id": "agent-1"}
```

**Extended existing messages (add `agent_id`):**
```json
{"type": "prompt", "agent_id": "agent-1", "text": "..."}
{"type": "permission_response", "agent_id": "agent-1", "id": "perm-1", "granted": true}
```

**New engine → frontend messages:**
```json
{"type": "agent_created", "agent_id": "agent-1", "name": "Agent 1"}
{"type": "agent_list", "agents": [{"agent_id": "agent-1", "name": "Agent 1"}]}
{"type": "agent_removed", "agent_id": "agent-1"}
{"type": "agent_error", "agent_id": "agent-1", "message": "..."}
```

**Extended existing messages (add `agent_id`):**
```json
{"type": "chunk", "agent_id": "agent-1", "text": "..."}
{"type": "done", "agent_id": "agent-1", "stop_reason": "end_turn"}
{"type": "tool_call", "agent_id": "agent-1", ...}
{"type": "terminal_started", "agent_id": "agent-1", ...}
{"type": "terminal_output", "agent_id": "agent-1", ...}
{"type": "terminal_done", "agent_id": "agent-1", ...}
{"type": "permission_request", "agent_id": "agent-1", ...}
```

#### 3. Event Routing

Route `AcpEvent` notifications to the correct agent by matching `session_id`.
Each agent's event stream is tagged with its `agent_id` before forwarding to
the frontend.

#### 4. Concurrent Prompts

Replace the single `prompt_lock` with per-agent locks. Multiple agents can
process prompts concurrently while each individual agent remains sequential.

### macOS App Changes

#### Models

- Add an `Agent` model: `id`, `name`, `status`, `timeline`.
- Move `timeline` from `ChatViewModel` to per-agent state.

#### ViewModel

- `ChatViewModel` manages a list of agents and a `selectedAgentID`.
- Agent creation/removal sends protocol messages to the engine.
- Incoming events are routed to the correct agent by `agent_id`.
- `draft` and composer state remain global (shared input for whichever agent
  is selected).

#### Views

- Replace the top-level `VStack` in `ContentView` with `NavigationSplitView`.
- Sidebar column: list of agents with selection binding.
- Detail column: existing chat view (header, message list, composer) scoped
  to the selected agent.
- Add a button to create new agents.

## Implementation Order

### Phase A: Engine Multi-Agent Support

1. Define the `Agent` struct and agent registry in the engine.
2. Add `create_agent` / `list_agents` / `remove_agent` message handling.
3. Add `agent_id` to all frontend protocol messages.
4. Route events and prompts through the agent registry.
5. Test with the CLI harness: create two agents, prompt each independently.

### Phase B: macOS App Sidebar & Multi-Agent UI

6. Add `Agent` model and per-agent timeline state.
7. Update `ChatViewModel` to manage multiple agents.
8. Update `EngineClient` to parse/send `agent_id` in messages.
9. Replace `ContentView` layout with `NavigationSplitView`.
10. Build sidebar with agent list, selection, and "new agent" button.
11. Scope chat view to selected agent.

### Phase C: Integration

12. End-to-end test: launch app, create two agents via sidebar, send prompts
    to each, verify independent streaming responses.

## Explicitly Out of Scope

- Agent configuration (model selection, system prompts, permissions mode).
- Agent naming/renaming UI.
- Persisting agents across app restarts.
- Drag-and-drop reordering in sidebar.
- Agent-to-agent coordination.
- Removing/stopping agents (can defer to a follow-up if needed).

## Risks

- **ACP adapter resource usage**: Each agent spawns its own ACP adapter
  subprocess and Claude session. Multiple concurrent agents will use more
  memory and potentially hit API rate limits.
- **NavigationSplitView quirks**: SwiftUI's `NavigationSplitView` can have
  subtle behavior differences across macOS versions. Target is macOS 15 which
  should have stable behavior.
