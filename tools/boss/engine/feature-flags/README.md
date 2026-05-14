# Engine feature flags

Toggleable on/off switches for optional or risk-bearing engine
behaviours. Surfaced in the macOS app under **Debug → Feature Flags**;
toggling a flag there sends a `set_feature_flag` RPC and the engine
honours the new value on every subsequent consumer-side check — no
rebuild needed.

This system is the implementation of incident 001 action item #5; see
`tools/boss/docs/postmortems/incident-001-pr-fan-out.md` §5 for the
motivation. In short: any engine behaviour that is
(a) optional-for-correctness, (b) has non-trivial blast radius when
wrong, and (c) is hard to reason about should be gated behind a flag
the human can flip the moment they see it misbehaving.

## Adding a flag

Two edits. Both are one line.

**1. Register the flag.** Append a `FeatureFlagSpec` to the `REGISTRY`
slice in `tools/boss/engine/src/feature_flags.rs`:

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
`~/Library/Application Support/Boss/feature-flags.toml`. The file is a
flat `flag_name = bool` mapping; flags that are absent fall back to
the registry default. Example:

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
engine binary loading a file with a future flag silently ignores it.

## The first flag

`detect_pr_cold_fallback` (default ON, category `completion`). When
OFF, the engine skips the `detect_pr` cold-path fallback in
`on_stop_inner` and `recheck_for_pr` — empty-staging falls through
to "no PR pushed" and the chore stays in `waiting_human` for the
human to resolve. This is the kill switch for the misbehaviour
documented in incident 001. Pair with action item #6 (the structural
fix to `detect_pr` itself) once that lands.
