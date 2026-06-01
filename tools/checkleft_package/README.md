# checkleft-package

`checkleft-package` is the packaging library behind the `checkleft`
repository-convention checker. It produces a self-contained, redistributable
source archive of `checkleft` that can be built outside this monorepo as a
standalone Cargo workspace and Bazel root module. It exists so that `checkleft`
can be shipped and consumed elsewhere without dragging along the surrounding
monorepo's workspace inheritance or Bazel wiring.

## How it fits

The crate is a support library paired with a thin standalone `checkleft-package`
binary. The library exposes a small surface for locating the monorepo root and
emitting a `checkleft-<version>-source.tgz` tarball; the binary parses an
optional `--output` path and drives the library. It depends only on third-party
crates, none from the monorepo.

The core responsibility is *manifest flattening*. Inside the monorepo,
`checkleft`'s `Cargo.toml` inherits its version, edition, and dependency
versions from the workspace (`workspace = true` / `*.workspace = true`).
Packaging rewrites that manifest into a fully resolved, standalone form:
workspace-inherited package fields and dependency specs are replaced with their
concrete values pulled from the root manifest, local dependency features are
merged with the inherited ones, and a single-member `[workspace]` is
synthesized so the archive is buildable on its own.

Around the flattened manifest, the crate stages the rest of a buildable tree —
copying `checkleft`'s `src`, `api`, license, readme, and the pinned
`Cargo.lock`/`.bazelversion`, then generating the Bazel files needed for a
standalone Bzlmod build: `MODULE.bazel`, `BUILD.bazel`, a rewritten
`bazel/defs.bzl` whose default target points at the archive root, and a
`BAZEL_CONSUMPTION.md` explaining how downstream roots register a compatible
Rust toolchain. The staged tree is then written to a deterministic gzip-
compressed tar archive (fixed mtimes and modes) so repeated packaging of
unchanged sources is reproducible.

Toolchain and ruleset versions baked into the generated Bazel files
(`rules_rust`, `aspect_rules_js`, the module compatibility level) are pinned as
constants in this crate and must be kept in step with the monorepo's own Bazel
configuration.
