# boss-protocol

The foundation crate for the Boss automation system. It defines the
shared domain types that describe Boss's work (products, projects,
tasks, chores, executions, comments, attention items, automations) and
the wire protocol spoken over the engine's frontend socket. Every other
Boss crate links against it so that the engine and its clients agree on
exactly the same shapes; `boss-protocol` itself owns no behaviour and
talks to nothing — it is pure data plus a little serde glue.

## Architecture

The crate is organised around two concerns: the *domain* model and the
*wire* envelope that carries it.

The domain types are the serializable projections of the rows the engine
keeps in its work database. They are deliberately plain — `String`-typed
ids and status fields, `Option`s for nullable columns — so that the
engine's DB mappers can populate them column-for-column and clients can
render them without re-deriving meaning. Small validated enums
(`EffortLevel` and similar) cover the closed vocabularies the engine
cares about, while open-ended config blobs stay as `serde_json::Value`
so new feature variants can ship without a protocol bump.

The wire layer defines the request/response/event protocol between the
CLIs and clients (`boss`, `bossctl`, `boss-client`) and the engine. A
single `FrontendRequest` enum enumerates every operation a caller can
ask of the engine; `FrontendEvent` carries both the matching responses
and the unsolicited pushes the engine fans out to subscribers. Those
pushes are organised by topic — there are helpers for composing the
per-product, per-execution, per-run, and per-artifact topic strings a
subscriber joins — so the macOS app and other live observers receive
only the streams they care about, tagged with a monotonic revision for
ordering. A second, smaller RPC surface describes the engine ↔ app pane
protocol (spawning and tearing down worker panes) that is layered inside
those same envelopes.

Alongside the request/event protocol sit the types for observing a
worker's life: the typed `WorkerEvent` form of the claude lifecycle
hooks (delivered via the `boss-event` shim), the per-slot
`LiveWorkerState` the engine derives from those hooks, the diagnostic
snapshot returned by the live-status debug verb, and the deterministic
slot-id → crew-name roster that the engine, `bossctl`, and the Swift app
keep in lock-step.

## The builder convention

Domain structs with eight or more fields (`Task`, `WorkExecution`,
`Product`, `Project`) derive `bon::Builder` with `on(String, into)`.
This is a deliberate source-stability measure: adding a new optional
field to one of these structs should not force every construction site
across the engine and clients to change. New optional fields carry a
`#[builder(default)]`; new required fields are an explicit breaking
change. See the repo `CLAUDE.md` for the full rules, including the
caveat that the engine's production DB mapper functions keep using
struct literals so that an unmapped new column is a compile error.

## Where it sits

`boss-protocol` has no internal dependencies — it depends only on
`serde`, `serde_json`, `bon`, and `thiserror`. It is depended on by
`boss-client`, `boss-engine`, `boss-editorial`, `bossctl`, and the
`boss` CLI. Changing a type here ripples to all of them, so it is the
crate to evolve carefully and additively.
