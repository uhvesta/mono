# boss-transcript-markdown

Renders the JSONL session logs that Claude Code writes during a Boss
worker run into human-readable markdown (and plain text). It exists so
the engine and CLIs have one canonical, well-tested place to turn a raw
transcript file into something a person can read in the macOS app, in a
CLI command, or as a single markdown blob — without each consumer
re-implementing the brittle JSONL-shape parsing.

## Architecture

The crate is a pure, dependency-light transformation library: it takes
JSONL text in and produces strings or structured segments out, with no
I/O, async, or knowledge of where transcripts live. Conversion runs in
three stages that callers can stop at depending on what they need.

First, parsing turns raw JSONL into a flat list of normalized
`TranscriptEvent`s. Each line is parsed independently, and malformed,
unrecognised, or incomplete trailing lines are silently skipped, so a
partially-written log (common while a worker is still running) still
yields the events that are well-formed. An event carries a monotonic
sequence number plus a discriminated `TranscriptEventKind` covering the
shapes Claude Code emits: user/assistant text, thinking blocks, tool
calls, tool results, and `system` events such as PR links and stop-hook
summaries.

Second, `events_to_segments` maps events to `TranscriptSegment`s — the
display-oriented form. This stage owns the presentation decisions: per
kind it picks a short label, renders an appropriate markdown body
(tool calls get tool-specific rendering, e.g. a shell fence for `Bash`
or a path-plus-diff for `Edit`), and sets display hints like whether a
segment is collapsible or collapsed by default. Large tool results are
truncated at a configurable byte budget (`RenderOpts`), recording how
much was shown so the UI can surface a "…showing N of M bytes" note.
Segments are designed for lazy display, so the engine can hand them to
the app one at a time rather than concatenating a giant document.

Third, two flattening helpers collapse segments into a single string:
`segments_to_markdown` for the markdown blob / CLI markdown path, and
`render_text` for a plain-text rendering (it strips markup back out).

The structured `TranscriptSegment` form is the value-add over the
sibling `boss-transcript-tail` crate, which handles incremental reading
of a transcript file; this crate owns the rendering of whatever bytes
are read. Only `boss-engine` depends on it, consuming both the
segment stream and the flat-document helpers.
