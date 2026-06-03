# Writing checks

Checks are implemented in Rust and registered as built-ins.

For the high-level execution contract (what checks receive and return, hermetic expectations, and change-scoped behavior), see [Concepts](concepts.md).

## Core trait

Each check implements:

```rust
#[async_trait]
pub trait ConfiguredCheck: Send + Sync {
    async fn run(
        &self,
        changeset: &ChangeSet,
        tree: &dyn SourceTree,
    ) -> Result<CheckResult>;
}

#[async_trait]
pub trait Check: Send + Sync {
    fn id(&self) -> &str;
    fn description(&self) -> &str;
    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>>;

    async fn run(
        &self,
        changeset: &ChangeSet,
        tree: &dyn SourceTree,
        config: &toml::Value,
    ) -> Result<CheckResult>;
}
```

## Inputs

- `changeset`: changed files, kinds, and optional description metadata.
- `tree`: safe source-tree access (`read_file`, `exists`, `list_dir`, `glob`).
- `config`: the resolved `[checks.<id>.config]` table as TOML, passed once to `configure()`.

## Output contract

Return `CheckResult` with:

- `check_id`
- `findings[]`

Each finding supports:

- `severity`: `error`, `warning`, or `info`
- `message`
- `location` (`path`, optional `line`, optional `column`)
- `remediation` (optional)
- `suggested_fix` (optional)

## Authoring steps

1. Add a new module in `tools/checkleft/src/checks/`.
2. Implement `Check`.
3. Register it in `tools/checkleft/src/checks/mod.rs`.
4. Add/update `CHECKS.yaml` / `CHECKS.toml` entries to configure an instance.
5. Add tests for:
   - happy path
   - invalid config
   - non-target files
   - edge cases for path/content parsing

## Best practices

- Parse and validate config in `configure()` so bad `CHECKS.yaml` / `CHECKS.toml` entries are reported as config-file findings before execution.
- Skip deleted files unless your check explicitly needs them.
- Parse config once per configured instance.
- Keep findings stable and actionable.
- Default check findings to the implementation's intrinsic severity; use `[checks.policy].severity` for per-instance overrides.
- Use framework policy for bypass behavior (`[checks.policy].allow_bypass`) instead of check-local bypass parsing.

## Minimal skeleton

```rust
#[derive(Debug, Default)]
pub struct ExampleCheck;

struct ConfiguredExampleCheck {
    threshold: usize,
}

#[async_trait]
impl Check for ExampleCheck {
    fn id(&self) -> &str { "example" }
    fn description(&self) -> &str { "validates example policy" }
    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        let threshold = config
            .get("threshold")
            .and_then(toml::Value::as_integer)
            .unwrap_or(1);
        Ok(Arc::new(ConfiguredExampleCheck {
            threshold: usize::try_from(threshold)?,
        }))
    }
}

#[async_trait]
impl ConfiguredCheck for ConfiguredExampleCheck {
    async fn run(
        &self,
        changeset: &ChangeSet,
        tree: &dyn SourceTree,
    ) -> Result<CheckResult> {
        let mut findings = Vec::new();

        for changed in &changeset.changed_files {
            if matches!(changed.kind, ChangeKind::Deleted) {
                continue;
            }

            // read file and evaluate policy
            let _contents = tree.read_file(&changed.path)?;

            // push findings as needed
        }

        Ok(CheckResult {
            check_id: "example".to_owned(),
            findings,
        })
    }
}
```
