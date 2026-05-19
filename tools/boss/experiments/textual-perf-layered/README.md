# textual-perf-layered

Layered bisection rig for Boss markdown render slowness (mono#688). Builds on the Textual-only baseline from `tools/boss/experiments/textual-perf/` (PR #686) by re-introducing Boss's wrappers one at a time, so the layer that crosses from milliseconds into seconds is the offender.

See `tools/boss/docs/investigations/markdown-render-slowness-2026-05-18.md` for the static-analysis writeup and hypothesis ranking the rig is designed to validate.

## Run

```sh
cd tools/boss/experiments/textual-perf-layered
swift run textualperflayered
```

In another terminal, stream the timing logs:

```sh
log stream --predicate 'subsystem == "com.boss.textualperf"' --level info
```

Use the segmented picker at the top to switch between layers. Each picker click logs `phase=parse_start layer=Ln`, and the first non-zero `StructuredText` height fires `phase=parse_end layer=Ln duration_ms=…`. The pane is keyed by `.id(layer)`, so re-clicking a layer captures a fresh sample.

`bazel run //tools/boss/experiments/textual-perf-layered:textualperflayered` also builds the .app, but the working directory is whatever Bazel sets and the relative path resolution won't find the sample — set `BOSS_SAMPLE_MD=/absolute/path/to/sample.md` when running under Bazel.

## Sample source

Defaults to `tools/boss/docs/designs/installable-distribution-package-for-boss.md` (the same 47 KB doc the textual-perf rig in PR #686 uses). Resolution order:

1. `BOSS_SAMPLE_MD` env var (absolute path).
2. Walks up from the current working directory looking for `tools/boss/docs/designs/installable-distribution-package-for-boss.md`.
3. Falls back to a 1 KB placeholder with an error banner if neither resolves.

The doc is *not* duplicated into this experiment's Resources folder — keeping the diff small and the rig pointed at the live design-doc source.

## Layers

| Layer | Adds                                                  | Hypothesis it isolates                                                                 |
|-------|-------------------------------------------------------|----------------------------------------------------------------------------------------|
| L0    | nothing — matches PR #686                             | baseline (~190 ms)                                                                     |
| L1    | `.bossMarkdown()`                                     | Boss's table-Canvas overlay / code-block / blockquote / heading styles                 |
| L2    | Boss inner wrappers (frame, dual text-selection, title, double padding) | nested `frame(maxWidth: .infinity)` + two text-selection modifiers           |
| L3    | `.withComments()` stub                                | HStack-wrap, `@StateObject` rebuild surface, environment injection                     |
| L4    | view-model `.loading` → `.loaded` flip                | view-model rebuild on `renderContentID` UUID change                                    |
| L5    | view-model + async fetch                              | spinner → content transition cost                                                      |

The comments stub (L3+) is intentionally a `@Published`-surface lookalike without NSEvent monitors. Adding global event monitors from a benchmark rig is hazardous (they leak across runs and intercept other apps' shortcuts), and the monitors don't fire during render — they only fire on user key/right-click events. If the rig's L3 shows the slowness, the cause is in the wrapper structure, not the monitors.

## Reading the output

`phase=parse_end layer=Ln duration_ms=<n>` is the headline number for each layer. Capture 3+ samples per layer (re-click the picker) and average — first render of any layer pays one-time SwiftUI-init costs, so the second and third runs are more representative.

The on-screen overlay in the bottom-right of each pane shows the same numbers in case you don't want to keep `log stream` open.

## Not measured

- **Async attachment resolution.** `WithAttachments` resolves image URLs asynchronously *after* `parse_end` fires, so it doesn't affect the headline number. Boss's design docs typically don't have images, so this is fine.
- **Code tokenization for highlighting.** `HighlightedTextFragment.tokenize(...)` runs asynchronously and updates the highlighted code via state change *after* the initial render. The 7 code blocks in the 47 KB doc each get one async tokenize task, but those don't move `parse_end`.
- **HighlightingMarkdownParser path.** With zero comments, Boss uses `AttributedStringMarkdownParser.markdown(...)` — same parser as L0. The highlighting wrapper only matters if comments exist; this rig measures the no-comments case to stay comparable with PR #686.
