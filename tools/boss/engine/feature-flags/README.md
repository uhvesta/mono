# Engine feature flags

`boss-feature-flags` provides toggleable on/off switches for optional or
risk-bearing engine behaviours. Each flag is a human-flippable kill switch:
the engine consults it on every consumer-side check, so flipping a flag
changes behaviour live with no rebuild and no redeploy. Flags surface in the
macOS app under **Debug → Feature Flags**; toggling one there sends a
`set_feature_flag` RPC to the engine, which honours the new value immediately.

This system is the implementation of incident 001 action item #5; see
`tools/boss/docs/postmortems/incident-001-pr-fan-out.md` §5 for the
motivation. In short: any engine behaviour that is (a) optional-for-correctness,
(b) has non-trivial blast radius when wrong, and (c) is hard to reason about
should be gated behind a flag the human can flip the moment they see it
misbehaving.

## Architecture

There are three pieces. A `const` **registry** is the single source of truth
for which flags exist, their human-readable descriptions, their grouping
category for the debug pane, and their defaults. A thread-safe **store**
holds an in-memory map of human overrides keyed by flag name, falling back
to the registry default for any flag the human has not touched, and mirrors
that map to a TOML file. Consumers call one method — `is_enabled("name")` —
at the site they want to gate.

`boss-feature-flags` is a leaf crate with no internal dependencies. It is
consumed by `boss-engine`, which re-exports it as `crate::feature_flags`,
constructs the store at boot from the Boss state root, and shares an
`Arc<FeatureFlagsStore>` across the subsystems that need it (the completion
handler, the runner, and the `ServerState` that backs the RPC handlers). The
debug-pane `list_feature_flags` / `set_feature_flag` RPCs operate against the
registry generically — they never name individual flags, so adding a flag
needs no protocol or UI change.

## Adding a flag

Two edits. Both are one line.

**1. Register the flag.** Append a `FeatureFlagSpec` to the `REGISTRY`
slice in this crate's `src/lib.rs`:

```rust
pub const REGISTRY: &[FeatureFlagSpec] = &[
    // … existing entries …
    FeatureFlagSpec {
        name: "your_flag_name",
        description: "What this flag does and what the off-position means.",
        category: "completion", // group label in the debug pane
        default_enabled: true,  // see §Safety contract
    },
];
```

**2. Check it at the consumer site.** Anywhere the engine wants to
gate optional behaviour:

```rust
if !self.feature_flags.is_enabled("your_flag_name") {
    return /* the off-path outcome */;
}
```

`feature_flags` is `Arc<FeatureFlagsStore>`; the completion handler
holds one (`with_feature_flags(...)`), and any subsystem that needs
its own can take an `Arc<FeatureFlagsStore>` parameter from
`ServerState::feature_flags`.

That's it. No protocol changes, no UI changes — the debug pane lists
every registered flag automatically, and the engine RPC handlers
(`list_feature_flags`, `set_feature_flag`) operate against the
registry without knowing which flags exist.

## Safety contract

**Default ON unless the flag is known-risky.** A new flag's default
governs the behaviour of every install that has never opened the
debug pane. If you flip the default to OFF, every running engine
that hasn't toggled the flag loses the gated behaviour on the next
release. The default-ON rule means:

- Most flags wrap a feature in a "kill switch" — leaving the feature
  ON for everyone, available to be killed by hand.
- A flag whose default is OFF is really a "manual opt-in" feature.
  Use sparingly; most things that are off-by-default should not be
  in the codebase yet.

**The flag is the kill switch, not the design.** Flags are for cases
where you want a human-toggleable escape hatch from a behaviour that
is broadly correct but occasionally wrong. They are not for permanent
configuration knobs (use the metadata KV or a config file for those)
and they are not for hiding incomplete features (don't ship code
that doesn't work yet).

## File format and race semantics

The store writes its state to
`~/Library/Application Support/Boss/feature-flags.toml` (a temp dir in
tests). The file is a flat `flag_name = bool` mapping; flags that are
absent fall back to the registry default. Example:

```toml
detect_pr_cold_fallback = false
```

Writes use a temp-then-rename, so a concurrent reader either sees the
old file or the new one — never a half-written file. The in-memory
map is the source of truth between reads; `set` updates the map
*before* it writes, so the next consumer-side `is_enabled` call sees
the new value the instant the RPC returns.

Stale entries in the file (names that no longer match any registered
flag) are dropped on load. Unknown flags are forward-compat: an older
engine binary loading a file with a future flag silently ignores it. A
malformed file makes `load` return an error rather than panic, so the
engine can log and continue on its existing in-memory state.
