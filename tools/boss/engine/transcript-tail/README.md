# boss-transcript-tail

`boss-transcript-tail` is the low-level primitive `boss-engine` uses to
incrementally read a claude session's transcript file as it grows. Each
worker session writes a JSONL transcript (at the path the engine records on
`WorkRun.transcript_path`); this crate streams that file line by line,
handing back each newly-written record as a parsed `serde_json::Value`.

It exists because the hooks-to-socket channel only delivers discrete
lifecycle events, whereas the transcript carries richer per-token content.
Tailing the file lets the engine observe that fuller stream without the
worker pushing it.

## How it fits

The crate is a single stateful type, `TranscriptTail`, constructed from a
path. Callers drive it by repeatedly invoking `poll`, which reads everything
appended since the previous call and returns the completed JSONL lines it
found. The type owns only a byte cursor and a buffer for a trailing partial
line; it does no I/O scheduling of its own, so the caller picks the polling
cadence (the engine runs it on its own loop rather than the crate spawning a
task).

`poll` is deliberately tolerant of the realities of a file being written
concurrently by another process: a not-yet-created file yields no events and
is picked up once it appears, an incomplete trailing line is held back until
the next poll completes it, and a shrink below the cursor (truncation or
rotation) resets the cursor to re-read from the top. Blank lines are skipped;
a line that is present but not valid JSON surfaces as a `TailError::Json`
carrying the offending text.

This is a leaf crate with no internal dependencies, building on `tokio`
async filesystem I/O and `serde_json`. It is depended on by `boss-engine`,
and pairs conceptually with `boss-transcript-markdown`: this crate is
responsible for *getting* transcript records off disk, while
`boss-transcript-markdown` renders them for human consumption.
