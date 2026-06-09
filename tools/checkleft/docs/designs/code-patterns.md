# Checkleft: `code-patterns`

## Overview

Checkleft should support a built-in, language-aware check for fairly static,
high-risk code patterns that are expensive to catch in review and
straightforward to describe as "do not call this API shape here."

The motivating examples are blocking async waits without timeouts, such as:

```java
CompletableFuture<String> future = someAsyncMethod();
String result = future.get();
```

and:

```java
Task<Result> task = xxx;
engine.run(task);
task.await();
Result result = task.get();
```

This fits Checkleft well because the framework already runs fast,
change-scoped, deterministic built-in checks, and it already has a
tree-sitter-based parser path for Starlark checks. The first implementation
should target Java, but the user-facing model should leave room for future
language-specific pattern families under one check name.

## Goals

- Add one built-in language-aware code-pattern check rather than a family of
  narrow language-specific one-offs.
- Support a compact rule syntax for "forbidden call" patterns such as
  `java.util.concurrent.Future#get()`.
- Catch no-timeout blocking waits on common async primitives such as
  `Future`, `CompletableFuture`, and ParSeq `Task`.
- Keep evaluation fast enough for local edit loops and CI presubmit use.
- Preserve Checkleft's normal severity, remediation, and bypass behavior.
- Leave room to generalize the model across languages later without
  committing to a large semantic-analysis framework now.

## Non-Goals

- Full Java compilation or complete symbol resolution.
- A generic tree-sitter query language exposed directly in config.
- Whole-program or cross-repository analysis.
- Auto-fixing arbitrary Java call sites in v1.
- A first pass that supports every Java syntactic edge case.
- A first pass that supports neutral matcher primitives such as `call`,
  `constructor`, or `field_access`.
- A first pass that supports more than one source language.

The first version should stay intentionally small: one check named
`code-patterns`, one supported language value `java`, one matcher family named
`nocall`, and lightweight type resolution that is good enough for the
intended static patterns.

## Why A Built-In Check

This feature should be implemented as a built-in Checkleft check rather than
as an external package:

- the feature needs direct access to Checkleft's change-scoped file model,
- lightweight parsing and type resolution are likely to be reused by future
  built-in code-pattern policies.

This is also a better fit for Checkleft than a purely text-based check because
string matching alone is too weak for the intended rules. For example, we need
to distinguish:

- `Future#get()` from `Future#get(long, TimeUnit)`,
- `Task#await()` from timeout-bearing overloads,
- `foo.get()` on an unrelated type from `get()` on a `Future`,
- `foo().get()` when the call result resolves to a forbidden type.

## Proposed User-Facing Model

### Check Name

Add a new built-in check named `code-patterns`.

### Rule Shape

The initial rule model should be prohibition-oriented:

- `lang` is required and selects the language-specific matcher semantics.
- `nocall` is required and defines the forbidden resolved call target.
- `message` is optional.
- `remediation` is optional.
- `severity` is optional.
- top-level `message`, `remediation`, and `severity` act as defaults for all
  rules in the check instance.

YAML example:

```yaml
checks:
  - id: blocking-java-calls
    check: code-patterns
    policy:
      severity: error
      allow_bypass: true
    config:
      lang: java
      message: Blocking wait without timeout.
      remediation: Use a timeout-bearing API or keep the flow async.
      rules:
        - nocall: java.util.concurrent.Future#get()
        - nocall: com.linkedin.parseq.Task#await()
        - nocall: com.linkedin.parseq.Task#get()
```

Per-rule overrides should still be supported:

```yaml
checks:
  - id: blocking-java-calls
    check: code-patterns
    config:
      lang: java
      remediation: Use a timeout-bearing API or keep the flow async.
      rules:
        - nocall: java.util.concurrent.Future#get()
          message: Blocking Future.get() without timeout.

        - nocall: com.linkedin.parseq.Task#await()
          message: Blocking ParSeq await() without timeout.
```

The design should allow future syntactic sugar such as multiple `nocall`
patterns sharing the same metadata, but the semantic model should remain "one
pattern expands to one logical rule."

For v1, `lang: java` is the only supported language value. Other values should
fail config validation.

### `nocall` Pattern Syntax

Under `lang: java`, the `nocall` value should be a resolved instance-method
pattern:

```text
<fully.qualified.Type>#<method>(<signature>)
```

Examples:

- `java.util.concurrent.Future#get()`
- `java.util.concurrent.CompletableFuture#get()`
- `com.linkedin.parseq.Task#await()`
- `com.linkedin.parseq.Task#get()`

For v1, the grammar should be intentionally small:

- fully qualified receiver type is required,
- method name is required,
- `()` means exactly zero arguments,
- `(..)` can be reserved for a future "any argument list" form, but does not
  need to ship in v1.

`#` is preferable to `.` here because `.` already appears inside the fully
qualified type name, and the method reference is for an instance call rather
than a static member.

### Meaning Of A Match

Under `lang: java`, `java.util.concurrent.Future#get()` should mean:

- match a Java call expression,
- whose invoked method name is `get`,
- whose arity is `0`,
- whose receiver resolves to `java.util.concurrent.Future` or to a subtype or
  implementation of that type.

This allows a single rule to catch:

```java
Future<String> a = start();
a.get();

CompletableFuture<String> b = start();
b.get();

makeFuture().get();
```

provided the lightweight resolver can determine that the receiver type is
`Future`-compatible.

### Findings

The finding should point at the actual offending call site and use normal
Checkleft output fields:

- `message`
- `location`
- `remediation`
- `severity`

If `message` is omitted on a rule, the check should derive a stable default,
for example:

```text
Disallowed call to java.util.concurrent.Future#get().
```

If `remediation` is omitted on a rule, the check should use the top-level
default when present.

## Matching And Resolution Semantics

### Parsing

When `lang: java`, the check should parse changed `*.java` files with
tree-sitter-java.

We do not need a general query DSL. The check should own a small
language-specific matcher implementation for Java call expressions and keep
the configuration surface at the policy level rather than at the grammar-node
level.

### Lightweight Type Resolution

The hard part of the feature is not the pattern syntax. The hard part is
answering "what type is this call being invoked on?" without turning Checkleft
into a compiler.

For v1, the resolver should stay local and best-effort:

- resolve imported class names and same-file package names,
- resolve local variable declarations,
- resolve method parameters,
- resolve fields declared in the same file,
- resolve obvious chained return types when they are syntactically explicit,
- recognize simple inheritance and interface implementation declarations in the
  current file when available.

If the resolver cannot determine the receiver type with enough confidence, the
check should not emit a finding. False negatives are preferable to noisy false
positives for this class of policy.

### Subtype Matching

Subtype matching should be on by default for `nocall`.

That means a rule targeting `java.util.concurrent.Future#get()` should match:

- `Future#get()` directly,
- `CompletableFuture#get()` because `CompletableFuture` implements `Future`,
- project-local wrapper types only when the local resolver can prove the
  relationship.

This is important because policy authors generally think in terms of the API
contract being blocked, not every concrete implementation type.

### Change Scope

The check should remain change-scoped:

- inspect only changed Java files by default,
- report only call sites in the changed file content,
- support `--all` sweeps through Checkleft's existing all-files mode.

This keeps the runtime aligned with the rest of Checkleft.

## Why `nocall` Instead Of `call`

The config could have been modeled as:

```yaml
rules:
  - call: java.util.concurrent.Future#get()
    action: forbid
```

That is more generic, but it adds ceremony without adding value for the first
target use case. The current design chooses `nocall` because:

- the user intent is directly prohibition-oriented,
- most initial rules are expected to forbid a call rather than describe a
  neutral matcher,
- the config reads more like repository policy and less like an abstract rule
  engine.

We should treat this as a deliberate v1 choice, not a permanent limit. If
Checkleft later needs broader matcher primitives across languages, `nocall`
can be evolved or mechanically translated into a more general `call + action`
model.

## Why Not Generic Tree-Sitter Queries

Exposing raw tree-sitter queries in config would be flexible, but it is the
wrong user-facing abstraction for this feature.

Problems with a query-first design:

- rule authors would need grammar-level Java syntax knowledge,
- type-sensitive matching would still need a separate ad hoc mechanism,
- config would become brittle against parser details,
- findings would be harder to explain in policy terms.

For Java, `nocall: java.util.concurrent.Future#get()` is the right
abstraction level: small, readable, policy-oriented, and still implementable
with AST-backed matching under the hood.

## Auto-Fix Strategy

The check should not promise general auto-fixes in v1.

For blocking waits without timeouts, there is rarely one universally correct
replacement because the correct timeout value, timeout unit, and async control
flow depend on local context and repository-specific guidance.

The check can still emit strong remediation text, but it should avoid claiming
that a machine-applicable fix exists unless the replacement is truly safe and
obvious for a specific rule family.

## Config Validation

The check should reject invalid config up front, including:

- missing `lang`,
- unsupported `lang` values,
- missing or empty `rules`,
- a rule without `nocall`,
- an empty `nocall` value,
- malformed `nocall` syntax,
- empty `message` or `remediation` strings when those keys are present,
- unsupported extra matcher keys in the same rule.

This should keep failures deterministic and make rule authoring reviewable.

## Implementation Sketch

Add a new built-in check module:

```text
tools/checkleft/src/checks/code_patterns.rs
```

Proposed internal direction:

```rust
struct CodePatternsConfig {
    lang: String,
    rules: Vec<JavaPatternRuleConfig>,
    message: Option<String>,
    remediation: Option<String>,
    severity: Option<String>,
}

struct JavaPatternRuleConfig {
    nocall: String,
    message: Option<String>,
    remediation: Option<String>,
    severity: Option<String>,
}

struct CompiledNoCallPattern {
    receiver_type: String,
    method_name: String,
    arity: AritySpec,
}
```

Check flow:

1. Parse config, require `lang: java`, and compile `nocall` patterns.
2. Iterate changed Java files.
3. Parse each file into an AST.
4. Walk call expressions and build a lightweight type environment.
5. Resolve the receiver type for each candidate call.
6. Match the resolved call target against compiled `nocall` rules.
7. Emit one finding per matched call site.

## Future Evolution

This design intentionally leaves room for later growth:

- richer call signatures such as `(..)`,
- additional language values under `code-patterns`,
- static-call matching,
- constructors, field accesses, or other Java matcher kinds,
- explicit `call` matcher primitives with separate actions,
- limited autofix templates for narrowly defined rule families,
- rule-level allowlists or enclosing-context exceptions.

Those should be added only when a concrete use case demands them. The first
implementation should prove that `nocall` plus lightweight resolution is
useful before turning `code-patterns` into a broader language-aware policy
engine.
