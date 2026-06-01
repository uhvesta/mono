# boss-pr-template

`boss-pr-template` loads a repository's GitHub pull-request templates out of a
leased cube workspace so the engine can enforce editorial controls on
agent-authored PRs. It exists to give the rest of Boss a single, cached source
of truth for "what shape should this PR body take?" — both the template text to
hand the worker and the required sections to check the result against.

## How it fits

The crate reads `.github/PULL_REQUEST_TEMPLATE.md` (the single-file form) and
`.github/PULL_REQUEST_TEMPLATE/*.md` (the directory form) from a workspace
root. Each discovered file becomes a `PrTemplate` carrying its verbatim
markdown, its workspace-relative source path, and the set of H2/H3 headings it
declares. A `PrTemplateSet` groups the optional default template with any named
templates (keyed by lowercased file stem) and offers a stable iteration order —
default first, then named templates sorted by stem.

The two outputs serve two distinct consumers. The raw template text is injected
into the `[editorial-rules]` prompt block so a worker authors its PR in the
project's house style. The extracted required headings feed the PreToolUse
hook's `template_policy = Enforce` conformance check, which flags a PR body that
omits a required section. An empty `PrTemplateSet` (no template found) is the
signal for callers to treat the policy as effectively `Off`. Heading extraction
is markdown-aware: only ATX H2/H3 headings count, and anything inside fenced
code blocks (` ``` ` or `~~~`, matched by fence length) is ignored.

Results are cached per `(product_id, lease_id)` pair, so repeated lookups within
one execution avoid touching disk. The lease id is deliberately part of the
key: a new lease forces a fresh read, since the workspace's template may have
changed between leases.

This is a focused leaf crate with no internal Boss dependencies. It is consumed
by `boss-engine`, which owns the editorial-control policy and the hook that
applies it; the conceptual companion is `boss-editorial`, which defines the
broader rules for PR and comment text.
