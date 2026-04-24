# Ghostty SwiftUI Prototype

This is a small macOS SwiftUI prototype that embeds multiple `libghostty`
surfaces inside a single main window.

It intentionally stays narrow:

- one window
- five terminal surfaces in one row
- minimal AppKit event forwarding
- no tabs, splits, config UI, or full IME/preedit support

## Bootstrap

The package links against the upstream `GhosttyKit.xcframework`, but that
binary is **not** checked into this repo.

Build it locally first:

```bash
./scripts/bootstrap-ghosttykit.sh
```

That script clones `ghostty-org/ghostty`, builds the native macOS
`GhosttyKit.xcframework`, and places it at:

```text
tools/ghostty-proto/app-macos/ThirdParty/GhosttyKit.xcframework
```

## Run

```bash
swift run GhosttyProtoApp
```

Once the window is up, the app will auto-launch five Claude sessions, one in
each embedded terminal pane.

## Notes

- Each terminal surface is a custom `NSView` passed to `ghostty_surface_new`.
- The prototype uses the upstream C embedding API exposed by `GhosttyKit`.
- The SwiftUI host manages the pane layout itself; it does not use Ghostty's
  built-in split tree APIs.
- The prototype can observe per-pane terminal state heuristically from visible
  contents, but it does not yet expose terminal output as a structured Swift stream.
- Keyboard handling is intentionally lightweight; it is suitable for a prototype,
  but not a complete replacement for Ghostty's production macOS input stack.
- Upstream Ghostty currently builds `GhosttyKit` in CI via `nix develop -c zig build ...`.
  On Xcode 26.4 hosts, upstream also documents a Zig 0.15 linker issue unless you
  use their Nix flake or Homebrew's patched `zig@0.15`.
- The bootstrap script prefers Homebrew's `zig@0.15` when available, otherwise it
  falls back to `zig` on `PATH`, then a cached Zig 0.15.2 download.
- The build also requires Xcode's Metal toolchain component. If `xcrun metal` is
  missing, install it with `xcodebuild -downloadComponent MetalToolchain`.
