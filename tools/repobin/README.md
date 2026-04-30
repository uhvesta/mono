# repobin

Status: experimental / under active development. The CLI behavior and config
format may change without notice.

`repobin` installs lightweight commands onto your `PATH` that dispatch to
repo-defined Bazel binaries in the current working directory.

## Install

```bash
cargo install repobin
```

## Usage

Check in a repo-root `REPOBIN.toml`:

```toml
version = 1

[tools.boss]
target = "//tools/boss/cli:boss"

[tools.cube]
target = "//tools/cube:cube"
```

Then install the `repobin` binary plus tool symlinks:

```bash
bazel run //tools/repobin:repobin -- install
repobin install
repobin install --bin-dir ~/.local/bin
```

If you use `direnv`, a lightweight setup is to make sure the same install
directory is on `PATH` while you are in the repo:

```bash
export REPOBIN_BIN_DIR="${REPOBIN_BIN_DIR:-$HOME/bin}"
PATH_add "$REPOBIN_BIN_DIR"
```

That keeps `boss`, `cube`, and other configured commands available without
having `.envrc` mutate global install state on directory entry.

Once installed, invoking a configured tool from inside that repo will:

1. find the nearest `REPOBIN.toml`,
2. build the configured Bazel target,
3. resolve the runnable executable from Bazel metadata,
4. replace the current process with the built binary.

Examples:

```bash
boss task list
cube workspace lease mono --task "prepare repobin publish"
repobin doctor
repobin list
repobin exec boss -- task list
```

## Default mode

When a tool is invoked from a working directory that has no matching
`REPOBIN.toml` (or the matching file does not declare that tool), `repobin`
falls back to a `repobin.yaml` peer to the installed binary:

```yaml
version: 1
tools:
  boss:
    repo: git@github.com:spinyfin/mono.git
  cube:
    repo: git@github.com:spinyfin/mono.git
```

The yaml only carries the repo URL — the canonical Bazel target lives in the
target repo's `REPOBIN.toml` and is read from the cached checkout after
refresh, so renaming a target in the source repo automatically takes effect
on the next default-mode invocation.

`repobin install` writes this file automatically by recording each local
tool's name against `git remote get-url origin`. Re-installing from another
repo merges new entries; existing entries are kept. Pass `--no-defaults` to
skip writing the file.

In default mode the configured repo is shallow-cloned into the cache and the
build runs from that clone (using the target declared in
`<checkout>/REPOBIN.toml`). A short notice goes to stderr so it is obvious
that the tool was built from `HEAD`:

```text
repobin: running `boss` from git@github.com:spinyfin/mono.git @ 1a2b3c4 (cloned; default mode — not in a configured workspace)
```

The cache lives at `$XDG_CACHE_HOME/repobin/repos/<slug>-<hash>/checkout` (or
`~/.cache/repobin/repos/...`). Subsequent invocations reuse that checkout: a
`fetch_stamp` gates whether to refresh (default 5 min, override with
`REPOBIN_DEFAULTS_TTL_SECS`). Past the gate, `repobin` runs `git ls-remote
origin HEAD`; if the remote sha differs from the local sha, it
`fetch --depth=1 origin HEAD` + `reset --hard FETCH_HEAD`. Concurrent
invocations serialise on a per-cache `flock`. Override the cache root via
`REPOBIN_CACHE_DIR`.

`repobin doctor` lists the active defaults file.

## Notes

- `repobin` currently supports Bazel-backed tools only.
- It expects a working `bazel` entry point on `PATH` and a `git` entry point
  for default-mode clones.
- `repobin install` defaults to `~/bin` and warns if the chosen directory is
  not on `PATH`.
- If you use `direnv`, prefer adding the chosen `repobin` bin dir to `PATH`
  rather than running `repobin install` from `.envrc`.
