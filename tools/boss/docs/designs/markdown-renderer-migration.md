# Markdown renderer migration

Design doc for replacing the hand-rolled markdown parser in the Boss macOS
app with a library-backed renderer.

## Why now

Markdown rendering is becoming a load-bearing piece of the Boss UI. Today
it's task / chore descriptions and the Designs file viewer. Soon it will
be an editing surface for those descriptions. The current path is a
hand-rolled block parser plus Foundation's inline-only
`AttributedString(markdown:)` — fine for the simple text we started with,
but it's already showing seams (no nested lists, no inline formatting
inside table cells beyond a one-paragraph parse, lossy code-block fences,
no link affordances, no syntax highlighting, no GFM extensions). Picking a
real renderer now sets the foundation before the editor work compounds on
top of the wrong abstraction.

## Current state

### The hand-rolled parser

It all lives in `tools/boss/app-macos/Sources/DesignsView.swift`
(`MarkdownParser`, `MarkdownBlock`, `MarkdownBlockView`, `MarkdownInline`,
`MarkdownTableView`, `CodeBlockView`, `MarkdownBodyView`,
`MarkdownViewerView`, `MarkdownViewerWindowController`).

What it supports:

- ATX headings `#`–`######`
- Paragraphs (lines joined with a space, blank-line separated)
- Fenced code blocks (` ``` ` or `~~~`, with optional language tag — stored
  but not used)
- Unordered lists (`- `, `* `, `+ `; one level, no nesting)
- Ordered lists (`N.` / `N)`; one level, no nesting)
- Blockquotes (collapsed to a single paragraph)
- Thematic breaks (`---`, `***`, `___`)
- GFM-style pipe tables with a separator row
- Inline formatting via `AttributedString(markdown:source, options:
  .inlineOnlyPreservingWhitespace)` — emphasis, strong, inline code,
  links resolve to attributed runs but SwiftUI's `Text` renders only a
  subset of those attributes (no real link affordance, no underline on
  hover, no theming hook).

What it stumbles on:

- Nested lists (flattened into one level)
- Multi-line list items (subsequent indented lines aren't picked up)
- Setext (underline) headings
- HTML blocks / inline HTML
- Reference-style links and image syntax (images don't render at all)
- Task list checkboxes (`- [ ]`)
- Strikethrough (~~ ~~) and other GFM inline features beyond what
  Foundation parses
- Hard line breaks (two-space EOL collapsed by paragraph buffering)
- Code blocks: no syntax highlighting; language hint is parsed and
  discarded
- Tables: a cell is a one-paragraph `AttributedString` — fine for bold /
  italic, but loses anything else; alignment from the separator row is
  ignored

### Render surfaces today

1. **Designs file viewer** — `MarkdownDocumentView` in
   `DesignsView.swift:370`. Loads a `.md` file from a product repo,
   renders inside a scroll view in the Designs tab. The biggest, most
   forgiving surface — content is long-form, authors are humans, and
   readers expect a github-ish rendering.
2. **Task / chore "Read full description" window** —
   `MarkdownViewerWindowController.present(title:markdown:)` opens an
   `NSWindow` hosting `MarkdownViewerView` (which wraps
   `MarkdownBodyView`). Invoked from the work-card popover in
   `ContentView.swift:1244`.
3. **Work-card popover preview** — `ContentView.swift:1233`
   (`descriptionSummary`). Currently renders the first paragraph as
   plain `Text`, not through the markdown parser at all. A natural
   future use site for inline-only rendering once we have the library.
4. **(Future) editing surface** — the user has flagged that an editor
   for task / chore descriptions is on the roadmap. Not a current
   render surface, but the library choice has to leave room for it.

### Previous renderer in history

Boss used [`gonzalezreal/textual`](https://github.com/gonzalezreal/textual)
v0.3.1 between Feb 2026 (commit `vxovpzpvxqlz` — "feat(boss): render
assistant messages with markdown using Textual") and May 2026 (commit
`pkowsyoqwvrz` — "chore(boss-mac): drop unused Textual SwiftPM
dependency"). The dependency was dropped because its only consumer
(`StructuredText` inside `MessageBubble.assistantText`) disappeared when
the native chat panel was replaced with a libghostty pane in Phase 7a
(`pluyuutsoqsv` / PR #163). The library itself was not the reason it
left — it was a clean-up of an unused dep, and the bazel
`swift_deps.from_package` integration that wired Textual into the build
was already proven to work.

## Library survey

### Candidates evaluated

| Library | What it is | Renders SwiftUI? | GFM | Code highlighting | Theming | Async images | Maintained |
|---|---|---|---|---|---|---|---|
| `gonzalezreal/textual` | SwiftUI text engine, Markdown via Foundation's `AttributedString` + `PresentationIntent`, custom `MarkupParser` protocol | yes | tables, strikethrough, task lists, blockquotes, code blocks | built-in, themeable | full style protocol (`.default`, `.gitHub`, custom) | yes, with `AttachmentLoader` | yes (gonzalezreal, swift-package-index) |
| `gonzalezreal/swift-markdown-ui` (MarkdownUI) | SwiftUI-native CommonMark + GFM renderer, predecessor to Textual | yes | full GFM, task lists | via `CodeSyntaxHighlighter` protocol, no built-in | block-level theme protocol (`MarkdownStyle`, `Theme`) | yes | maintenance mode — author redirects to Textual |
| Apple `swift-markdown` | Parser only, produces an AST (`Document` of `Markup` nodes); no rendering | no | full CommonMark + tables | n/a | n/a | n/a | yes (apple) |
| `iwasrobbed/Down` | Swift wrapper around `cmark-gfm`; outputs `NSAttributedString` / HTML | no (AppKit/UIKit only) | full GFM | no | limited (`DownStyler` for fonts/colors) | no | low activity since 2023 |
| `JohnSundell/Ink` | Pure-Swift CommonMark-ish renderer → HTML string | no | partial | no | no | no | low activity, server-flavoured |

### Recommendation: `gonzalezreal/textual`

It is the best fit on every axis that matters here, and the project
already has receipts that it integrates cleanly:

- **SwiftUI-native.** Renders through `Text` and the SwiftUI layout
  system; selection, accessibility, dynamic type, and dark mode work the
  way the rest of the app already works. No `NSAttributedString`
  bridging or hosting-view rendering glue.
- **Block-level theming protocol.** `StructuredText.Style` and the
  per-block style protocols (`HeadingStyle`, `CodeBlockStyle`,
  `BlockQuoteStyle`, …) let us match the existing Boss visual language
  cleanly without monkey-patching the library. `.gitHub` is a reasonable
  baseline we can override piece by piece.
- **GFM coverage.** Tables (with alignment), strikethrough, task lists,
  fenced code with a language tag, blockquotes — all in the box. The
  parser is Foundation's `AttributedString` markdown parser, so it
  inherits the same parser behavior we already lean on for the inline
  case.
- **Code highlighting.** Built in, themeable, scrollable code blocks with
  their own selection context. Eliminates a whole category of follow-up
  work.
- **Editor-friendly architecture.** The `MarkupParser` protocol means
  the parser sits behind an interface that produces `AttributedString`
  with `PresentationIntent` runs. That's the same representation
  SwiftUI's `TextEditor` and Foundation produce, which keeps the door
  open for round-tripping in an editor (rendered preview + source edit,
  or attributed-edit with a parser hooked to a `TextEditor`-shaped
  surface). We do not have to commit to an editor design today — but
  this choice does not paint us into a corner.
- **Async images.** `AttachmentLoader` handles remote images out of the
  box, which we will want for Designs-tab files that reference repo
  screenshots.
- **Track record in this codebase.** It built under bazel via
  `rules_swift_package_manager` (`swift_deps.from_package` + a
  `@swiftpkg_textual//:Textual` dep target) and shipped in a release.
  Re-adding it is restoring known-good wiring, not greenfield.
- **License.** MIT.

### Why not the others

- **MarkdownUI (`swift-markdown-ui`).** Same author, explicitly
  superseded by Textual ("the spiritual successor … reimagined from the
  ground up"). Picking it would mean adopting the older API of a
  library the maintainer has soft-deprecated.
- **Apple `swift-markdown`.** Parser only — it gets us an AST but
  nothing to render with. Useful as a building block (and Textual could
  in principle be swapped onto it via the `MarkupParser` protocol if
  Foundation's parser ever falls short), but on its own it just shifts
  the rendering problem back to us, which is exactly the position
  we're trying to leave.
- **Down.** AppKit/UIKit `NSAttributedString` output. Using it from
  SwiftUI means hosting an `NSTextView` per render surface, which is a
  step backwards from where we are with `Text`-backed rendering
  (selection, accessibility, dynamic type are all things we'd have to
  fix by hand). No syntax highlighting, no SwiftUI theming. Low recent
  activity.
- **Ink.** Outputs HTML — wrong shape entirely. Useful for static-site
  generators, not a SwiftUI app.

## Theming integration plan

Boss's visual language today (sampled from `DesignsView.swift` and the
work-card popover): system font, headings at 26 / 22 / 18 / 16 / 14 pt,
body and callout sized text, code in `.system(.callout, design:
.monospaced)`, code-block surfaces in `Color(nsColor:
.quaternaryLabelColor).opacity(0.18)` with `.separatorColor` borders at
0.5 pt, tables in the same separator-color border, blockquotes prefixed
with a 3 pt `Color.accentColor.opacity(0.6)` rail.

The plan:

1. **Start from Textual's `.gitHub` preset.** It already gets us close
   to a GitHub-flavored block layout, which is what readers expect from
   a `.md` viewer and what the current parser was implicitly aiming at.
2. **Wrap it in a `BossStructuredTextStyle`** that composes the `.gitHub`
   preset with per-block overrides:
   - `HeadingStyle` mapped to the current 26/22/18/16/14/14 scale via
     `fontScale` so dynamic-type still works.
   - `CodeBlockStyle` using the existing background / border / corner
     radius treatment so a code block in a `.md` file matches a code
     block in any other Boss surface today.
   - `BlockQuoteStyle` keeping the accent-color rail and secondary text
     color.
   - `TableStyle` matching the current separator-color border treatment.
   - `InlineStyle.code` keeping the monospaced callout / surface
     treatment for inline code so it doesn't drift from the block style.
3. **Expose it as a single `View.bossMarkdown()` modifier** (or
   `.textual.structuredTextStyle(.boss)`-shaped extension) so every call
   site applies the same theme with one line. This is the seam that
   keeps the design language coherent if Textual's theming surface
   evolves.
4. **Defer link click handling** to a follow-up: SwiftUI's
   `OpenURLAction` is the right hook, but the policy (open in browser?
   confirm? scope to allowlisted schemes?) is out of scope for the
   foundation task.

## Migration ordering

The bias is **smallest blast radius first, no flag unless something
forces one.**

1. **Foundation task: add the dep and the theme shim.** Re-add the
   `gonzalezreal/textual` SwiftPM dependency (Package.swift /
   Package.resolved) and wire the Bazel side
   (`swift_deps.from_package` use_repo + `@swiftpkg_textual//:Textual`
   `swift_library` dep on `boss_mac_app_lib`), pinning a version. Build
   the `BossStructuredTextStyle` (and a `BossInlineTextStyle` if we end
   up needing one). No call-site changes yet. CI build proves the dep
   resolves.
2. **First surface: the "Read full description" viewer window.** It's a
   modal NSWindow, opened only on demand, with no other UI inside it.
   Worst case if the renderer mis-handles something, the user closes
   the window — no impact on the kanban. Swap
   `MarkdownBodyView(source:)` for `StructuredText(markdown:)
   .textual.structuredTextStyle(.boss)`. Keep the old
   `MarkdownBodyView` / `MarkdownParser` types alive for now — the
   Designs tab still uses them.
3. **Second surface: the Designs file viewer.** Larger and more varied
   content (real docs from the mono repo). Swap the same `MarkdownBodyView`
   inside `MarkdownDocumentView`. At this point the parser still exists
   but has no live consumers in the app.
4. **(Optional) third surface: render the work-card popover preview
   inline.** Replace the plain `Text(descriptionSummaryText)` in
   `ContentView.swift:1233` with `InlineText(markdown:)` so bold / code
   / links render in the popover. Truncation policy stays the same.
   Filed as a separate task because it's a behavior change, not a
   parity migration.
5. **Cleanup: delete the hand-rolled parser.** Remove `MarkdownBlock`,
   `MarkdownBlockView`, `MarkdownTableView`, `CodeBlockView`,
   `MarkdownInline`, `MarkdownParser`, and the corresponding
   `MarkdownParserTests` in `Tests/BossTests/DesignsTests.swift`.
   `MarkdownBodyView`, `MarkdownViewerView`, and
   `MarkdownViewerWindowController` stay (they're the public seam call
   sites use) but their bodies collapse onto `StructuredText`.

**On feature-flagging.** The default plan is no flag. Each call-site
swap is one line and the old parser stays in the binary until step 5,
so reverting any single surface is just reverting that PR. A flag would
only earn its keep if we expected to ship Textual to users and toggle
back at runtime, which we don't — this is a one-way migration. If
the first surface flips badly in dogfood, we revert that commit and
keep going.

**When to delete.** After both step 2 and step 3 have been on `main`
long enough to dogfood (at least one full day of normal use across the
Designs tab and the task popover viewer), and there are no open
regressions, land step 5. The tests in `MarkdownParserTests` are the
canary — they prove the old code path is gone, not that the new one is
right.

## Out of scope for this migration

- The editor surface itself. The library choice keeps the door open;
  the design of the editor is its own project.
- Link-click policy. The renderer will surface links; what happens when
  one is clicked is a separate decision.
- Image attachment policy for the Designs tab (which repo paths are
  fetchable, caching, etc.). Textual's `AttachmentLoader` is the seam;
  the policy is a follow-up.
- Custom emoji / math expressions. Both are supported by Textual via
  `syntaxExtensions`. Not currently used by Boss content.

## Proposed task breakdown

Each item below is intended to be one task in the follow-up project.
The first two depend on the foundation; the cleanup depends on every
surface migration landing first.

1. **Add Textual dep + Boss theme shim.** Re-add `gonzalezreal/textual`
   in `Package.swift` and `Package.resolved`; restore the Bazel
   `swift_deps.from_package` use_repo for `swiftpkg_textual` and the
   `@swiftpkg_textual//:Textual` dep on `boss_mac_app_lib`; add a
   `BossStructuredTextStyle` (and matching inline style) that composes
   `.gitHub` with the existing Boss type scale, code-block surface,
   blockquote rail, and table border. No call-site changes.
2. **Migrate the work-card "Read full description" viewer.** Replace
   `MarkdownBodyView(source:)` inside `MarkdownViewerView` with
   `StructuredText(markdown:)` styled with `BossStructuredTextStyle`.
   Update the `MarkdownViewerView` snapshot/screenshot test if there is
   one; otherwise add a smoke test that the view builds.
3. **Migrate the Designs file viewer.** Replace `MarkdownBodyView`
   inside `MarkdownDocumentView` with `StructuredText`. Verify against
   a handful of real `.md` files in `tools/boss/docs/designs/`
   (long-form prose, fenced code, tables, blockquotes, nested lists).
4. **(Optional) Render the work-card popover description preview
   inline.** Swap `Text(descriptionSummaryText)` in `descriptionSummary`
   for `InlineText(markdown:)` so the first paragraph honors inline
   formatting. Keep the 6-line / 280-char truncation policy. Update
   `shouldOfferFullDescription` only if the new render makes the
   truncation predicate stale.
5. **Delete the hand-rolled parser.** Remove `MarkdownBlock`,
   `MarkdownBlockView`, `MarkdownTableView`, `CodeBlockView`,
   `MarkdownInline`, and `MarkdownParser` from `DesignsView.swift`;
   delete `MarkdownParserTests` from
   `Tests/BossTests/DesignsTests.swift`; collapse `MarkdownBodyView` /
   `MarkdownViewerView` / `MarkdownViewerWindowController` to thin
   wrappers around `StructuredText` (or inline them at the call sites
   if nothing else justifies the wrapper).

Tasks 2, 3, and 4 each depend on 1. Task 5 depends on 2 and 3 (and 4 if
that surface is included in this project).
