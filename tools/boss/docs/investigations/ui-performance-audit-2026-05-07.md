# Boss macOS UI performance audit — 2026-05-07

Static audit of `tools/boss/app-macos/Sources/` for the reported "UI gets sluggish after long sessions" symptom.

## Scope and methodology

- **Static-only repro.** No live `BossMacApp` was running on the host at audit time, so no Instruments / `leaks` / `heap` evidence could be gathered. Severities below are reasoned from the code, the per-event-rate of each suspect, and the expected accumulation curve over a multi-hour session.
- **Surfaces audited:** every `*.swift` file under `tools/boss/app-macos/Sources/` (16 files, 7104 LoC) plus the design docs that describe how the UI is wired (`worker-live-status.md`, `work-subscriptions.md`, `engine-app-rpc.md`).
- **Process model recap (relevant to findings).** `EngineClient` runs its socket I/O and JSON parsing on a private dispatch queue (`Boss.EngineClient`); each parsed event is then hopped to the MainActor via `Task { @MainActor in onEvent(event) }` before it mutates `ChatViewModel`. Each engine event therefore costs one MainActor task per event, plus all downstream SwiftUI invalidation.
- **Repro hint for whoever lands the fixes.** Run with `BOSS_SHOW_SYSTEM_MESSAGES=1`, open the Work board with one Doing card, and watch RSS via `ps -o rss= -p $(pgrep -f BossMacApp)` over 30+ minutes while a worker streams output. The two surfaces most likely to dominate growth are (a) the libghostty surface leak (#1), measured per spawn/release cycle, and (b) the unbounded `agents[].timeline` array (#6), measured against the chunk rate of any active Claude session.

## Findings

### 1. libghostty surface is never freed when a worker pane is torn down — leak per pane spawn/release **(HIGH)**

`GhosttyTerminalHostView` allocates a `ghostty_surface_t` in `init` (`tools/boss/app-macos/Sources/Ghostty/GhosttyTerminalView.swift:91`) and stores it in `self.surface`. The host view's `deinit` (`tools/boss/app-macos/Sources/Ghostty/GhosttyTerminalView.swift:120-122`) only cancels `pendingGeometrySync`. There is no `ghostty_surface_free`/`ghostty_surface_close` call anywhere in `Sources/`:

```
$ grep -rn "ghostty_surface_free\|ghostty_surface_close" tools/boss/app-macos/Sources/
tools/boss/app-macos/Sources/Ghostty/GhosttyTerminalView.swift:406:        defer { ghostty_surface_free_text(surface, &text) }
```

(That single hit is `ghostty_surface_free_text`, the read-text helper — not surface lifecycle.)

`WorkersWorkspaceModel.releaseWorkerPane` (`tools/boss/app-macos/Sources/Ghostty/WorkersWorkspaceModel.swift:54-69`) clears `slots[index].session = nil`, which lets SwiftUI tear down `GhosttyTerminalView` → `GhosttyTerminalHostView`. The Swift wrapper goes away; the C-level surface (PTY, scrollback, GPU resources) remains allocated forever.

**Impact.** Every completed worker run leaks one libghostty surface plus its scrollback buffer plus its PTY file descriptors. Over a long session that runs many work items through the 8-slot pool, this grows linearly with the number of completions. This is exactly the shape of "UI gets sluggish after long sessions": fd table grows, GPU resources accumulate, scrollback memory accrues.

Note: the singleton `GhosttyRuntime.shared` is intentionally never deallocated, so leaving the runtime alive is correct. The leak is the per-surface lifecycle.

### 2. Per-pane 0.5 s "Claude monitor" timer doing main-thread viewport scraping for every live pane **(HIGH)**

`startClaudeMonitor()` (`tools/boss/app-macos/Sources/Ghostty/GhosttyTerminalView.swift:363-372`) installs a 0.5 s repeating `Timer.scheduledTimer` per host view. Each tick:
- calls `ghostty_surface_read_text` for the entire visible viewport (`GhosttyTerminalView.swift:384-408`),
- builds a fresh Swift `String` from the C buffer,
- runs `localizedCaseInsensitiveContains("esc to interrupt")` and three `String.contains(...)` checks,
- splits the whole viewport on newlines, filters empties, takes the last 24 lines,
- reverse-iterates lines looking for the `❯` prompt prefix,
- assigns `claudeState` on `TerminalPaneSession` (an `ObservableObject` `@Published`).

This is the *only* work in `GhosttyTerminalHostView` that runs on the main actor on a regular timer. With 8 worker panes plus the Boss pane all visible, the app does ~18 viewport scrapes per second, every second, for the entire lifetime of the app, on the main thread. The `ClaudeMonitorTracker` debounce (`TerminalPaneSession.swift:57-133`) is cheap, but the read + string work is not.

`worker-live-status.md` (PR #232) explicitly intends `claudeState` to become a *fallback*, with the engine-supplied `LiveWorkerState` as the authoritative signal — `WorkersDetailView.swift:131-142` already prefers `liveState` and only falls back to `claudeState` "until the worker's first hook fires". The screen-scrape is therefore mostly redundant in steady state, but it keeps running at 2 Hz per pane regardless.

**Impact.** Constant main-thread cost that scales with active pane count; scales further if Ghostty's text read is O(viewport) which it likely is. Doesn't grow unbounded over time, but is a continuous baseline drag that gets worse as more workers spawn.

### 3. `TrekIconAssets.image` re-loads PNGs from disk on every render **(HIGH)**

`TrekIconAssets.image` (`tools/boss/app-macos/Sources/TrekIconAssets.swift:62-72`) does:

```swift
if let url = Bundle.module.url(forResource: resource, withExtension: "png"),
   let image = NSImage(contentsOf: url) {
    return image
}
```

There is no cache. Every call re-resolves the bundle URL and re-decodes the PNG. The function is called from view `body` in:

- `WorkBoardCardView.body` for the per-card crew portrait — `ContentView.swift:937-944`. Every Doing card re-loads its PNG on every kanban re-render.
- `WorkerSlotView.slotHeader` for the slot titlebar portrait — `WorkersDetailView.swift:111-117`. Every slot re-loads on every workspace tick.
- `WorkerSlotView.idlePaneView` for the large idle portrait — `WorkersDetailView.swift:77-83`. Re-loaded whenever the idle view re-renders.
- `bossAgentHeader` for the Picard avatar — `ContentView.swift:533-546`. Re-loaded on every `ContentView` re-render.

Kanban re-renders fire every time `worker.live_states` pushes (see #4) and every time `workTree` is refreshed. With ~10 Doing cards plus 8 slots plus the Boss header, a single push triggers ~19 PNG decodes.

**Impact.** Each decode is small (a few hundred KB PNG), but the I/O is on the main thread and happens dozens of times per second when the engine is busy. NSImage caching the underlying CGImage is `Bundle.module`-relative, so reads do hit the disk cache, but the file-URL lookup, NSData read, and decode-on-first-draw all run on main.

### 4. `worker.live_states` topic pushes invalidate the entire `ContentView` subtree **(MEDIUM)**

`workerLiveStatesByRunID` and `workerLiveStatesBySlot` are `@Published` on `ChatViewModel` (`ChatViewModel.swift:41,44`). On every `worker_live_states_list` event from the engine the handler reassigns *both dictionaries* by rebuilding from scratch (`ChatViewModel.swift:820-826`):

```swift
workerLiveStatesByRunID = Dictionary(uniqueKeysWithValues: states.map { ($0.runId, $0) })
workerLiveStatesBySlot  = Dictionary(uniqueKeysWithValues: states.map { ($0.slotId, $0) })
```

Two `@Published` writes → two `objectWillChange` fires. Every `View` that observes `model: ChatViewModel` re-evaluates its `body` — that's `ContentView` (the root) and everything it transitively contains, including the kanban, the Boss panel, the toolbar pickers, etc.

Per `worker-live-status.md`, the engine emits this push "whenever any slot changes" — so during active work this fires on hook events, tool calls, prompt-submit transitions, etc., at the cadence of the Claude Code hook stream.

**Impact.** Every hook event causes a full SwiftUI invalidation across the app's view tree. SwiftUI's diff is fast, but #3 (PNG reload) and #7 (kanban recompute) compound on it.

**Note on the published shape.** Splitting these out so kanban-only views observe a slim object (just the by-run mapping needed for `AgentActivityState`) and pane-row views observe by-slot is the correct refactor. Today both are on the same `ChatViewModel`, so any consumer of either map invalidates everyone.

### 5. Boss assistant chunks accumulate in `agents[].timeline` but nothing renders that timeline **(MEDIUM)**

`appendAssistantChunk` (`ChatViewModel.swift:1086-1101`) and `appendTerminalOutput` (`ChatViewModel.swift:1121-1131`) keep extending `agents[i].timeline`. The Boss pane has been moved to a libghostty surface (`BossPaneTerminalView`, `ContentView.swift:472-486`) — the `messageList` helper that would have rendered the chat timeline is defined at `ContentView.swift:359` but **never called** anywhere in the file:

```
$ grep -n "messageList" tools/boss/app-macos/Sources/ContentView.swift
359:    private func messageList(items: [TranscriptItem], emptyState: String? = nil) -> some View {
```

So every `chunk` event from the engine still hits `appendAssistantChunk`, mutates `agents[bossIdx].timeline`, fires `objectWillChange` on `ChatViewModel`, invalidates the whole `ContentView` subtree (#4), and stores the chunk in a buffer that no view ever reads. The same pattern applies to `terminalStarted` / `terminalOutput` / `terminalDone`: `agents[i].timeline` and `agents[i].terminalEntryIndexByID` grow unboundedly during a session.

`maxTerminalOutputChars = 200_000` (`ChatViewModel.swift:159, 1126-1129`) caps the *contents* of any one terminal entry, but does not cap the *number* of entries. A long-running session that makes many tool calls accumulates indefinitely many entries.

**Impact.** Twofold:

1. Every chunk fires a no-op publisher update that re-renders `ContentView`. With Claude streaming at typical chunk rates, this is the most frequent source of `body` re-evaluation in the app.
2. The `timeline` array and `terminalEntryIndexByID` dictionary grow linearly with session length. RSS growth from this alone could plausibly reach hundreds of MB after a long Boss session.

### 6. `ChatViewModel.visibleWorkItems` / `workItems(in:)` / `workSections(in:)` recompute from scratch on every render **(MEDIUM)**

`workSections(in:)` (`ChatViewModel.swift:1224-1241`) calls `workItems(in:)` (`ChatViewModel.swift:1218-1222`), which calls `visibleWorkItems` (`ChatViewModel.swift:112-142`). `visibleWorkItems` does:

- iterate `projectsForSelectedProduct` (sort-on-read of `projectsByProductID[productID]`),
- accumulate into a fresh `[WorkTask]`,
- append all chores (sort-on-read),
- filter on `showBlockedOnly`,
- if the search box is non-empty, do four `localizedCaseInsensitiveContains` calls per item.

Then `workItems(in:)` filters by column and sorts the result. `workColumn` (`ContentView.swift:632-684`) calls `workSections(in: column)` — once per column — every time `body` runs, which is every time *any* `@Published` property on `ChatViewModel` fires (so every push from #4, every chunk from #5, etc.).

For a single `worker.live_states` push: 4 columns × full `visibleWorkItems` walk = 4 × O(N log N) of all tasks plus chores. With dozens of cards across columns, this is a non-trivial main-thread cost on every event.

**Impact.** Multiplicative with #4 and #5 — every event burns 4× the work on the kanban alone.

### 7. `Agent.timeline` items use UUID/`String` ids; `ForEach` is fine, but `terminalEntryIndexByID` dict only ever grows **(LOW–MEDIUM)**

Tied to #5 — the dictionary on `Agent` (`Models.swift:34`) maps every terminal-entry id seen in the session, never evicted. Each entry is small but it's a per-agent map that grows over the life of the agent. Worth bounding when #5 is fixed.

### 8. SwiftUI assigns the entire `agents` array on every chunk **(LOW–MEDIUM)**

`appendAssistantChunk` reassigns elements of `agents[agentIdx].timeline` and rewrites the timeline entry (`ChatViewModel.swift:1086-1101`). Because `Agent` is a value type and `agents` is an `@Published [Agent]`, each reassignment of a sub-element triggers a publisher update on the whole array. SwiftUI doesn't see this as an array-shape change, but every observer invalidates. This is a structural consequence of using `[Agent]` as a value-type collection and feeding it streaming text. With the Boss pane no longer rendering this timeline (#5), the simplest fix is to stop accumulating chunks in `agents[].timeline` for the Boss agent at all.

### 9. `EngineClient` `Task { @MainActor in onEvent?(event) }` per parsed line **(LOW)**

`EngineClient.emit` (`tools/boss/app-macos/Sources/EngineClient.swift:685-689`) creates a fresh `Task` per event to hop onto MainActor. JSON decode is correctly off main, but every event allocates a Task. With high-frequency `chunk` and `worker.live_states` traffic this is dozens of Tasks per second. Each Task is cheap (a few hundred bytes) but it's continuous allocator pressure on the GCD queue.

**Impact.** Low on its own, but combined with #4 and #5 it forms part of the per-event tax. Could be batched via `MainActor.assumeIsolated` when the queue is already on main, or by collapsing high-frequency chunks before the hop.

## Surfaces audited and ruled clean (so nobody re-digs)

- **JSON decode is off main.** `EngineClient.consumeLines` runs on `Boss.EngineClient` queue; the JSON parse uses `JSONSerialization.jsonObject` and runs *before* the MainActor hop. No JSON decode happens on the main actor (`EngineClient.swift:479-683`). This is correct.
- **Reconnect logic is bounded.** `EngineClient.scheduleReconnect` checks `shouldReconnect` and `connection == nil` before scheduling, and the cancellation path nils `connection` correctly (`EngineClient.swift:691-702`). No double-scheduling, no orphan reconnect tasks.
- **`NotificationCenter` observers are tracked and removed.** `GhosttyRuntime.installObservers` stores tokens in `observers`, and `deinit` removes them all (`GhosttyRuntime.swift:101-107, 113-135`). The runtime is a singleton so `deinit` never fires in practice, but the bookkeeping is correct — no observer leak path.
- **`pendingGeometrySync` cancellation is correct.** `GhosttyTerminalHostView.deinit` cancels the pending coalesced reflow (`GhosttyTerminalView.swift:120-122`). The 30 Hz throttle on `ghostty_surface_set_size` is correct — drag-resize won't reflow scrollback at frame rate.
- **`claudeMonitorTimer` lifecycle is correct.** Started in `init` and `viewDidMoveToWindow(window!=nil)`, invalidated when `window == nil` (`GhosttyTerminalView.swift:142-154, 363-372`). Timer holds `[weak self]`. No leak path. (Cost is the issue — see #2.)
- **`receiveNext` uses `[weak self]`.** No retain cycle on the NWConnection callback chain (`EngineClient.swift:450-477`).
- **`PrDetector` is engine-side (Rust).** No Swift counterpart in `Sources/`. Out of scope per the audit's Swift-only constraint.
- **Action callbacks from libghostty are now sync.** `GhosttyRuntime.action` uses `MainActor.assumeIsolated` synchronously to handle actions on the calling thread; the comment at `GhosttyRuntime.swift:167-192` documents why (PR #209's use-after-free fix). No analogous async-dispatch shape elsewhere — `GhosttyRuntime.wakeup` and `closeSurface` use `OperationQueue.main.addOperation` but neither dereferences a transient pointer, so they're fine.
- **Engine process controller has no observers.** `EngineProcessController` does not subscribe to anything; `stop()` only kills the engine PID. No leak surface.

## Recommended follow-up chores

Each one bite-sized and independently mergeable. Severity in `[ ]`.

1. **[HIGH] Free libghostty surface on host-view teardown.** Add `ghostty_surface_free(surface)` (or whatever upstream's surface destructor is — see `GhosttyKit` headers; possibly `ghostty_surface_close` then `ghostty_surface_free`) to `GhosttyTerminalHostView.deinit`. Acceptance: `leaks $(pgrep BossMacApp)` after a spawn/release cycle shows no growing `ghostty_surface_t` allocations; RSS does not creep after repeated `bossctl agents launch`/`agents stop` cycles. Verify via PR #209's surface lifecycle docs and upstream Ghostty.app's `Ghostty.SurfaceView` for the canonical free order (focus + cursor cleanup before free).

2. **[HIGH] Drop the per-pane 0.5 s screen-scrape monitor when LiveWorkerState is the source of truth.** Now that `worker-live-status.md` lands hook-driven activity, `claudeMonitorTimer` is a fallback for "before first hook". Either (a) gate it on `slot.session.hostView == nil || liveState == nil` and stop the timer once we have a `LiveWorkerState`, or (b) cut it entirely and let the pill say "Spawning" until the engine sends activity. Acceptance: with 8 active workers, `sample BossMacApp` shows no main-thread time in `readVisibleContents` / `extractTail`.

3. **[HIGH] Cache `TrekIconAssets.image` results.** Wrap loads in a `static var cache: [String: NSImage] = [:]` keyed on `"\(character.rawValue)-\(size.rawValue)"`. NSImages are immutable here — single shared instance is safe. Acceptance: a `print(#function)` on the slow path fires once per (character, size) pair across the lifetime of the app.

4. **[MEDIUM] Stop accumulating Boss assistant chunks into `agents[].timeline` now that the Boss pane is libghostty.** Either drop the `appendAssistantChunk` / terminal-activity accumulation for the Boss agent specifically, or remove the `messageList` dead code path entirely and stop accumulating for all agents (workers don't render their timeline either; they're libghostty panes). Acceptance: a 1-hour Boss session keeps `agents[bossIdx].timeline.count` bounded; RSS growth attributable to `Agent.timeline` is zero.

5. **[MEDIUM] Memoize / split the `worker.live_states` published surface.** Split `workerLiveStatesByRunID` and `workerLiveStatesBySlot` into a child `ObservableObject` (e.g. `LiveWorkerStateStore`), or move them off `@Published` and emit a slimmer `ObservableObjectPublisher` that only fires when the *set of slot ids* or per-slot `activity` changes (not every payload reassignment). Acceptance: a `worker.live_states` push that doesn't change any slot's activity does not invalidate `ContentView.body`.

6. **[MEDIUM] Cache `visibleWorkItems` and per-column `workSections`.** Compute once per `workTree` / filter / search-text change, not per `body`. A `private var cachedVisibleItems: [WorkTask]` invalidated on any input mutation is enough; or precompute a `[WorkBoardColumnKey: [WorkBoardSection]]` map. Acceptance: scrolling the kanban with no engine traffic produces zero calls into `visibleWorkItems`.

7. **[MEDIUM] Bound or evict `Agent.terminalEntryIndexByID` and `Agent.timeline`.** Tied to #4 — once the Boss timeline isn't accumulated, this is mainly a worker concern, but workers also accumulate. Cap to the last N entries (e.g. 200) with FIFO eviction; rebuild `terminalEntryIndexByID` on eviction. Acceptance: a long worker session keeps `Agent.timeline.count <= 200`.

8. **[LOW] Coalesce `EngineClient.emit` MainActor hops for chunk-shaped events.** Either `MainActor.assumeIsolated` when already on main, or batch consecutive chunks for the same agent before the hop. Acceptance: a stream of 1000 chunks in 1 second creates O(10) MainActor tasks rather than O(1000).

9. **[LOW — safety patch candidate] Audit `DispatchQueue.main.asyncAfter` calls without cancellation.** `TerminalOutputPane.body` (`ContentView.swift:2009-2012`) and `messageList` (`ContentView.swift:386-391`) use unguarded `DispatchQueue.main.async{After}` for scroll-pinning. Today these can fire after the view has gone away; harmless, but a known minor leak shape. Re-write as `Task { @MainActor in try? await Task.sleep(...) }` with cooperative cancellation, or drop the helper if the timeline isn't rendered (#4 already removes one of them).

## Open questions for the next investigator

- **Live RSS curve is unverified.** The audit infers severities from the code; an Instruments / `heap`-vs-time capture across a 30+ minute session would let us rank #1 vs. #5 by actual MB-per-minute. Recommend doing this during fix-validation rather than as a precondition.
- **Is there an upstream Ghostty.app teardown order we should mirror?** Best to check `Ghostty.app/Sources/Ghostty/SurfaceView.swift` (or whatever the current upstream filename is) before writing #1's fix — there's likely a focus-clear / cursor-uninstall step that should run before `ghostty_surface_free`.
