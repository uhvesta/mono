# macOS app modernization audit

Sweep of `tools/boss/app-macos/Sources/` for code that exists only as
a workaround for pre-macOS-15 APIs. PR #361 raised the deployment
floor to macOS 15 and demonstrated the shape of the win: the 56-line
`NSViewRepresentable` it deleted (`SidebarResizeCursorOverlay`)
collapsed to a one-liner using the new `pointerStyle(.frameResize(...))`
modifier. This document is the corresponding sweep for everything
else.

## Deployment target

Source of truth: `tools/boss/app-macos/Package.swift:6` and
`tools/boss/app-macos/BUILD.bazel:29`. Both pin to macOS 15
(`platforms: [.macOS(.v15)]` / `minimum_os_version = "15.0"`). Swift
tools version is `6.2`.

After macOS 15, Apple switched to year-aligned naming, so the *next*
major OS is **macOS 26 (Tahoe, fall 2025)** — there is no macOS 16
or 17. This audit treats "raising the floor" as a binary choice
between macOS 15 (where we are) and macOS 26 (the only later
shipping release). I did not find a payoff that would justify
raising the floor today.

## Findings

Every candidate below is something the current code does in AppKit
or in a hand-rolled workaround, where a SwiftUI modifier or scene
primitive available **at our current macOS 15 floor** would replace
it. Each entry lists the file/symbol pointer, current line count,
proposed modifier, minimum deployment target, expected line count
after, and user-visible behavioural risk.

For each finding I also include a copy-pasteable `boss chore
create` invocation at the end of this document — see
[Queued chores](#queued-chores). They are not auto-queued; see
[A note on chore queueing](#a-note-on-chore-queueing).

### 1. Replace `onHover` + `NSCursor` push/pop on PR links with `.pointerStyle(.link)`

* File / symbol: `tools/boss/app-macos/Sources/ContentView.swift:1683-1689`
  (inside `PRURLLink.body`).
* Current shape (7 lines, the closure plus the `.onHover` modifier):

  ```swift
  .onHover { hovering in
      if hovering {
          NSCursor.pointingHand.push()
      } else {
          NSCursor.pop()
      }
  }
  ```

* Proposed: one-line `.pointerStyle(.link)` modifier on the `Link`
  (or removed entirely if the SwiftUI default for a `Link` already
  flips the pointer once `.buttonStyle(.plain)` is reconciled).
* Minimum deployment target: macOS 15 (`pointerStyle` shipped in 15,
  same modifier the sidebar overlay uses today on line 180).
* Expected line count after: 1 line (or 0).
* Risk: very low. `.buttonStyle(.plain)` may need to be re-checked —
  it should not suppress `pointerStyle`, but PR #361's experience
  with the sidebar suggests verifying on the real app rather than
  trusting parity from the docs.

### 2. Replace `SidebarProductPicker` `NSPopUpButton` bridge with a SwiftUI `Picker`

* File / symbol: `tools/boss/app-macos/Sources/ContentView.swift:818-870`
  (`struct SidebarProductPicker: NSViewRepresentable` plus its
  `Coordinator`). Used by `ContentView.swift:226` (Work sidebar)
  and `DesignsView.swift:240` (Designs sidebar).
* Current shape (53 lines): `NSViewRepresentable` over
  `NSPopUpButton`, with a `Coordinator` that mirrors a `[String]` of
  product ids and translates `selectionDidChange(_:)` into a
  `Binding<String?>` write. Calls
  `setContentCompressionResistancePriority(.defaultLow, for: .horizontal)`
  so the popup shrinks in a narrow sidebar.
* Proposed: thin SwiftUI `Picker(selection:)` with `.pickerStyle(.menu)`
  and a `ForEach` over products, tagging each item by its `id`.
  Apply `.frame(maxWidth: .infinity)` (and likely `.controlSize(.small)`)
  to recover the shrink-on-narrow-sidebar behaviour the explicit
  AppKit compression-resistance call was providing.
* Minimum deployment target: macOS 15 (the SwiftUI primitives are
  much older — macOS 11 — but the sidebar this lives in was already
  validated against macOS 15+ in PR #361).
* Expected line count after: ~10 lines.
* Risk: medium. The bridge predates the deployment-floor bump and
  may have been retained because earlier SwiftUI `Picker(menu)`
  versions wouldn't shrink horizontally inside a `List` row. Both
  call sites (Work sidebar at 220-360 pt column width; Designs
  sidebar at 240-460 pt) must be re-verified at the **minimum**
  column widths after migration. Edit-affordance overlay at
  `ContentView.swift:233-244` depends on the picker padding
  layout — keep `padding(.trailing, 28)` intact.

### 3. Replace `MarkdownViewerWindowController` with a SwiftUI `WindowGroup` + `openWindow`

* File / symbol: `tools/boss/app-macos/Sources/DesignsView.swift:474-509`
  (`final class MarkdownViewerWindowController`), called from
  `ContentView.swift:1435` in the "Read full description"
  affordance.
* Current shape (~35 lines plus a 3-line caller): an
  `NSWindowDelegate`-backed singleton that builds an `NSWindow` +
  `NSHostingView(rootView: MarkdownViewerView)` for each
  invocation, holds the window in `openWindows: [NSWindow]`, and
  drops it on `windowWillClose`. `isReleasedWhenClosed = false` is
  the explicit lifetime control that a SwiftUI `WindowGroup`
  manages automatically.
* Proposed: declare a `WindowGroup("Markdown Viewer", id:
  "markdown-viewer", for: MarkdownViewerArgs.self)` (where
  `MarkdownViewerArgs` is a small `Codable` struct holding
  `{title, markdown}`); the caller switches to
  `@Environment(\.openWindow)` and dispatches
  `openWindow(id: "markdown-viewer", value: args)`. Multiple
  concurrent viewers come for free.
* Minimum deployment target: macOS 14 (the
  `WindowGroup(_:id:for:)` overload that passes a value to the
  scene). Already covered by our macOS 15 floor.
* Expected line count after: ~12 lines (scene declaration) + ~2 lines
  at the call site. Net reduction: ~21 lines.
* Risk: medium. Depends on migrating `BossMacApp` to the SwiftUI
  `App` lifecycle first (see #5) so the `Scene` can be declared.
  If `App` migration is deferred, the workaround is to keep the
  controller; the win only lands after the scene graph is in
  SwiftUI's hands.

### 4. Replace `DispatchEventsWindowController` with a SwiftUI `Window` + `openWindow`

* File / symbol: `tools/boss/app-macos/Sources/DispatchEventsViewer.swift:797-893`
  (`final class DispatchEventsWindowController`), wired to the
  Debug menu in `BossMacApp.swift:99-105` via the obj-c selector
  `toggleDispatchEventsViewer(_:)`.
* Current shape (97 lines): `NSWindowDelegate`-backed singleton
  that builds a single `NSWindow` + `NSHostingView`, persists
  visibility (`UserDefaults` key `boss.dispatchEventsViewer.visible`)
  and frame (`boss.dispatchEventsViewer.frame`) through
  `windowWillClose` / `windowDidMove` / `windowDidResize`, and
  reopens on launch via `restoreIfNeeded()`.
* Proposed: declare a `Window("Dispatch Events", id:
  "dispatch-events")` (single instance, not a `WindowGroup`); use
  `setFrameAutosaveName` equivalent via the scene's automatic
  frame persistence. Replace the explicit
  `UserDefaults`-backed visibility with `@SceneStorage` (or
  simply re-open on launch using
  `.handlesExternalEvents(preferring:allowing:)` + scene-level
  `defaultLaunchBehavior`, macOS 15 has the right primitives).
  Toggle the window via
  `@Environment(\.openWindow)` from a `CommandMenu("Debug")`
  command, replacing the manual `installMainMenu` Debug entry.
* Minimum deployment target: macOS 15 (single-instance `Window`
  scene; SwiftUI `CommandMenu`; scene-level
  `defaultLaunchBehavior`).
* Expected line count after: ~25-30 lines combined (scene + command).
  Net reduction: ~65-70 lines.
* Risk: medium-high. Window-frame persistence needs side-by-side
  validation that the SwiftUI autosave matches the
  `NSStringFromRect`/`NSRectFromString` round-trip the current
  controller does. Restore-on-launch behaviour (only reopens if
  the user had it open at quit) must survive. Depends on
  migrating `BossMacApp` first (#5).

### 5. Replace `BossMacApp` `NSApplicationDelegate` shell with SwiftUI `App` lifecycle

* File / symbol: `tools/boss/app-macos/Sources/BossMacApp.swift:1-111`
  (entire file).
* Current shape (111 lines): hand-rolled `@main` entry that calls
  `NSApplication.shared.run()`, installs a manual `NSMenu` for App
  / Edit / Debug menus, builds a single `NSWindow` hosting
  `ContentView` with `titleVisibility = .hidden`, `toolbarStyle =
  .unified`, and `.fullSizeContentView`, then activates the app
  and calls `DispatchEventsWindowController.shared.restoreIfNeeded()`.
* Proposed: rewrite as `@main struct BossApp: App` with:
  - one `WindowGroup` for the main window
    (`.windowToolbarStyle(.unified)`,
    `.windowResizability(.contentMinSize)`,
    plus the equivalent of `.fullSizeContentView` via
    `.toolbarBackgroundVisibility(.hidden, for: .windowToolbar)`),
  - SwiftUI `Commands` for the Debug menu (one `CommandMenu("Debug")`
    with the Dispatch Events toggle),
  - SwiftUI's built-in `TextEditingCommands` for cut/copy/paste/select-all
    (drops every line of the manual Edit menu);
  - `NSApplicationDelegateAdaptor` only if any non-SwiftUI-able
    AppKit behaviour is genuinely required (probably none — the
    "activate ignoring other apps" can be skipped on a fresh
    `App` launch, and `applicationShouldTerminateAfterLastWindowClosed`
    has a SwiftUI knob).
* Minimum deployment target: macOS 15 (every primitive listed is
  available; the toolbar-style and toolbar-background-visibility
  knobs are the cleanest at macOS 15).
* Expected line count after: ~35-45 lines. Net reduction: ~65-75
  lines.
* Risk: high — this is the app entry point. Title-bar styling
  (`.fullSizeContentView`, hidden title, unified toolbar) must be
  visually pixel-equivalent so the Work sidebar's sidebar-material
  bleed (the comment at `ContentView.swift:189-197` explains this)
  keeps working. Worth gating behind a manual rebuild + screen
  comparison.

### 6. Replace `NativeWorkBoardScrollView` NSScrollView bridge with a SwiftUI `ScrollView`

* File / symbol: `tools/boss/app-macos/Sources/ContentView.swift:2074-2233`
  (`NativeWorkBoardScrollView` plus `WorkBoardScrollView`,
  `FlippedContentView`, `HorizontalOnlyClipView` helpers).
* Current shape (~160 lines): `NSViewRepresentable` over
  `NSScrollView`, with:
  - a `WorkBoardScrollView` subclass exposing an `onLayout`
    callback on every `tile()` so the Coordinator can re-lay out
    its column hosting views (`hostingViews: [NSHostingView<AnyView>]`);
  - a `FlippedContentView` document view (top-left origin);
  - a `HorizontalOnlyClipView` that clamps `origin.y` to zero so
    vertical drift can't hide the column headers;
  - manual frame math for column placement (`columnWidth`,
    `spacing`, `horizontalPadding`) plus a `sync(columns:)` that
    diffs the hosting-view list against the SwiftUI column list.
* Proposed:
  ```swift
  ScrollView(.horizontal, showsIndicators: true) {
      HStack(alignment: .top, spacing: workBoardColumnSpacing) {
          ForEach(columns) { col in
              col.view.frame(width: workBoardColumnWidth)
          }
      }
      .padding(.horizontal, workBoardHorizontalPadding)
      .frame(maxHeight: .infinity, alignment: .top)
  }
  .scrollClipDisabled(false)
  ```
  with each `workColumn(...)` already filling vertical space via
  the existing `.frame(maxHeight: .infinity, alignment: .topLeading)`
  modifier at `ContentView.swift:633`.
* Minimum deployment target: macOS 15 — SwiftUI 14's horizontal
  `ScrollView` is the floor (`scrollClipDisabled` shipped in 14);
  we're already above that. Performance characteristics of nested
  `ScrollView` (per-column vertical lanes inside a single
  horizontal scroll) have improved materially on macOS 14+.
* Expected line count after: ~10 lines. Net reduction: ~150 lines.
* Risk: medium-high. The AppKit bridge is load-bearing for three
  reasons that need explicit verification:
  1. Vertical drift clamping (`HorizontalOnlyClipView` keeps
     `origin.y == 0`). SwiftUI horizontal-only `ScrollView`
     should not generate vertical offset, but rubber-banding on
     trackpad scrolls inside child vertical scrollers needs a
     check.
  2. Column header stability across project/filter changes
     (the current Coordinator re-clamps on every
     `sync(columns:)`).
  3. Drop-target geometry: `dropDestination(for: String.self)`
     on each column at `ContentView.swift:640` must continue to
     hit correctly with the new layout. Drag-and-drop the
     kanban tasks across columns end-to-end before merge.

  PR #357 hard-protects the `Boss/Picard` resize divider but says
  nothing about this board scroll view, so it is queueable. The
  Coordinator pattern was almost certainly defensive against
  earlier SwiftUI versions; macOS 15 should make it
  unnecessary.

## Protected / flagged but not queued

### `ResizeDivider` (Boss/Picard pane divider)

* File / symbol: `tools/boss/app-macos/Sources/ContentView.swift:2236-2377`
  (`ResizeDivider: NSViewRepresentable` + `ResizeDividerView:
  NSView`). 142 lines.
* Why it stays: PR #357 and PR #361 explicitly hold this divider's
  drag behaviour and visible separator styling constant. The
  divider owns three things bundled together — cursor feedback,
  click-and-drag width adjustment, and the 1pt visible separator
  with hover/active tint — and the AppKit bridge is the only
  place that gets all three to compose inside the SwiftUI
  `overlay(alignment: .leading)` host.
* macOS 15 brings `pointerStyle(.frameResize(...))` (used today in
  PR #361 for the sidebar overlay), which trivially replaces the
  *cursor feedback* portion. It does **not** replace the drag math
  or the visible separator tinting. A future simplification might
  split this view into "SwiftUI separator + `pointerStyle` for
  cursor + minimal AppKit drag bridge", but that is a behavioural
  change and explicitly requires human signoff. Flagged here,
  not queued.

### `GhosttyTerminalView` and friends

* File / symbol: `tools/boss/app-macos/Sources/Ghostty/GhosttyTerminalView.swift:14-612`.
* Why it stays: wraps the libghostty C API
  (`ghostty_surface_new`, `ghostty_surface_key`, mouse event
  routing). There is no SwiftUI equivalent — this is a terminal,
  not a control. Out of scope.

## Negative results

I looked for the following candidate shapes and did **not** find
matches that would justify a chore:

- `NSAlert` / `NSSavePanel` / `NSOpenPanel` wrappers: none. All
  alerts use `.alert(...)` and there are no file dialogs.
- `@FocusState`-replaceable manual `becomeFirstResponder` plumbing:
  only inside `GhosttyTerminalView` (out of scope).
- `NSPopover` wrappers: none — `.popover(...)` is used directly
  at `ContentView.swift:927`.
- `onContinuousHover` / hover-effect workarounds: none beyond the
  PR-link case already in #1.
- Manual selection state that `List(selection:)` would replace:
  none — both the Work sidebar and the Designs file tree already
  use SwiftUI `List(selection:)` / `OutlineGroup`.
- Manual keyboard-shortcut plumbing: none beyond the `installMainMenu`
  Edit/Debug menus folded into #5.
- Manual table column resizing or reordering: none — `Table` is
  used directly in `EngineView.swift` and `DispatchEventsViewer.swift`.

## macOS 26 (post-15) opportunities

Considered and discarded for this audit:

* Liquid Glass redesign (`Toolbar(spacer:)`, refined
  `.scrollEdgeEffect`, etc.) is a *visual* refresh, not a code
  simplification. Adopting it would *grow* the diff, not shrink it.
* Nothing in the audit above genuinely requires a macOS-26-only
  modifier to land.

Recommendation: keep the floor at macOS 15 until a concrete
macOS-26-only win surfaces. The wins listed above are all free at
our current floor.

## Aggregate

| # | Floor | Net line delta (estimated) | Risk |
|---|-------|---------------------------:|------|
| 1 | 15    | -6  | low    |
| 2 | 15    | -43 | medium |
| 3 | 15    | -21 | medium (depends on #5) |
| 4 | 15    | -67 | medium-high (depends on #5) |
| 5 | 15    | -70 | high   |
| 6 | 15    | -150 | medium-high |
| **Total** | | **~-357** | |

`ResizeDivider` (142 lines) is excluded from the total because it
is not queued.

## A note on chore queueing

The task asked for these to be queued as chores via `boss chore
create`. The worker-side `.claude/CLAUDE.md` and the operator's
deny-rule make `boss` and `bossctl` coordinator-only inside a
Boss-managed worker run, so this worker cannot issue the
commands itself. The exact invocations are reproduced below for a
coordinator (or a human) to run; each is a single shell line that
reads the audit content above.

The chores intentionally cite each finding's section number so a
later reader of the chore (and of the audit) can re-find the
proposal verbatim. Product slug is `boss` (matches existing macOS
app chores in `git log` such as PR #361 / PR #357).

## Queued chores

```bash
# Finding #1 — PR-link cursor: AppKit push/pop → SwiftUI .pointerStyle(.link)
boss chore create \
  --product boss \
  --name "boss-app: replace PRURLLink NSCursor push/pop with .pointerStyle(.link)" \
  --description "ContentView.swift:1683-1689 inside PRURLLink wraps a SwiftUI Link in an .onHover closure that calls NSCursor.pointingHand.push() / .pop(). At our macOS 15 floor this is one line: .pointerStyle(.link). See docs/designs/macos-modernization-audit.md §1. Expected diff: -6 lines. Risk: very low; verify .buttonStyle(.plain) does not suppress the modifier (PR #361 hit a similar concern with the sidebar overlay)." \
  --priority low \
  --effort trivial

# Finding #2 — SidebarProductPicker: NSPopUpButton bridge → SwiftUI Picker
boss chore create \
  --product boss \
  --name "boss-app: replace SidebarProductPicker NSPopUpButton bridge with SwiftUI Picker" \
  --description "ContentView.swift:818-870 wraps NSPopUpButton in an NSViewRepresentable + Coordinator (53 lines). At our macOS 15 floor a SwiftUI Picker(selection:) with .pickerStyle(.menu) is equivalent. The only AppKit-side concern is the explicit setContentCompressionResistancePriority(.defaultLow, for: .horizontal) — replace with .frame(maxWidth: .infinity) and verify on both call sites: ContentView.swift:226 (Work sidebar, 220-360pt) and DesignsView.swift:240 (Designs sidebar, 240-460pt). See docs/designs/macos-modernization-audit.md §2. Expected diff: -43 lines. Risk: medium." \
  --priority medium \
  --effort small

# Finding #3 — MarkdownViewerWindowController → SwiftUI WindowGroup + openWindow
boss chore create \
  --product boss \
  --name "boss-app: replace MarkdownViewerWindowController with SwiftUI WindowGroup + openWindow" \
  --description "DesignsView.swift:474-509 is an NSWindowDelegate that builds an NSWindow + NSHostingView for every 'Read full description' click and holds the windows in an array. At our macOS 15 floor a WindowGroup(_:id:for:) scene plus @Environment(\\.openWindow) replaces the controller and gets multiple concurrent viewers for free. Depends on Finding #5 landing first so the SwiftUI scene graph owns the app's windows. See docs/designs/macos-modernization-audit.md §3. Expected diff: -21 lines. Risk: medium." \
  --priority low \
  --effort small

# Finding #4 — DispatchEventsWindowController → SwiftUI Window + openWindow
boss chore create \
  --product boss \
  --name "boss-app: replace DispatchEventsWindowController with SwiftUI Window + openWindow" \
  --description "DispatchEventsViewer.swift:797-893 is a 97-line NSWindowDelegate that owns the Dispatch Events window plus its visibility/frame persistence in UserDefaults. At our macOS 15 floor a single-instance Window scene with SwiftUI's built-in frame autosave, @SceneStorage for visibility, and a CommandMenu('Debug') item replaces it. Validate that restore-on-launch behaviour (only reopens if user had it open at quit) survives. Depends on Finding #5. See docs/designs/macos-modernization-audit.md §4. Expected diff: -67 lines. Risk: medium-high." \
  --priority low \
  --effort medium

# Finding #5 — BossMacApp NSApplicationDelegate → SwiftUI App lifecycle
boss chore create \
  --product boss \
  --name "boss-app: rewrite BossMacApp as a SwiftUI App, drop the manual NSMenu / NSWindow" \
  --description "BossMacApp.swift:1-111 is the @main entry point: it calls NSApplication.shared.run(), installs a hand-built NSMenu for App/Edit/Debug, and constructs a single NSWindow hosting ContentView with .fullSizeContentView + hidden title + unified toolbar. At our macOS 15 floor a SwiftUI App with a WindowGroup, Commands { CommandMenu('Debug') { ... } }, and the built-in TextEditingCommands replaces the entire file. Title-bar styling (sidebar-material bleed referenced at ContentView.swift:189-197) must be pixel-equivalent after the rewrite. Prerequisite for Findings #3 and #4. See docs/designs/macos-modernization-audit.md §5. Expected diff: -70 lines. Risk: high (load-bearing app entry)." \
  --priority medium \
  --effort medium

# Finding #6 — NativeWorkBoardScrollView NSScrollView bridge → SwiftUI ScrollView
boss chore create \
  --product boss \
  --name "boss-app: replace NativeWorkBoardScrollView with a SwiftUI horizontal ScrollView" \
  --description "ContentView.swift:2074-2233 is ~160 lines of NSScrollView bridging plus three NSView subclasses (WorkBoardScrollView, FlippedContentView, HorizontalOnlyClipView) that manually lay out column hosting views and clamp vertical scroll drift to zero. At our macOS 15 floor a ScrollView(.horizontal) { HStack { columns } } with .frame(maxHeight: .infinity) on each column does the same job. Verify: (a) trackpad rubber-banding does not introduce vertical offset that hides headers, (b) dropDestination drop-target geometry at ContentView.swift:640 still hits correctly, (c) column header stability across project/filter changes. PR #357 does NOT cover this bridge (it covers the Boss/Picard divider only), so it is queueable. See docs/designs/macos-modernization-audit.md §6. Expected diff: -150 lines. Risk: medium-high." \
  --priority medium \
  --effort medium
```

Six chores total. Findings are independently shippable except for the
dependency edges noted in #3 and #4 (which block on #5). The
`ResizeDivider` simplification is intentionally **not** queued — it
needs an explicit human decision per the PR #357 / PR #361 hard
constraint.
