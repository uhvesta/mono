# Boss markdown render slowness — 2026-05-18

Investigation of the ~129 s render time Boss reports for the 46 KB editorial-controls design doc (mono#688). PR #686's Textual-only rig already established that **Textual itself is not the bottleneck** — the same doc renders in ~190 ms when driven directly. This document captures the static-analysis findings that point at specific Boss wrappers and the layered bisection rig staged for the next iteration.

## Scope and methodology

- **Static review** of every Boss-side wrapper Textual is rendered inside, plus the Textual sources at the pinned version (gonzalezreal/textual 0.3.1, in `tools/boss/app-macos/.build/checkouts/textual/`).
- **Comparison baseline:** the textual-perf rig from PR #686 (`tools/boss/experiments/textual-perf/`) — same Textual pin, same 46 KB sample, ~190 ms `parse_end`.
- **No live profiler runs** were collected for this writeup; severities are reasoned from the code shape and the difference between the textual-only path and Boss's path. Numbers from the rig in `tools/boss/experiments/textual-perf-layered/` (added in this change) replace these guesses with measurements once a human runs them.
- **What Boss measures as `phase=parse`:** wall clock from `MarkdownViewerScrollContent.onAppear` to the first non-zero height arriving via `StructuredTextHeightKey` (`tools/boss/app-macos/Sources/DesignsView.swift:545-563`). That window covers both the synchronous markdown parse and the first SwiftUI layout pass of the rendered tree. It does *not* include async attachment resolution. So whatever blows up to 129 s is happening in the synchronous parse or the first layout pass of the Boss wrapper tree.

## The wrappers Boss adds on top of `StructuredText`

The async-markdown-viewer flow (the surface that produced the 129 s number) wraps `StructuredText` in this stack, top-down:

1. `AsyncMarkdownViewerView` switches on `vm.state`. On `.loaded`, builds `MarkdownViewerView(...).id(vm.renderContentID).navigationTitle(title).onAppear { … }` (`tools/boss/app-macos/Sources/DesignsView.swift:619-648`).
2. `MarkdownViewerView` wraps `MarkdownViewerScrollContent(...)` in `.withComments()` (`tools/boss/app-macos/Sources/DesignsView.swift:470-478`).
3. `WithCommentsModifier.body` recomputes `commentedTexts = layer.comments.map(\.quotedText).filter { !$0.isEmpty }` every body pass, wraps the content in an `HStack(spacing: 0)`, injects `.environment(\.commentedTexts, ...)` and `.environment(\.commentFlashText, ...)`, overlays an add-comment button, attaches a hidden ⌘⇧K button via `.background { Button("") {...}.keyboardShortcut(...) }`, and installs/removes NSEvent monitors on appear/disappear (`tools/boss/app-macos/Sources/Comments/CommentLayer.swift:290-345`).
4. `MarkdownViewerScrollContent.body` returns `ScrollView { VStack { Title; Divider; StructuredText(source, parser: markdownParser).bossMarkdown().textual.textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading).id(parseVersion).onChange(of: commentedTexts).onChange(of: commentFlashText).background(GeometryReader for height preference) }.padding(.horizontal, 24).padding(.vertical, 20).frame(maxWidth: .infinity, alignment: .leading) }.textSelection(.enabled).onAppear { parseStartTime = Date() }.onPreferenceChange(StructuredTextHeightKey.self) { … }` (`tools/boss/app-macos/Sources/DesignsView.swift:511-564`).
5. `StructuredText.body` then calls `WithAttachments(attributedString) { BlockContent(content: $0).modifier(TextSelectionInteraction()).modifier(TextSelectionCoordination()) }.coordinateSpace(.textContainer).onChange(of: markup, initial: true) { markupDidChange(markup) }.lineLimit(nil)` (`.build/checkouts/textual/Sources/Textual/StructuredText/StructuredText.swift:118-135`).
6. `bossMarkdown()` swaps in `BossStructuredTextStyle()` whose `codeBlockStyle` wraps each code block in `Overflow { … .padding(12) }.background(RoundedRectangle.fill).overlay(RoundedRectangle.stroke)`, and whose `tableStyle` adds a `Canvas { … }` overlay via `.textual.tableOverlay { layout in Canvas { … for divider in layout.dividers() { … } } }` plus a `RoundedRectangle.stroke` overlay (`tools/boss/app-macos/Sources/BossMarkdownStyle.swift:38-116`).

PR #686's textual-only rig collapses 1–5 down to `ScrollView { StructuredText(markdown: source).padding() }` and runs in ~190 ms. The three-orders-of-magnitude slowdown must come from something added above the `StructuredText(...)` line.

## Hypotheses, ranked

### 1. `.withComments()` HStack-wraps the entire ScrollView, plus a recently-changed observable surface **(HIGH)**

`WithCommentsModifier` is the most recently-added wrapper (`169a9db` on 2026-05-15) and is the only wrapper whose body recomputes a non-trivial value on every `@Published` change of an observable model. Two things make it suspicious:

- It re-creates a fresh `[String]` array via `layer.comments.map(\.quotedText).filter { !$0.isEmpty }` on every body pass. `[String]` is `Equatable`, so SwiftUI's environment-update diff *should* suppress propagation when the array is empty, but the value still has to be constructed and compared on every observable change.
- It wraps the inner `ScrollView` inside an `HStack(spacing: 0)` and only conditionally adds a sidebar (`if !layer.comments.isEmpty`). SwiftUI's `HStack` propagates a horizontal layout pass; combined with the inner `ScrollView { VStack { … } }`, the inner content is asked to size to a flexible-width column, which then has to interact with `frame(maxWidth: .infinity)` at two levels (on the `StructuredText` and on its enclosing `VStack`).
- The hidden ⌘⇧K shortcut is installed via `.background { Button("") {...}.keyboardShortcut("k", modifiers: [.command, .shift]).frame(width: 0, height: 0).hidden() }`. That's a real `Button` in the view tree with global keyboard-shortcut wiring; SwiftUI re-evaluates it on every body invalidation.

Layer **L3** in the rig isolates this.

### 2. `bossMarkdown()` table style adds a `Canvas { … layout.dividers() … }` overlay **(MEDIUM)**

`BossTableStyle.makeBody` adds `.textual.tableOverlay { layout in Canvas { context, _ in for divider in layout.dividers() { context.fill(Path(divider), with: .style(...)) } } }` (`tools/boss/app-macos/Sources/BossMarkdownStyle.swift:96-105`). The default Textual style does not add a `Canvas` overlay; it uses a different table style. `Canvas` is redrawn whenever the table layout invalidates. Across multiple tables, layout passes can compound — and the editorial-controls doc has several tables.

Layer **L1** in the rig isolates the Boss style stack as a whole. If L1 alone crosses into seconds, this is the prime suspect inside it.

### 3. Boss inner wrappers nest `frame(maxWidth: .infinity)` against `ScrollView` and `textSelection(.enabled)` at two levels **(MEDIUM)**

`MarkdownViewerScrollContent` applies `.frame(maxWidth: .infinity, alignment: .leading)` to both the `StructuredText` and the enclosing `VStack`, and `.textSelection(.enabled)` on the outer `ScrollView` while *also* applying `.textual.textSelection(.enabled)` on the inner `StructuredText`. The two text-selection modifiers serve different stacks (SwiftUI's native one and Textual's `TextSelectionInteraction` / `TextSelectionCoordination`) and may both stand up the AppKit text-selection plumbing over the same 47 KB body.

Layer **L2** in the rig isolates this.

### 4. `.id(parseVersion)` + `.onChange(of: commentedTexts)` rebuild **(LOW)**

`MarkdownViewerScrollContent` bumps `parseVersion` whenever `commentedTexts` or `commentFlashText` changes, then the `.id(parseVersion)` on `StructuredText` forces SwiftUI to discard the old `StructuredText` and construct a new one from scratch — which triggers a fresh parse. With zero comments the value should never change, so this shouldn't fire on first render. But if there is any spurious environment churn (cf. hypothesis 1), the `.id()` could thrash. This is downstream of hypothesis 1: it can only matter if something upstream is causing the value to change.

The rig captures this indirectly: in L3, the comments stub publishes the same `[String]` values, so any extra rebuilds caused by the `.id` thrash show up there.

### 5. View-model state-machine churn between `.loading` and `.loaded` **(LOW)**

`AsyncMarkdownViewerView` reads `vm.state` directly (not via `@Published` projection) and rebuilds when *any* `vm.objectWillChange` fires — `renderStartTime`, `pendingRenderProjectShortID`, `renderContentID`, `clickStartTime`, *and* `state` all live on the same `@MainActor final class AsyncMarkdownViewerViewModel: ObservableObject` (`tools/boss/app-macos/Sources/DesignsView.swift:585-609`). The `.id(vm.renderContentID)` forces a full inner rebuild whenever the UUID changes, which it does once per content load.

Layer **L4** (without async) and **L5** (with async) isolate this together. If L5 is much slower than L4, the spinner→content transition is doing extra work.

### 6. Async fetch dispatch on `Task.detached` `String(contentsOf:)` **(LOW)**

`ChatViewModel.openProjectDesignDoc` dispatches a `Task.detached` to read the file (`tools/boss/app-macos/Sources/ChatViewModel.swift:1089+`). 47 KB is trivial to read; this should not show up. The async hop *itself* can cause a `.loading` → `.loaded` rebuild as in (5), but the file I/O isn't a bottleneck.

## Bisection plan

`tools/boss/experiments/textual-perf-layered/` re-introduces wrappers 1–4 above on top of `StructuredText`, one per layer, with a picker so a human can click between layers and capture per-layer timing in os.log:

| Layer | What it adds                                  | Expected (if Boss-side stack only) |
|-------|-----------------------------------------------|------------------------------------|
| L0    | none (= PR #686)                              | ~190 ms                            |
| L1    | `.bossMarkdown()`                             | hypothesis 2 lights up here        |
| L2    | Boss inner wrappers (frame, dual text-selection, title, padding) | hypothesis 3 lights up here |
| L3    | `.withComments()` stub                        | hypothesis 1 lights up here        |
| L4    | view-model `.loading` → `.loaded` flip        | hypothesis 5 lights up here        |
| L5    | async fetch + view-model                      | hypothesis 6 lights up here        |

Run:

```sh
cd tools/boss/experiments/textual-perf-layered
swift run textualperflayered
# in another terminal:
log stream --predicate 'subsystem == "com.boss.textualperf"' --level info
```

Each picker click logs `phase=parse_start layer=Ln` and, on first non-zero layout, `phase=parse_end layer=Ln duration_ms=…`. The rig keys its pane by `.id(layer)`, so re-clicking a layer captures a fresh sample.

The expected outcome is that one specific layer crosses from milliseconds into seconds — that's the offender. If two adjacent layers both jump, the wrapper added by the *first* is the cause (the second is just inheriting the cost).

## Open questions

- Does **L1** alone reproduce the slowness, or is it L2 / L3 that crosses? The static review can't distinguish — only the rig measurements can.
- Does the slowness depend on Designs-tab state (e.g. how many products/projects are loaded) or is it intrinsic to the single doc? The rig is standalone, so if it reproduces, the cause is local.
- Is there a feedback loop between `.withComments()` rebuilding and `parseVersion` bumping? The rig's L3 stub has no NSEvent monitors, which means it captures the rebuild path without the keyboard-event path; if Boss is slower than rig-L3, that gap implicates the monitors or something else only present in Boss.
- If none of L1–L5 reproduces the 129 s, then the cause is *not* in any single wrapper — likely an interaction between the wrappers and some app-level environment value Boss injects further up the tree. Next step: bisect by adding Boss's top-level scene wiring (`@EnvironmentObject ChatViewModel`, `.openWindow(...)` plumbing) on top of L5.
