# Friendly Numeric IDs for Work Items

## Problem

Boss work-item primary ids look like `task_18aefd71a9458550_17`,
`chore_18b002cab32a4c10_3`, `proj_18a2bbe20fc03718_8`. They are
authoritative — a monotonic hex timestamp plus a per-process counter,
universally unique, easy for the engine to mint without coordination,
and stable forever. They are also unspeakable. When the coordinator
writes "I filed `task_18aefd71a9458550_17`", the user cannot map that
string back to a card on the kanban without copy-paste or a CLI
roundtrip. The eye cannot scan a column for it; the voice cannot
dictate it; two of them in the same paragraph blur into noise.

Every issue tracker the user has ever used solves this with a short
sequential number — `#42`, `BOSS-42`, `T-42`. The number is small,
glanceable, easy to say aloud ("did you look at forty-two?"), and
trivially mapped to a UI element. The primary id stays around because
machines still need a globally unique handle.

This design adds a **friendly numeric id** alongside the primary id.
It is short, monotonically allocated, visible on every kanban card,
and accepted as a lookup key by the CLI and the macOS app. The
primary id is unchanged and remains authoritative. Friendly ids are
additive — no column rename, no API break, no migration of any
external system that already quotes a primary id.

The primary use cases are:

1. **Coordinator-to-human chat.** The coordinator says "I filed #42
   for the migration; #43 covers the UI" instead of two hex blobs.
2. **Human-to-CLI.** `boss task show 42` and `boss task show #42`
   both work; the user types what they see on the card.
3. **Voice / verbal.** "Did review #42 land?" is sayable.
4. **Visual scan.** The kanban card's top-right badge is the same
   thing the user just heard or typed; the eye finds the match in
   under a second.

## Goals

- Every work item (every `tasks` row regardless of `kind`, and every
  `projects` row, and every `products` row if we extend later — but
  see Non-Goals) carries a short, monotonically-allocated numeric
  identifier in addition to its existing primary id.
- The friendly id is **rendered as a small badge in the top-right
  corner** of every kanban card on every lane in every column, and
  in the detail popover header.
- The coordinator (the Claude session attached to this Boss engine)
  refers to items by friendly id by default in chat, falling back
  to the primary id only when disambiguation requires it.
- CLI surfaces (`boss task show`, `boss task list`, `boss chore
  show`, `boss project show`, etc.) accept the friendly id as a
  lookup key in addition to the primary id, and emit both the
  primary and the friendly id in JSON and human output.
- Allocation is race-safe under concurrent creates from any surface
  (CLI, macOS app, engine auto-creates, manifest-driven spawn).
- Existing rows in the Boss DB get backfilled deterministically so
  the numbers are predictable and reproducible (a fresh clone of
  the same DB plus the same backfill rule yields the same numbers).
- The friendly id is **additive, not a replacement.** Primary ids
  stay byte-for-byte identical. `task_18aefd71a9458550_17` continues
  to work everywhere it worked before; no API surface that takes a
  primary id grows a new failure mode.

## Non-goals

- **Renaming, retiring, or hiding primary ids.** They stay
  authoritative on the wire, in JSON output, in error messages, in
  audit logs, and in `work_item_dependencies` / `work_executions`
  foreign keys. The friendly id is a display + lookup convenience;
  the primary id is the canonical handle.
- **Numbering non-work-item rows.** `work_executions`, `work_runs`,
  `work_attention_items`, `conflict_resolutions`, `effort_escalations`
  etc. all keep their primary ids unadorned. They aren't surfaced as
  cards and their ids aren't said aloud often enough to justify the
  scaffolding.
- **A separate human-readable slug or title shortener.** Tasks have
  a `name` already; if the user wants to refer to a task by name
  they can. The friendly id is a *number*, not a slug or alias.
- **Search by friendly id beyond exact lookup.** No fuzzy match, no
  "did you mean #42?", no prefix completion in the CLI. If the user
  types `42` we look up `42`; if they type `42x` we error.
- **Backporting friendly ids onto external references.** GitHub PR
  bodies, Slack messages, and old commits that already quote primary
  ids stay as-is. Rewriting them would be expensive and lossy.
- **Friendly ids on deleted work items.** Soft-deleted rows
  (`tasks.deleted_at IS NOT NULL`) keep their friendly id in the
  column but it does not get reassigned, and the lookup CLI surfaces
  it only with a `--include-deleted` flag. Hard-deletes (which Boss
  does not currently do but might in the future) release the
  number; we do not gap-fill.
- **Padding / sortable formatting.** `#42` always, never `#0042`. We
  don't display-pad and we don't store-pad; whatever number the
  allocator gave us is what gets shown.
- **A separate "ticket" or "issue" abstraction.** Friendly ids are a
  column on existing rows, not a new entity. They do not get their
  own table, their own RPC, their own lifecycle.

## Alternatives considered

### Alternative A — Per-product per-kind sequence with prefix (e.g. `boss-T-42`)

GitHub Issues style. Each `(product, kind)` pair gets its own
sequence: Boss tasks numbered `T-1`, `T-2`, …; Boss chores
`C-1`, `C-2`, …; Flunge tasks `T-1`, `T-2`, … (collides on display
with Boss tasks unless prefixed). Format like `boss-T-42`.

**Why not:**

- The prefix carries information the kanban already conveys (the
  column the card lives in says it's a task; the product header says
  it's Boss). Repeating it in the id is noise.
- Pronounceability suffers: "boss-tee-forty-two" is three tokens
  where "forty-two" is one. The pronounceability *was* the point.
- The user's stated end state is "a badge in the top-right of every
  card." A `boss-T-42` badge is wider than a card title sometimes;
  layout fights start.
- Multi-product chat is rare in practice for this user (the engine
  primarily runs against Boss itself with occasional Flunge work),
  so the cross-product disambiguation that prefixes solve is
  solving a problem the user does not have.
- We *would* still need the prefix scheme if cross-product chat
  became common — but we can add it later (Q1 below explains the
  graceful path) without invalidating the per-product numbers we
  hand out today.

### Alternative B — Global sequence across the whole engine

One counter; every new row of any kind in any product gets the next
integer. `#1` is the very first thing ever filed; `#42` is the
forty-second; the millionth chore eventually gets `#1000000`.

**Why not:**

- Within a single product (the dominant case) it interleaves
  unrelated kinds: the user files three Boss tasks and a Flunge
  chore lands in the middle, and Boss's "task list" reads `#5, #6,
  #8, #9` with #7 unaccounted-for in this product. Confusing.
- Numbers grow faster than per-product sequences, so they get longer
  sooner. `#1234` is fine; `#123456` is not glanceable any more.
- It conflates "what's the next number" with "what's the next number
  *here*." The latter is the question the user actually asks ("how
  many tasks have we filed for Boss?"); the former is rarely useful.
- The badge gets wider as numbers grow. Per-product sequences keep
  numbers small for years.

### Alternative C — Hex friendly ids (e.g. `#2a` for 42)

Two characters get you 256 items; three chars get 4096. Visually
distinct from the long hex primary ids only by length, but still
short.

**Why not:**

- Hex is harder to say aloud. "Two-A" is not a number you reach for
  the way you reach for "forty-two."
- The user can already read decimal; hex buys nothing.
- The user explicitly said "user is open to numeric or hexadecimal"
  but the deciding criterion was "two cards visually distinct in a
  glance." Decimal `#42` vs `#43` is as distinct as hex `#2a` vs
  `#2b` while being instantly readable. Hex loses on the secondary
  axis (pronounceability) without winning on the primary.

### Alternative D — Insert-time SQL trigger

Use a SQLite `AFTER INSERT` trigger on each numbered table to assign
the friendly id from a per-table counter. SQL serialises writes
inside a transaction, so allocation is automatically race-safe.

**Why not:**

- Triggers are invisible side-effects. Future migrations would have
  to remember to update both the trigger and the column; missing
  one breaks allocation silently.
- Cross-table sequences (per-product across `tasks` and `projects`
  rows, see Q1 below) are awkward in pure SQL — you'd need a
  separate counter table and a trigger on each numbered table that
  reads/writes it, all of which is harder to test than the same
  thing in Rust.
- The engine already creates work items in two places (CLI/RPC path
  and engine auto-creates), both of which go through `WorkDb`. A
  Rust-side allocator sees the same call sites; the trigger would
  add a parallel mechanism with no benefit.
- Trigger logic is not portable. Boss uses SQLite today; if it ever
  needs Postgres for a multi-user setup, the trigger comes along
  but with different syntax. The Rust allocator is portable.

### Alternative E — CLI-side / app-side allocation

Each client (CLI invocation, macOS app instance) reads the current
max friendly id, increments, writes the new row with that id, and
hopes nothing else inserted in between.

**Why not:**

- Famously racy. Two concurrent `boss task create` invocations
  produce two rows with the same friendly id unless we add explicit
  locking, which means the engine has to mediate anyway, which
  means we may as well put the allocator in the engine.
- The CLI doesn't even directly write to SQLite — it calls into the
  engine via RPC. So "CLI-side" really means "RPC client-side,"
  which is even more obviously wrong.

### Alternative F — `AUTOINCREMENT` ROWID exposure

Just expose `rowid` (or `INTEGER PRIMARY KEY AUTOINCREMENT`) as the
friendly id. SQLite gives it to us for free.

**Why not:**

- Global across all rows in the table, so per-product sequences are
  impossible (see Q1).
- ROWIDs are reused after deletes unless `AUTOINCREMENT` is set, and
  `AUTOINCREMENT` requires an `INTEGER PRIMARY KEY` column, which
  conflicts with our existing `id TEXT PRIMARY KEY`.
- Mixing two primary keys (the existing TEXT one and a new INTEGER
  AUTOINCREMENT) is schema noise for marginal gain.

## Chosen approach

A new nullable integer column `short_id` on every numbered table
(`tasks`, `projects`), allocated atomically by a single engine-side
counter table (`short_id_sequences`), assigned at row-insert time
inside the same transaction as the row insert, deterministically
backfilled for existing rows ordered by `created_at`, displayed as
`#<n>` everywhere the primary id appears today, and accepted as a
lookup key alongside the primary id in CLI / RPC / app.

The next nine subsections (Q1 through Q9) commit to one answer for
each open design question.

---

### Q1 — Sequence scope: per-product

**Answer:** **Per-product, across all work-item kinds inside that
product.** Boss tasks and Boss chores and Boss projects share one
sequence — `#1, #2, #3, …` — incrementing in the order rows are
inserted. Flunge has its own sequence starting fresh at `#1`. No
prefix on display: the column / row context the card lives in
already tells the user which product it belongs to.

**Why per-product and not per-product-per-kind:**

The user's day-to-day vocabulary is "task #42" and "chore #43" and
"project #44," and they want those three numbers distinct because
they refer to three different rows. Per-product-per-kind would
collide — there'd be a "task #42" *and* a "chore #42" and the
coordinator would have to spell out the kind every time. Within a
product, mixing kinds in one sequence is fine: the user's question
is "where is row #42 on the board" and the answer is in exactly one
place regardless of kind. (Q5 explains how `boss task show 42` and
`boss chore show 42` disambiguate inside the same sequence.)

**Why per-product and not global:**

See Alternative B. The dominant use case is single-product chat; a
per-product sequence keeps numbers small (Boss has ~100 work items
ever filed, so we have years before we hit four-digit numbers) and
reads naturally inside a product's kanban.

**Cross-product chat — what about it?**

The coordinator says `Boss #42` (or `boss/#42` in machine output)
when the chat involves more than one product in the same turn.
This is a *display* convention applied by the coordinator and the
CLI's human formatter; it is not a separate id. The underlying
column is still just `42`; the prefix is rendered. Q7 spells out
the rule.

**Schema implication:**

The uniqueness invariant is `(product_id, short_id)` — unique per
product, monotonic, gap-free for non-deleted rows in insertion
order. No global uniqueness across products. This is enforced by
the allocator's read-modify-write cycle inside the transaction
(Q3) and a `UNIQUE` index for safety:

```sql
CREATE UNIQUE INDEX tasks_product_short_id_idx
    ON tasks(product_id, short_id) WHERE short_id IS NOT NULL;
CREATE UNIQUE INDEX projects_product_short_id_idx
    ON projects(product_id, short_id) WHERE short_id IS NOT NULL;
```

The `WHERE short_id IS NOT NULL` clause lets us keep the column
nullable for migration safety (Q4) and for any future row type
that doesn't get a friendly id.

---

### Q2 — Format: `#<decimal>`, unpadded

**Answer:** **`#42` in all human-facing surfaces.** Unpadded
decimal, single leading `#`, no trailing punctuation.

- The `#` is **visible** in display (CLI human output, kanban
  badge, popover header). It signals "this is a friendly id, not a
  bare number" and prevents accidental confusion with priorities
  (which never use `#`), counters, or ordinals.
- In CLI input the `#` is **optional**. `boss task show 42` and
  `boss task show #42` both work. The argument parser strips a
  leading `#` if present, then parses the rest as a decimal integer.
- In JSON wire output the field is `short_id: 42` (integer, no
  prefix). The `#` is a display affordance, not a storage form.
  Tools that pipe `boss task list --json` into `jq` deal with
  integers, not strings.
- No padding. `#1` and `#10000` both render with their natural
  digit count. Padding to a fixed width was considered for
  alignment in `boss task list`'s human output, but the kanban
  badge is variable width anyway and the CLI table aligns on
  string length already; padding the column with leading zeros
  would just look like the dates of a 1970s mainframe.
- Cross-product disambiguation prefix (Q7): `Boss #42` in chat,
  `boss/#42` in machine-readable contexts. Always lowercase product
  slug; always the same `#` before the number.

The hex alternative (C) was rejected in Alternatives. Decimal is
sayable, scannable, and unambiguous.

**Example renderings:**

```
$ boss task show 42
#42  review-feedback-iter
     product: boss
     status:  in_review
     ...

$ boss task show 42 --with-primary-id
#42  task_18aefd71a9458550_17   review-feedback-iter
     product: boss
     status:  in_review
     ...

$ boss task list --json | jq '.[] | {short_id, id, name}'
{ "short_id": 42, "id": "task_18aefd71a9458550_17", "name": "..." }
{ "short_id": 43, "id": "task_18b01239ff21a3c8_3",  "name": "..." }
```

(JSON always carries both fields; the human default hides the
primary id behind `--with-primary-id`. See Q5 for the full
rule.)

---

### Q3 — Allocation: engine-side sequence table, allocated in-transaction

**Answer:** A new table `short_id_sequences` keyed by `product_id`
holds the next value to allocate. Insertion of a new `tasks` or
`projects` row reads the current value, increments it, writes it
back, and stamps the row — all inside the same SQLite transaction
that's already wrapping the insert. SQLite's serializable isolation
(the default in WAL mode for the writer) makes this race-safe
without explicit locks; concurrent writers serialise at the SQLite
level.

```sql
CREATE TABLE short_id_sequences (
    product_id  TEXT PRIMARY KEY REFERENCES products(id),
    next_value  INTEGER NOT NULL DEFAULT 1
);
```

The allocator (Rust, in `engine/src/work.rs`):

```rust
fn allocate_short_id(tx: &Transaction, product_id: &str) -> Result<i64> {
    let next: i64 = tx.query_row(
        "SELECT next_value FROM short_id_sequences WHERE product_id = ?1",
        [product_id],
        |row| row.get(0),
    ).optional()?
        .unwrap_or(1);
    tx.execute(
        "INSERT INTO short_id_sequences(product_id, next_value) VALUES(?1, ?2)
         ON CONFLICT(product_id) DO UPDATE SET next_value = excluded.next_value",
        params![product_id, next + 1],
    )?;
    Ok(next)
}
```

Every `WorkDb` method that inserts a `tasks` or `projects` row
(`create_task`, `create_chore`, `create_many_tasks`,
`create_many_chores`, `create_project`,
`insert_design_task_for_project_in_tx`, manifest-driven spawn from
`design-producing-tasks`) calls `allocate_short_id` inside its
transaction and stores the result in the row's `short_id` column.
The existing `next_id(prefix)` helper that mints primary ids is
untouched.

**Why not just `MAX(short_id) + 1` per insert:**

That works only if every concurrent writer holds the same lock.
SQLite *does* serialise writers, so in practice
`MAX(short_id) + 1` would produce correct results — but it's
implicit, fragile to future code paths that read without writing,
and hard to reason about under any future move to Postgres. An
explicit counter table makes the invariant testable
(`SELECT next_value FROM short_id_sequences WHERE product_id = ?`
is always strictly greater than `MAX(short_id)` for that product)
and decouples the allocator from the table being inserted into.

**Why not one global counter:**

Per Q1; keeps the abstraction matching the data.

**Why not a single-row-per-table counter (`short_id_sequences(table_name, product_id)`):**

We chose per-product, not per-product-per-table, so the counter is
per-product too. If we ever split (Q1's "cross-table" footnote
forecloses this for v1), the schema migration is mechanical.

**Backfill timing:** the counter is initialised in the same
migration that adds the column; for each existing product, after
backfilling existing rows (Q4), set
`short_id_sequences.next_value = MAX(short_id) + 1`. Subsequent
creates pick up where the backfill left off.

**Performance.**

A reviewer asked whether the allocator slows down every write.
The honest answer is "by a negligible amount at expected write
volumes, but worth being explicit about."

- **Per-insert cost.** Each `tasks` / `projects` insert now does
  one extra `SELECT` and one extra `UPSERT` against
  `short_id_sequences`. Both touch a single row keyed by
  `product_id` (the table's primary key), so each is an O(log N)
  index lookup where N is the number of products — currently 1–2.
  In absolute terms on a local SQLite in WAL mode, each
  round-trip is sub-millisecond; the pair adds well under a
  millisecond to a row insert that itself takes a few
  milliseconds at worst.
- **Volume context.** Boss writes work items on the order of a
  few dozen times per day during active development and far less
  in steady state. The added cost across a full day is on the
  order of tens of milliseconds total — i.e. unmeasurable. The
  design does not optimise for high-volume inserts because that
  workload does not exist.
- **Worst case.** A bulk import (`boss task create` in a tight
  loop) or a future automated chore-creation pipeline doing
  thousands of inserts per second would amplify the per-insert
  overhead. v1 does not optimise for this. If it ever
  materialises, the allocator can be reworked to reserve ranges
  (allocate N ids in one transaction, hand them out from an
  in-memory cursor until exhausted); the schema does not need to
  change. We accept the cost rather than pre-optimising for a
  workload that does not exist yet.
- **No benchmarks promised.** The math above is the design's
  case for "no measurable impact." If post-migration write
  performance regresses, we re-evaluate; otherwise no benchmark
  is part of v1.

---

### Q4 — Migration of existing rows: backfill by `created_at` ascending, per product

**Answer:** The migration that adds `short_id` and
`short_id_sequences` also **backfills every existing row** so the
column is dense and predictable from day one.

**Step 0 — back up the database first.**

Before the migration touches a single row, the engine takes a
file-level snapshot of `state.db`. This is non-negotiable: if the
backup cannot be written, the migration aborts and the engine
exits non-zero without modifying any row. Concretely:

- The backup is a byte-for-byte copy of `state.db` to a sibling
  path in the same directory (`~/Library/Application Support/Boss/`
  in production):
  `state.db.pre-friendly-id-backup-<UNIX_TIMESTAMP>`. The Unix
  timestamp is the engine's wall-clock at migration start;
  millisecond precision is unnecessary because the migration only
  runs once per DB.
- The backup is taken **inside the migration runner**, which lives
  in the engine and fires from `WorkDb::open` as part of the
  `migrate_*` chain (same place every other migration runs). The
  runner uses the SQLite backup API (`VACUUM INTO`) so the snapshot
  is consistent even if a concurrent reader has the WAL open; a
  plain filesystem `copy()` is the fallback if `VACUUM INTO` is
  unavailable for any reason.
- **Abort on failure.** If the backup cannot be written (disk
  full, permission error, target path collision), the migration
  rolls back its outer transaction, logs the error, and exits with
  a non-zero status. The engine does not run the backfill against
  a DB it could not back up.
- **The backup is preserved indefinitely.** The migration does not
  delete the backup file on success. The user can `rm` it manually
  once they're satisfied the migration produced correct numbers.
  We deliberately do not auto-clean: a backup that disappears at
  some point after success is worse than one that lingers.
- **Restore procedure** (documented for copy-paste; the engine
  prints it on successful migration):

  ```
  # 1. Stop the engine and the app.
  launchctl unload ~/Library/LaunchAgents/com.spinyfin.boss.engine.plist  # or however the user runs it
  killall Boss                                                            # the macOS app

  # 2. Replace the migrated DB with the pre-migration backup.
  cd "~/Library/Application Support/Boss/"
  mv state.db state.db.post-migration-broken
  mv state.db.pre-friendly-id-backup-<UNIX_TIMESTAMP> state.db

  # 3. Restart the engine. The migration will run again on the
  #    restored DB; if the user wants to opt out of the migration
  #    entirely they can downgrade the engine binary to a build
  #    that predates this work.
  ```

The restore path assumes the user spots corruption post-migration.
The migration itself is deterministic (see "Deterministic and
reproducible" below) so corruption should not happen; the backup
exists for the case where the user simply doesn't like the
numbers, or where a future bug in this code path slips through
tests.

**Backfill order (runs after the backup completes):**

1. For each `products.id`, gather every `tasks` row (regardless of
   `deleted_at` — soft-deleted rows get numbers too, see Non-Goals)
   and every `projects` row whose `product_id` matches.
2. Sort the combined set by `(created_at ASC, id ASC)`. The
   tiebreaker on `id` matters because two rows can share a
   `created_at` second; the primary id is itself monotonic within a
   process so this preserves insertion order.
3. Assign `1, 2, 3, …` to that sorted list, writing back to each
   row's `short_id` column.
4. Stamp `short_id_sequences.next_value = N + 1` where `N` is the
   highest value assigned for that product.

The combined set (tasks + projects in one stream) implements Q1's
"per-product across all kinds in that product" rule for historical
rows. New rows allocated after the migration just keep the counter
ticking.

**Why `created_at` and not primary id chronology:**

Primary ids embed a nanosecond timestamp (`nanos:x_counter:x`), so
sorting on the primary id string in lexicographic order is almost
the same as sorting on `created_at`. Almost — but the primary id's
hex encoding doesn't sort as ASCII the way base-10 timestamps do
(`a_<small_nanos>_0` < `a_<large_nanos>_0` only because the hex
nanos are the same width; if process clock skew puts an older row's
nanos higher than a newer row's, lex order would invert). Using
`created_at` (a stored ISO-ish seconds string in the existing
schema) is the user-meaningful ordering and is what the kanban
already sorts by.

**Deterministic and reproducible:**

A second clone of the same `boss.sqlite3` produces the same
backfilled numbers because the sort key is stable and the rows
aren't modified during backfill. If the user deletes the DB and
restores a backup, the friendly ids come back unchanged.

**No backfill of soft-deleted rows? Yes, backfill them too.**

A soft-deleted task still has a primary id; the engine still
references it from `work_item_dependencies` etc. Giving it a
friendly id costs one integer and means a user who looks at the
audit log can still see "task #17 was deleted on …" without a
hex blob. Hard-deletion (which Boss doesn't do today) is treated
in Q9.

**The migration is one-shot at engine boot:** the standard Boss
schema-migration mechanism (`migrate_*` functions in `work.rs`)
already runs at `WorkDb::open`. We add one more: `migrate_short_id_columns`.
It's idempotent — if `short_id` columns already exist on every
relevant table and `short_id_sequences` exists, the migration
short-circuits. Fresh installs run it on first boot and there's
nothing to backfill, so it just adds the empty counter rows.

---

### Q5 — CLI lookup semantics

**Answer:** **Friendly ids are accepted alongside primary ids by
every CLI verb that takes a work-item selector.** The selector
grammar in priority order:

1. If the selector starts with `#`, strip it and treat the rest as
   a decimal integer; look up by `(product_id, short_id)`.
2. If the selector parses as a positive integer, treat it the same
   way (the `#` is optional).
3. If the selector starts with `task_` / `chore_` / `proj_` /
   `prod_` (any known primary-id prefix), look up by primary id.
4. Otherwise, fall back to the existing string-slug / product/slug
   semantics.

**Disambiguation across kinds (Q1's per-product sequence implies
tasks and chores share a sequence):**

- `boss task show 42` resolves `42` against the current product's
  `tasks` rows. If `short_id = 42` is on a chore (i.e. a `tasks`
  row with `kind = 'chore'`), that's fine — `boss task show`
  surfaces any kind of task-table row, and the disambiguation is
  by row, not by verb. (The verb naming is historical and a tiny
  bit misleading; `boss task show` shows tasks, chores, design
  tasks, anything in `tasks`.)
- `boss chore show 42` is an alias for the same lookup but errors
  if the resolved row's `kind != 'chore'`. So a user who types
  `boss chore show 42` and gets a task back gets a clear "row #42
  is a task, not a chore — use `boss task show 42`" error.
- `boss project show 42` looks up `(product_id, short_id)` against
  `projects`. The sequence is shared with tasks per Q1, but project
  rows live in `projects`, so the lookup table is distinct. If `#42`
  is a task and the user runs `boss project show 42`, they get a
  "row #42 in this product is a task, not a project — use
  `boss task show 42`" error.

**Cross-product disambiguation:**

`boss task show 42` requires a current product. The current product
is resolved by:

1. `--product <slug>` flag, or
2. the user's `$BOSS_PRODUCT` env var, or
3. the single product if there's only one, or
4. interactive selection.

If multiple products are in play and the user wants to be explicit,
they can write `boss task show boss/42` or `boss task show
boss/#42` — the `<slug>/<number>` form bypasses product resolution.
This matches the existing CLI convention for project selectors
(`product/project_slug`).

**`boss task show <primary_id>` still works** for every caller that
already passes one; the primary-id prefix detection in step 3 above
ensures we never accidentally interpret a primary id as a number.
(The primary ids start with `task_`/`chore_`/`proj_` so they're
unambiguous.)

**JSON output gets both fields:**

```jsonc
{
  "id": "task_18aefd71a9458550_17",
  "short_id": 42,
  "product_id": "prod_18a2bb78815be670_1",
  "name": "...",
  ...
}
```

`short_id` is always present on numbered rows; it is omitted via
`#[serde(default, skip_serializing_if = "Option::is_none")]` on
hypothetical legacy rows where the migration hasn't run yet, which
in practice never happens because the migration runs on engine
boot.

**Human output for `boss task show` (and every other human-mode
CLI verb that prints a work item): friendly id only by default,
primary id behind an opt-in flag.**

Default human-readable output shows only the friendly id, the
name, and the rest of the metadata the verb already prints. The
primary id is **suppressed** unless the user asks for it.

```
$ boss task show 42
#42  review-feedback-iter
     product: boss
     status:  in_review
     ...
```

This applies symmetrically to `boss task list`, `boss chore
show`, `boss chore list`, `boss project show`, `boss project
list`, and any other current or future verb that renders a
work-item row in human mode. None of them prints the primary id
in default human output.

**Opt-in flag: `--with-primary-id`.**

The flag is named `--with-primary-id` (additive, opt-in, present
on every work-item-printing verb). When passed, the primary id
is rendered alongside the friendly id; in `show` it appears on
the first line, in `list` it appears as an extra column.

```
$ boss task show 42 --with-primary-id
#42  task_18aefd71a9458550_17   review-feedback-iter
     product: boss
     status:  in_review
     ...
```

The primary id is rendered dim (terminal `2;` attribute) so that
even when shown, it doesn't compete with the friendly id for the
eye.

**Why `--with-primary-id` and not `--verbose`.**
`--verbose` is broader and tempting to overload later (more
metadata, more rows of subtask state, more attention-item
context). Coupling "show me the primary id" to "show me
everything" forces a binary choice the user may not want. A
named, scoped flag stays clean. (An audit of the current CLI
turns up no existing `--verbose` flag on the work-item verbs, so
the name is available — but we still prefer the specific name on
the principle that flags should mean exactly one thing.)

**JSON output: always both fields, regardless of flag.**

`--json` mode on every subcommand emits *both* `id` and
`short_id` unconditionally. The hide-by-default behaviour applies
only to human-readable text output; tools that pipe `--json`
through `jq` already have to deal with the full record and
benefit from never wondering whether a field is present.
`--with-primary-id` is a no-op in `--json` mode (and the CLI does
not error if both are passed; the flag is simply redundant
there).

**Errors and "not found" cases still print the primary id.**

The hide-by-default rule has one explicit exception: error
messages, "no such row" messages, and "ambiguous lookup" messages
print the primary id when the engine has resolved one. This
matters because a user troubleshooting (or grepping logs, or
talking to support) needs the canonical handle that is unique
across the engine, and the friendly id alone is insufficient
when something has gone wrong.

```
$ boss task show 999
error: no task with id #999 in product boss

$ boss task show task_18aefd71a9458550_17 --product flunge
error: task task_18aefd71a9458550_17 belongs to product boss, not flunge
       (id in boss is #42; use `boss task show 42` from boss)
```

The error path always knows the primary id once the row is
resolved (or, in the not-found case, that no primary id matched);
showing it is free and avoids forcing the user to flip on
`--with-primary-id` and re-run the command. Note that the
error-message wording follows the Q6 rule too — the user-facing
text says "id" (or `#999`), never "friendly id."

**Consistency with the coordinator-referral rule (Q7).**

Q7 instructs the coordinator to *prefer the friendly id when
speaking to the user*, with the primary id quoted parenthetically
on first mention in a session. The CLI's hide-by-default
behaviour is the same pattern in a different surface: the
friendly id is the user-facing handle, the primary id is
available on demand or in error paths. The two surfaces stay
aligned so the user is never seeing the primary id in one place
and not the other for no reason.

---

### Q6 — UI badge placement, typography, behaviour

**Answer:** Top-right of every kanban card. Small, monospaced,
secondary colour, the `#` visible. No label, no caption,
no qualifier — the badge is just `#42`.

**No "friendly" anywhere the user reads.** From the user's
perspective these are just *the* ids — the only ids they see
in the UI by default. The word "friendly" exists only in
internal terminology (the schema column is `friendly_id` in
some prose / `short_id` in code, the JSON field is `short_id`,
code comments and this doc say "friendly id"). The UI itself
never qualifies the number. If a label is unavoidable (e.g. in
a detail-view header that lists multiple identifiers side by
side), the label is `ID` or just `#`, never "Friendly ID."

Concretely, in `WorkBoardCardView` (the existing struct in
`ContentView.swift`):

- A new `Text("#\(task.shortId)")` modifier placed in the
  top-right of the card via an overlay or as the trailing element
  of the existing title `HStack`'s `Spacer`-bounded layout.
- Font: `.caption.monospacedDigit().weight(.regular)` — monospaced
  digits keep `#42` and `#43` the same width when scanning a
  column, regardless of font.
- Foreground style: `.secondary` so the badge is present but does
  not compete with the card's title (which is `.body.weight(.medium)`).
- No background pill in v1 — the colour and position do the work.
  A pill was considered but adds visual weight that crowds the
  card on narrow lanes. If contrast complaints come in we add a
  subtle `.quaternary` rounded-rectangle background.
- The badge is **not draggable** independently — clicking it
  selects the card (same as clicking anywhere else on the card).
  Right-click (or a long-press on the trackpad) on the badge
  copies the number to the clipboard (as `#42`, including the
  `#`) with a brief flash to confirm; this is the one piece of
  badge-local interaction. The context-menu item label is
  `Copy ID` — not `Copy Friendly ID`.
- Accessibility: `accessibilityLabel("ID 42")` so VoiceOver reads
  it as "I D forty-two," not as a hash sign and not as "friendly
  id forty-two." Tooltip on hover is just `#42` (no extra prose).

Beyond the card:

- **Detail popover (`WorkCardPopoverView`):** the header line gets
  `#42` to the left of the title, same style. If the popover's
  metadata grid surfaces both ids side by side (only when the
  user has opted in via a "Show primary ID" toggle in app
  settings, mirroring the CLI's `--with-primary-id` flag from
  Q5), the row labels are `ID` (for the friendly id) and
  `Primary ID` (for the long form) — never "Friendly ID."
- **`DesignsView`:** when a project's design doc is open in the
  in-app browser, the header shows `#<project-short-id> <name>`
  similarly. No "Friendly ID" prefix.
- **Worker pane title / kanban column counts:** worker pane
  titles include `#42` (the friendly id of the work item the
  worker is on) prefixing the work-item name; column counts
  stay numeric (no friendly-id influence). Nothing in either
  surface is labelled "Friendly."
- **No badge on the dispatch-events viewer or transcripts** —
  those surfaces speak engine-internals where the primary id is
  the one the user wants for grepping. They keep printing the
  primary id with no friendly-id decoration.

**User-facing strings, in summary:** the number is presented as
`#42` with no qualifier. Labels are `ID` (short) or `Primary ID`
(long form) on the rare occasion a side-by-side label is needed.
The word "friendly" never reaches the user.

**Per-card or per-product prefix?**

Inside the kanban (a single-product surface) the badge is just
`#42`. The product header at the top of the kanban already names
the product, so the badge is unambiguous in context. The Q1
cross-product display convention (`Boss #42`) only kicks in in
surfaces that mix products — chat with the coordinator, multi-product
search results, future cross-product views. Inside the kanban
those don't apply.

---

### Q7 — Coordinator referral protocol

**Answer:** Three pieces:

1. **Wire-level:** the existing `Task` / `Project` protocol structs
   gain a `short_id: i64` field. `WorkTree` rows (the kanban data
   feed delivered via `Subscribe`) carry it. Every JSON-emitting
   CLI verb includes it. The coordinator reads `short_id` directly
   from the same data structures it already reads `id` from.
2. **Engine-side guidance:** every `boss task create` /
   `boss chore create` / `boss project create` JSON response
   includes `short_id` in its returned row. The coordinator, when
   it files an item on the user's behalf, learns the friendly id
   the same turn and can quote it back.
3. **Coordinator-side rule (CLAUDE.md addition):** a short
   paragraph in `.claude/CLAUDE.md` (the worker / coordinator
   instructions, already a checked-in file) instructing the
   coordinator to *prefer the friendly id when speaking to the
   user* and to fall back to the primary id only when:
   - the friendly id would be ambiguous across products in the
     same chat turn (use `Boss #42` / `Flunge #7` in that case);
   - the user explicitly asked for the primary id;
   - the coordinator is producing output the user will paste into
     a system that doesn't speak friendly ids (a SQL query
     window, a debugger, a non-Boss tool).

The CLAUDE.md addition spells out the format: `#42` for in-product
chat, `Boss #42` for cross-product chat, `task_…` whenever the
above exceptions apply. It also reminds the coordinator to surface
the primary id parenthetically the first time a new item is filed
in a session, so the user has the canonical handle if they need
to grep for it: "Filed `#42` (task_18aefd71a9458550_17) for the
migration."

The CLAUDE.md change is part of the same PR that ships the v1
implementation (so the moment the column exists, the coordinator
knows to use it). It is a small addition, not a rewrite of the
file; the existing rules around workspace lifecycle, PR semantics,
etc. are untouched.

---

### Q8 — Failure modes and edge cases

**`short_id` collision under concurrency.**
SQLite's transaction serialisation makes the allocator atomic; the
`UNIQUE (product_id, short_id)` index is a belt-and-braces guard.
If the index ever fires (it shouldn't, but the unit test in Q9
includes a concurrent-create test that asserts the index is the
safety net), the second writer's transaction is rolled back with a
`UNIQUE constraint failed` error. The caller retries by re-entering
the same `create_task` path, which re-reads the counter. No data
corruption.

**Counter row missing for a product.**
A product that exists but has no `short_id_sequences` row is
treated as "next is 1." The allocator's `optional()` + `unwrap_or(1)`
handles this. Fresh-product creates initialise the counter at the
same time as the product row.

**Backfill on a corrupt DB.**
If the backfill encounters two rows with the same `(created_at, id)`
(impossible given `id` is unique, but defensively), they get
adjacent numbers in arbitrary order. The migration logs which rows
got which numbers.

**Friendly id reuse after delete.**
We do not reuse. Soft-deleted rows keep their friendly id; the
counter never decrements. If a user manually `DELETE`s a row from
SQLite (an unsupported operation), the gap stays.

**User types `0` or a negative number.**
`boss task show 0` errors with `id must be >= 1`. Negative numbers
fail at integer-parse time with the same kind of error the CLI
emits today for any malformed argument. (Internal prose in this
doc still uses "friendly id" to disambiguate from the primary id;
the user-facing message just says "id" per Q6.)

**User types a friendly id that doesn't exist in the current product.**
`boss task show 999` errors with `no task with id #999 in product
boss`. The error names both the id and the product, so the user
knows which product was searched.

**User types a friendly id that exists in a different product.**
The CLI uses the current product (resolved per Q5) and only
searches there. If the user wants to look up a friendly id in
another product, they pass `--product` or write
`<slug>/#<n>`. We do not auto-search across products on miss
(would be slow at scale and would mask the real "you're in the
wrong product" mistake).

**Two products with the same friendly id (legal):**
Boss `#42` and Flunge `#42` coexist. The cross-product display
prefix (Q1, Q7) resolves the ambiguity wherever both appear in the
same context. The CLI never returns ambiguous results because it
always operates on one product at a time.

**A primary id that happens to be all digits.**
Impossible — primary ids always start with a known prefix (`task_`,
`chore_`, `proj_`, `prod_`, `run_`, `exec_`, `attn_`, `paud_`,
`crz_`, `esc_`, `test`, etc.). The selector parser checks for a
leading digit and only does friendly-id lookup if so. Anything that
contains a `_` falls into the primary-id path.

**Future row kinds.**
A new `tasks.kind` (e.g. `'spike'`) automatically inherits the
sequence because the allocator keys on `product_id`, not on `kind`.
A new table for some future entity (e.g. `incidents`) would need
its own decision; the precedent we're setting is "if it's a
kanban-visible work item, give it a `short_id` from the per-product
sequence."

---

### Q9 — Implementation plan and follow-up chores

A single PR for v1 is reasonable in size but not trivial; the work
breaks naturally into the following implementation chores, each
fitting one worker session. They are not ordered with hard
sequencing — schema goes first, but the CLI, app, and CLAUDE.md
work can interleave once the schema lands.

1. **Schema + migration + allocator.**
   - Add `short_id` columns to `tasks` and `projects`.
   - Add `short_id_sequences` table.
   - Add unique indexes.
   - Implement `allocate_short_id(tx, product_id) -> i64`.
   - Implement `migrate_short_id_columns` (Q4 backfill rules).
   - Wire allocator into every `WorkDb::create_*` path.
   - Unit tests: allocator atomicity (concurrent inserts produce
     distinct ids), backfill determinism (same input → same
     numbers), index enforcement (manual conflict raises).
   - **Acceptance:** fresh DB and migrated DB both expose dense
     friendly ids; primary ids unchanged.

2. **Protocol + RPC + JSON wire.**
   - Add `short_id: i64` to `Task` and `Project` protocol structs.
   - Update every JSON-emitting CLI verb to include it.
   - Update `WorkTree` so Subscribe pushes it.
   - Mirror in `Models.swift` (the macOS app's protocol types).
   - **Acceptance:** every existing wire test passes; new wire tests
     cover round-trip of the field.

3. **CLI lookup semantics.**
   - Implement the selector grammar in Q5 (strip leading `#`, parse
     decimal, fall through to primary-id and slug paths).
   - Add `<product_slug>/#<n>` and `<product_slug>/<n>` forms.
   - Add the "wrong kind" error (Q5: `chore show 42` on a task row).
   - Update `boss task show` / `boss task list` / `boss chore show`
     / `boss chore list` / `boss project show` / `boss project list`
     human formatters to render `#<n>` prominently.
   - Help text in each verb names the friendly-id form.
   - **Acceptance:** CLI integration tests cover every selector
     shape; existing primary-id-based tests still pass.

4. **macOS kanban badge.**
   - Add the badge to `WorkBoardCardView` per Q6.
   - Snapshot tests for the badge presence and position on each
     column.
   - Add the friendly id to `WorkCardPopoverView` header.
   - Wire the right-click "copy friendly id" menu item.
   - **Acceptance:** snapshot tests pass; manual test confirms badge
     readability on every lane.

5. **CLAUDE.md coordinator rule.**
   - Add the section to `.claude/CLAUDE.md` per Q7. Specifically a
     "Referring to work items" subsection that documents the
     friendly-id-first rule, the cross-product `Boss #42` prefix,
     the parenthetical-primary-id-on-first-mention rule, and the
     exceptions list.
   - **Acceptance:** the coordinator's next chat about a filed work
     item uses the friendly id by default. Verified by hand on the
     branch's own follow-up chores.

6. **(Optional follow-up) `boss admin renumber-product`.**
   - One-shot CLI verb that re-runs the backfill for a single
     product. Useful if the user ever hand-edits the DB and wants
     to re-densify the numbers. Not in v1; documented as a future
     escape hatch.

7. **(Optional follow-up) Cross-product `Boss #42` parser.**
   - Extend the selector grammar to accept `<slug>/#<n>` from any
     CLI verb (not just task / chore / project show). Tackled when
     the first user complaint about cross-product lookup arrives.

8. **(Optional follow-up) Filter / sort by `short_id`.**
   - `boss task list --sort short-id` and `--filter
     short-id=<n..m>`. Cheap; ship if anyone asks.

The implementation chores get filed as `kind = 'chore'` rows
against the Boss product once this design is approved. Each
references this doc by path; the design pointer (per
`project-design-doc-pointer.md`) is set to
`tools/boss/docs/designs/friendly-numeric-ids-for-work-items.md`.

---

## Schema summary

```sql
-- New column on numbered tables.
ALTER TABLE tasks    ADD COLUMN short_id INTEGER;
ALTER TABLE projects ADD COLUMN short_id INTEGER;

-- Per-product allocator.
CREATE TABLE short_id_sequences (
    product_id  TEXT PRIMARY KEY REFERENCES products(id),
    next_value  INTEGER NOT NULL DEFAULT 1
);

-- Uniqueness per product, ignoring NULLs (migration-safe; new rows
-- are never NULL after the allocator wires up).
CREATE UNIQUE INDEX tasks_product_short_id_idx
    ON tasks(product_id, short_id) WHERE short_id IS NOT NULL;
CREATE UNIQUE INDEX projects_product_short_id_idx
    ON projects(product_id, short_id) WHERE short_id IS NOT NULL;
```

### Why `INTEGER`, not `TEXT`

A reviewer raised the question of whether `short_id` should be a
string column for future flexibility — e.g. if we ever want to
switch the display format from decimal to hex, or add a product
prefix like `B-42`. We stay with `INTEGER`. Reasons:

- **The allocator is integer-native.** The counter table
  (`short_id_sequences.next_value`) is an integer; the sequence
  invariant (`next_value > MAX(short_id)` per product) is an
  integer comparison; the read-modify-write cycle in
  `allocate_short_id` does `next + 1` arithmetic. A string column
  would force conversion at the read and the write of every
  insert, and the `MAX()` / ordering queries that the allocator
  and the kanban sort path rely on would have to either lex-sort
  (wrong for unpadded numbers — `"10" < "2"`) or cast at query
  time. None of that buys anything the integer column can't do.
- **Display format is a presentation concern, not a storage
  concern.** Hex vs decimal vs `B-42` is decided in the
  renderer — the macOS app's `WorkBoardCardView`, the CLI's
  human formatter, the coordinator's chat template. The reviewer
  noted this themselves ("I guess we could just do that as a
  display filter"). Changing the display format does not require
  a migration; changing the storage type would.
- **Cheap aggregates and ordering.** `MAX(short_id)`,
  `ORDER BY short_id`, and range filters (the future
  `--filter short-id=42..99` opt-in chore in Q9) are all
  trivial on an integer column and awkward on a text column.
- **No range or precision concern.** SQLite's `INTEGER` is a
  variable-width signed int up to 8 bytes (i.e. effectively
  i64). At Boss's write rate it would take longer than the
  lifetime of the engine to exhaust the space.

We acknowledge the reviewer's flexibility concern explicitly:
the column stays integer because display formatting covers the
realistic flexibility use cases without a storage type change.
If a future requirement somehow needed non-numeric `short_id`
content (a UUID-shaped identifier, a hash, anything that does
not parse as `i64`), that would be a different feature with a
different name, not a v2 of this column.

## Wire summary

```rust
// protocol/src/types.rs
pub struct Task {
    pub id: String,
    /// Per-product short id (Q1). Always present after the
    /// migration runs; the `Option` is for the brief migration
    /// window only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<i64>,
    /* … existing fields … */
}

pub struct Project {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<i64>,
    /* … existing fields … */
}
```

No new RPC verbs. No new attention-item kinds. No new events. The
`work_item_changed` event the kanban subscribes to already covers
row-content changes; the `short_id` column rides along.

## Risks / open questions

**R1 — The `WHERE short_id IS NOT NULL` index isn't supported on
older SQLite.** Boss's bundled `rusqlite` is recent enough that
partial indexes are fine; double-check at implementation time. If
not, drop the predicate and rely on the migration backfilling every
row before any index queries see a NULL.

**R2 — Backfill on a very large existing DB is O(N) in writes.**
For Boss's current scale (~100 work items) this is trivial. The
migration is one-shot at boot; if a Flunge or work-tracker DB has
millions of rows the migration is still small in absolute terms
(one update per row), but worth a sanity check before shipping.

**R3 — Two `boss task create` calls landing in the same nanosecond
across two processes.** SQLite serialises writers; the second
transaction blocks until the first commits, then reads the updated
counter. Verified by the concurrent-create unit test in chore 1.

**R4 — Soft-deleted rows showing up in `boss task show <n>`.**
Default behaviour: deleted rows are excluded. `--include-deleted`
opt-in reveals them with their original friendly id intact. Matches
the existing `--include-deleted` semantics on `task list`.

**R5 — The coordinator forgets to switch to friendly ids.** The
CLAUDE.md addition is advisory; the model has to read it and
internalise it. We expect a few sessions of friction before the
behaviour is consistent. Mitigation: a periodic `bossctl probe`
that reminds the coordinator if it sees the coordinator quoting a
raw primary id in chat without disambiguation. v1 ships without
the probe; we add it if the friction is real.

**R6 — Numbers grow unboundedly.** A long-running product
eventually has `#100000` cards. They're still glanceable, but the
badge gets wider. No mitigation in v1; if it ever bites we can
add a per-product "reset" verb that renumbers after a
quarterly cleanup (this is intentional sugar — most issue trackers
let this number grow forever and it's fine).

**R7 — Coordination with `design-producing-tasks` and
`project-design-doc-pointer`.** Those designs add columns to
`tasks` and `projects`; this design adds one more column to each.
Migrations are commutative — they touch different columns. The
order in which they ship is irrelevant.

**R8 — Multi-user / multi-engine in the future.** A second engine
process pointed at the same SQLite file would serialise writes
correctly. A second engine process with its *own* SQLite file
would have its own per-product counters, which is the expected
behaviour. If we ever consolidate to a single shared DB across
multiple engine instances, the allocator just keeps working
because it's transactional.

**R9 — Existing primary-id references in PR descriptions, Slack,
old commits.** Out of scope per Non-Goals. The coordinator's
new referral rule (Q7) only applies to new chat output; backfilling
historical mentions would be expensive and lossy. New mentions
get friendly ids; old mentions stay primary.

**R10 — Sequence skew between tasks and projects.** Because Q1's
per-product sequence shares across kinds, the sequence "1, 2, 3,
…" interleaves task and project creations in whatever order they
were inserted. A user reading `boss project list` sees their
projects with friendly ids like `#1, #4, #7` (tasks took the
intervening numbers). This is expected and fine — the friendly id
is a per-product identifier, not a per-kind counter — but worth
flagging in the user-facing docs / help text so it doesn't
surprise.

**R11 — What if the user wants per-kind sequences after all?**
Out of scope for v1, but the schema would accommodate it: replace
the `short_id_sequences` key from `product_id` to
`(product_id, kind)` and re-backfill. The unique index becomes
`(product_id, kind, short_id)`. A one-shot migration.

**R12 — What about `products` themselves having friendly ids?**
Not in v1. Boss has two products; the user calls them by slug
("boss", "flunge"). If Boss ever has dozens of products, we
revisit. The cost of adding it later is one more column on one
more table.

