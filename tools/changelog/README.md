# changelog

`changelog` is a path-scoped, GitHub-matching changelog generator. It
reads a git repository's commit history between two tags or refs, filters
to only the commits that touched files you own, and emits output in the
same Markdown format GitHub uses for auto-generated release notes. The
result can be pasted directly into a GitHub release or piped to a file.

The path-scoping is the key feature: in a monorepo every release tag
spans the whole tree, but most projects only care about the slice of
history that touched their files. `changelog` lets you pass one or more
glob patterns (or point it at a `PROJECT.yaml`) and it silently drops any
PR that did not touch at least one matching path, so the notes stay
relevant even when the tag was cut across dozens of unrelated changes.

## Build

```sh
bazel build //tools/changelog
```

The resulting binary is `bazel-bin/tools/changelog/changelog`. You can
also run it directly through Bazel:

```sh
bazel run //tools/changelog -- --from v1.0.0
```

## Usage

```
changelog --from <TAG> [--to <REF>] [--path <GLOB>]... [OPTIONS]
```

The only required flag is `--from`; everything else has a sensible
default.

### Basic: all PRs between two tags

```sh
changelog --from v1.0.0 --to v1.1.0
```

Scans every commit reachable from `v1.1.0` but not from `v1.0.0`,
extracts PR numbers, and prints a `## What's Changed` block to stdout.
The GitHub repo slug is derived automatically from the `origin` remote.

### Filter to a subtree

```sh
changelog --from v1.0.0 --to v1.1.0 --path 'tools/cube/**'
```

Only PRs that touched at least one file under `tools/cube/` are included.
`--path` can be repeated to union multiple patterns:

```sh
changelog --from v1.0.0 \
  --path 'tools/boss/**' \
  --path 'boss-protocol/**'
```

### Filter via a glob file

```sh
changelog --from v1.0.0 --paths-file my-paths.txt
```

`my-paths.txt` is a plain text file with one glob per line; `#` comments
and blank lines are ignored. Globs from `--paths-file` are unioned with
any `--path` flags.

### Filter via PROJECT.yaml

```sh
changelog --from v1.0.0 --project tools/cube/PROJECT.yaml
```

Reads the `PROJECT.yaml` in that directory, derives path globs from its
`paths` list (and from the directory itself), and uses those as the
filter. `--project` may be combined with `--path` / `--paths-file`; all
sources are unioned.

### Enrich with live GitHub data

By default, PR titles and author logins are taken from the git commit
message. Pass `--enrich` to fetch the canonical title and `@login` from
the GitHub API via `gh`:

```sh
changelog --from v1.0.0 --enrich
```

This requires `gh` to be installed and authenticated (`gh auth login` or
a `GITHUB_TOKEN` environment variable).

## Flags

| Flag | Default | Description |
|---|---|---|
| `--from <TAG>` | *(required)* | Start tag, exclusive lower bound. |
| `--to <REF>` | `HEAD` | End tag or ref, inclusive upper bound. |
| `--path <GLOB>` | *(none)* | Include only commits touching this glob. Repeatable. |
| `--paths-file <FILE>` | *(none)* | File of glob patterns (one per line; `#` comments ok). |
| `--project <FILE>` | *(none)* | `PROJECT.yaml` whose directory and `paths` entries define owned globs. |
| `--repo <OWNER/NAME>` | *(from `origin` remote)* | GitHub repo slug. |
| `--enrich` | `false` | Fetch real PR titles and author logins via `gh api`. |
| `--git-dir <PATH>` | `.` | Path to the git repository. |

## Output format

The output matches GitHub's auto-generated release notes format:

```markdown
## What's Changed
* Add cool feature by @alice in https://github.com/owner/repo/pull/42
* Fix nasty bug by @bob in https://github.com/owner/repo/pull/41


**Full Changelog**: https://github.com/owner/repo/compare/v1.0.0...v1.1.0
```

If no matching PRs are found the entries section reads `Nothing significant!`.

## How it works

`changelog` runs `git log <from>..<to>` and scans each commit message for
two PR patterns:

- **Squash merge** — subject ends with `(#N)`: `Add feature (#42)`
- **Merge commit** — subject matches `Merge pull request #N from …`; the
  PR title is taken from the first non-empty line of the commit body.

Commits that do not match either pattern are skipped. Duplicate PR
numbers (e.g. both the squash commit and a revert) are deduplicated by PR
number. When path globs are active, `git diff-tree` is called for each
candidate commit to get the touched file list; commits whose files do not
match any glob are dropped before adding to the output.

When `--enrich` is set, a `gh api` call fetches the canonical PR title
and author `@login` from GitHub for each entry, overwriting the
git-derived values. Enrichment failures print a warning to stderr and
leave the entry unchanged.
