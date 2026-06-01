# boss-editorial

`boss-editorial` is the editorial-rules evaluator that keeps Boss's internal
vocabulary out of user-facing PR and issue text. Worker sessions author PR
titles, bodies, and comments that get published to GitHub; this crate decides
whether that text is clean, can be silently sanitised, or must be sent back to
the worker for a rewrite. It owns the policy for what counts as "internal" Boss
language and how to scrub it.

## Architecture

The crate is a single pure function, `evaluate(body, title, rules,
template_body)`, returning an `EditorialDecision` of `Allow`, `Rewrite` (with
the sanitised text and a list of `Finding`s describing each change), or `Block`
(with findings the worker must fix by hand). Because it does no I/O and holds no
global state, it is cheap to call on the engine's async threads.

Two classes of rule apply on every call. *Rewrite* rules strip text that can be
removed automatically — Boss identifier shapes (`exec_…`, `proj_…`, `task_…`,
`chg_…`, `boss/exec_…` branch names) and UUIDs that sit near the words "lease"
or "cube" — collapsing leftover whitespace afterwards. *Block* rules flag
free-text phrases that leak internals ("Boss worker", "the engine", "PreToolUse",
and similar) which a worker must rephrase rather than have machine-deleted. A set
of baked-in patterns for both classes is always on; products can layer additional
user-configured `RedactionRule`s on top. Block findings always win over Rewrite,
so any blocking phrase forces a `Block` decision.

Scanning is markdown-aware: a lightweight segment splitter (not a full
CommonMark parser) skips fenced code blocks entirely and applies Rewrite
patterns inside inline code spans only when the whole span is an identifier,
avoiding false positives on legitimate examples. When the product's
`TemplatePolicy` is `Enforce` and a PR template is supplied, `evaluate` also
checks that each H2/H3 heading from the template is present in the body, emitting
a `Block`-class finding per missing section.

User-supplied patterns are validated and compiled once into `CompiledRules`
(surfacing bad regexes early), while baked-in regexes live in `LazyLock`s, so
callers can amortise compilation across the hot path.

It depends only on `boss-protocol` for the shared rule types (`EditorialRules`,
`RedactionKind`, `TemplatePolicy`) and `regex`. `boss-engine` is the sole
consumer: it compiles a product's rules, calls `evaluate` from the `gh`
PreToolUse interception path, and turns the resulting decision into an allow,
an in-place rewrite, or a denial with a `decisionReason` shown to the worker.
