use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use globset::{Glob, GlobSetBuilder};
use regex::Regex;
use starlark::environment::{GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::{AstModule, Dialect, DialectTypes};
use starlark::values::list::ListRef;
use starlark::values::structs::{AllocStruct, StructRef};
use starlark::values::{Heap, UnpackValue, Value};

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree, TreeVersion};
use crate::output::{CheckResult, Finding, Location, Severity};
use crate::starlark::loader::{CheckleftFileLoader, LoadContext};

#[derive(Debug, Clone)]
pub struct StarlarkCheckSource {
    pub id: String,
    pub path: PathBuf,
    pub source: String,
    pub(crate) load_context: Option<LoadContext>,
}

impl StarlarkCheckSource {
    pub fn inline(id: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            path: PathBuf::from("<inline>"),
            source: source.into(),
            load_context: None,
        }
    }

    pub fn with_load_context(mut self, checkleft_root: impl Into<PathBuf>, check_dir: impl Into<PathBuf>) -> Self {
        self.load_context = Some(LoadContext {
            checkleft_root: checkleft_root.into(),
            check_dir: check_dir.into(),
        });
        self
    }
}

#[derive(Debug, Clone)]
pub struct StarlarkCheckRunner {
    source: StarlarkCheckSource,
}

impl StarlarkCheckRunner {
    pub fn new(source: StarlarkCheckSource) -> Self {
        Self { source }
    }

    pub fn evaluate_text(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        Module::with_temp_heap(|module| {
            let parsed = ParsedCheck::parse(&self.source)?;
            let files = collect_text_file_pairs(changeset, tree, &parsed.meta.applies_to)?;
            if files.is_empty() {
                return Ok(CheckResult {
                    check_id: self.source.id.clone(),
                    findings: Vec::new(),
                });
            }

            let globals = starlark_globals();
            let findings = if let Some(context) = &self.source.load_context {
                let loader = CheckleftFileLoader {
                    tree,
                    globals: &globals,
                    context: context.clone(),
                };
                let mut eval = Evaluator::new(&module);
                eval.set_loader(&loader);
                eval.eval_module(parsed.ast, &globals)
                    .map_err(|e| anyhow!(e.to_string()))
                    .with_context(|| format!("failed to evaluate {}", self.source.path.display()))?;

                let check = module
                    .get("check")
                    .ok_or_else(|| anyhow!("{} does not define check(ctx)", self.source.path.display()))?;
                let ctx = alloc_text_context(eval.heap(), &files);
                let result = eval
                    .eval_function(check, &[ctx], &[])
                    .map_err(|e| anyhow!(e.to_string()))
                    .with_context(|| format!("failed to run check(ctx) in {}", self.source.path.display()))?;
                unpack_findings(result)?
            } else {
                let mut eval = Evaluator::new(&module);
                eval.eval_module(parsed.ast, &globals)
                    .map_err(|e| anyhow!(e.to_string()))
                    .with_context(|| format!("failed to evaluate {}", self.source.path.display()))?;

                let check = module
                    .get("check")
                    .ok_or_else(|| anyhow!("{} does not define check(ctx)", self.source.path.display()))?;
                let ctx = alloc_text_context(eval.heap(), &files);
                let result = eval
                    .eval_function(check, &[ctx], &[])
                    .map_err(|e| anyhow!(e.to_string()))
                    .with_context(|| format!("failed to run check(ctx) in {}", self.source.path.display()))?;
                unpack_findings(result)?
            };
            Ok(CheckResult {
                check_id: self.source.id.clone(),
                findings,
            })
        })
    }
}

#[async_trait::async_trait]
impl Check for StarlarkCheckRunner {
    fn id(&self) -> &str {
        &self.source.id
    }

    fn description(&self) -> &str {
        "Starlark check"
    }

    fn configure(&self, _config: &toml::Value) -> Result<std::sync::Arc<dyn ConfiguredCheck>> {
        Ok(std::sync::Arc::new(self.clone()))
    }
}

#[async_trait::async_trait]
impl ConfiguredCheck for StarlarkCheckRunner {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        self.evaluate_text(changeset, tree)
    }
}

struct ParsedCheck {
    ast: AstModule,
    meta: CheckMeta,
}

impl ParsedCheck {
    fn parse(source: &StarlarkCheckSource) -> Result<Self> {
        let dialect = Dialect {
            enable_types: DialectTypes::Enable,
            enable_load: source.load_context.is_some(),
            enable_keyword_only_arguments: true,
            enable_f_strings: true,
            ..Dialect::Standard
        };
        let ast = AstModule::parse(source.path.to_string_lossy().as_ref(), source.source.clone(), &dialect)
            .map_err(|e| anyhow!(e))
            .with_context(|| format!("failed to parse {}", source.path.display()))?;
        let meta = CheckMeta::parse_from_source(&source.source)
            .with_context(|| format!("failed to parse check_meta() in {}", source.path.display()))?;
        Ok(Self { ast, meta })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckMeta {
    applies_to: Vec<String>,
}

impl CheckMeta {
    fn parse_from_source(source: &str) -> Result<Self> {
        let capture = source
            .split("check_meta(")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .ok_or_else(|| anyhow!("check_meta(...) is required"))?;

        let applies_to_raw = capture
            .split("applies_to")
            .nth(1)
            .and_then(|rest| rest.split('[').nth(1))
            .and_then(|rest| rest.split(']').next())
            .ok_or_else(|| anyhow!("check_meta() must set applies_to = [...]"))?;

        let applies_to = applies_to_raw
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(|item| item.trim_matches('"').trim_matches('\'').to_owned())
            .collect::<Vec<_>>();
        if applies_to.is_empty() {
            bail!("check_meta.applies_to must contain at least one glob");
        }
        Ok(Self { applies_to })
    }
}

#[derive(Debug)]
struct TextFilePair {
    path: PathBuf,
    before: Option<TextFile>,
    after: Option<TextFile>,
    added_lines: Vec<Line>,
    removed_lines: Vec<Line>,
    change_kind: ChangeKind,
}

#[derive(Debug)]
struct TextFile {
    lines: Vec<Line>,
}

#[derive(Debug, Clone)]
struct Line {
    number: u32,
    text: String,
}

fn collect_text_file_pairs(
    changeset: &ChangeSet,
    tree: &dyn SourceTree,
    applies_to: &[String],
) -> Result<Vec<TextFilePair>> {
    let glob_set = build_glob_set(applies_to)?;
    let mut files = Vec::new();
    for changed in &changeset.changed_files {
        if !glob_set.is_match(&changed.path) {
            continue;
        }
        let before = read_text_file(tree, before_path(changed), TreeVersion::Base).transpose()?;
        let after = read_text_file(tree, &changed.path, TreeVersion::Current).transpose()?;
        let (added_lines, removed_lines) = line_delta(before.as_ref(), after.as_ref());
        files.push(TextFilePair {
            path: changed.path.clone(),
            before,
            after,
            added_lines,
            removed_lines,
            change_kind: changed.kind,
        });
    }
    Ok(files)
}

fn build_glob_set(patterns: &[String]) -> Result<globset::GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).with_context(|| format!("invalid applies_to glob `{pattern}`"))?);
    }
    builder.build().context("failed to build applies_to glob set")
}

fn before_path(changed: &ChangedFile) -> &Path {
    changed.old_path.as_deref().unwrap_or(&changed.path)
}

fn read_text_file(tree: &dyn SourceTree, path: &Path, version: TreeVersion) -> Option<Result<TextFile>> {
    let bytes = match tree.read_file_versioned(path, version) {
        Ok(bytes) => bytes,
        Err(_) => return None,
    };
    Some(
        String::from_utf8(bytes)
            .with_context(|| format!("{} is not valid UTF-8", path.display()))
            .map(|contents| TextFile {
                lines: contents
                    .lines()
                    .enumerate()
                    .map(|(idx, text)| Line {
                        number: (idx + 1) as u32,
                        text: text.to_owned(),
                    })
                    .collect(),
            }),
    )
}

fn line_delta(before: Option<&TextFile>, after: Option<&TextFile>) -> (Vec<Line>, Vec<Line>) {
    let before_lines = before.map(|f| f.lines.as_slice()).unwrap_or(&[]);
    let after_lines = after.map(|f| f.lines.as_slice()).unwrap_or(&[]);
    let added = after_lines
        .iter()
        .filter(|line| !before_lines.iter().any(|old| old.text == line.text))
        .cloned()
        .collect();
    let removed = before_lines
        .iter()
        .filter(|line| !after_lines.iter().any(|new| new.text == line.text))
        .cloned()
        .collect();
    (added, removed)
}

fn starlark_globals() -> starlark::environment::Globals {
    GlobalsBuilder::standard()
        .with(checkleft_globals)
        .with(|builder| {
            builder.set(
                "Severity",
                AllocStruct([("fail", "fail"), ("fail_but_overridable", "fail_but_overridable")]),
            );
        })
        .build()
}

#[starlark_module]
fn checkleft_globals(builder: &mut GlobalsBuilder) {
    fn check_meta<'v>(applies_to: &ListRef<'v>, tier: Option<String>) -> anyhow::Result<Value<'v>> {
        let tier = tier.unwrap_or_else(|| "hermetic".to_owned());
        if tier != "hermetic" {
            bail!("unsupported Starlark sandbox tier in this implementation slice: {tier}");
        }
        if applies_to.content().is_empty() {
            bail!("check_meta.applies_to must contain at least one glob");
        }
        Ok(Value::new_none())
    }

    fn finding<'v>(
        severity: String,
        message: String,
        path: Option<String>,
        line: Option<i32>,
        column: Option<i32>,
        remediation: Option<String>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        alloc_finding(heap, severity, message, path, line, column, remediation)
    }

    fn fail<'v>(
        message: String,
        path: Option<String>,
        line: Option<i32>,
        column: Option<i32>,
        remediation: Option<String>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        alloc_finding(heap, "fail".to_owned(), message, path, line, column, remediation)
    }

    fn fail_but_overridable<'v>(
        message: String,
        path: Option<String>,
        line: Option<i32>,
        column: Option<i32>,
        remediation: Option<String>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        alloc_finding(
            heap,
            "fail_but_overridable".to_owned(),
            message,
            path,
            line,
            column,
            remediation,
        )
    }

    fn regex_match(pattern: String, s: String) -> anyhow::Result<bool> {
        Ok(Regex::new(&pattern)
            .with_context(|| format!("invalid regex pattern `{pattern}`"))?
            .is_match(&s))
    }

    fn regex_find_all(pattern: String, s: String) -> anyhow::Result<Vec<String>> {
        Ok(Regex::new(&pattern)
            .with_context(|| format!("invalid regex pattern `{pattern}`"))?
            .find_iter(&s)
            .map(|matched| matched.as_str().to_owned())
            .collect())
    }

    fn glob_match(pattern: String, path: String) -> anyhow::Result<bool> {
        Ok(Glob::new(&pattern)
            .with_context(|| format!("invalid glob pattern `{pattern}`"))?
            .compile_matcher()
            .is_match(Path::new(&path)))
    }
}

fn alloc_finding<'v>(
    heap: Heap<'v>,
    severity: String,
    message: String,
    path: Option<String>,
    line: Option<i32>,
    column: Option<i32>,
    remediation: Option<String>,
) -> anyhow::Result<Value<'v>> {
    Ok(heap.alloc(AllocStruct([
        ("severity", heap.alloc(severity)),
        ("message", heap.alloc(message)),
        ("path", path.map_or_else(Value::new_none, |p| heap.alloc(p))),
        ("line", line.map_or_else(Value::new_none, |l| heap.alloc(l))),
        ("column", column.map_or_else(Value::new_none, |c| heap.alloc(c))),
        (
            "remediation",
            remediation.map_or_else(Value::new_none, |r| heap.alloc(r)),
        ),
    ])))
}

fn alloc_text_context<'v>(heap: Heap<'v>, files: &[TextFilePair]) -> Value<'v> {
    let file_values = files
        .iter()
        .map(|file| alloc_text_file_pair(heap, file))
        .collect::<Vec<_>>();
    heap.alloc(AllocStruct([("files", heap.alloc(file_values))]))
}

fn alloc_text_file_pair<'v>(heap: Heap<'v>, file: &TextFilePair) -> Value<'v> {
    let before = file
        .before
        .as_ref()
        .map_or_else(Value::new_none, |f| alloc_text_file(heap, f));
    let after = file
        .after
        .as_ref()
        .map_or_else(Value::new_none, |f| alloc_text_file(heap, f));
    let added_lines = file
        .added_lines
        .iter()
        .map(|line| alloc_line(heap, line))
        .collect::<Vec<_>>();
    let removed_lines = file
        .removed_lines
        .iter()
        .map(|line| alloc_line(heap, line))
        .collect::<Vec<_>>();
    heap.alloc(AllocStruct([
        ("path", heap.alloc(file.path.to_string_lossy().to_string())),
        ("before", before),
        ("after", after),
        ("added_lines", heap.alloc(added_lines)),
        ("removed_lines", heap.alloc(removed_lines)),
        ("change_kind", heap.alloc(change_kind_name(file.change_kind))),
    ]))
}

fn alloc_text_file<'v>(heap: Heap<'v>, file: &TextFile) -> Value<'v> {
    let lines = file.lines.iter().map(|line| alloc_line(heap, line)).collect::<Vec<_>>();
    heap.alloc(AllocStruct([
        ("lines", heap.alloc(lines)),
        ("line_count", heap.alloc(file.lines.len() as i32)),
    ]))
}

fn alloc_line<'v>(heap: Heap<'v>, line: &Line) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("number", heap.alloc(line.number as i32)),
        ("text", heap.alloc(line.text.clone())),
    ]))
}

fn change_kind_name(kind: ChangeKind) -> String {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Modified => "modified",
        ChangeKind::Deleted => "deleted",
        ChangeKind::Renamed => "renamed",
    }
    .to_owned()
}

fn unpack_findings(value: Value<'_>) -> Result<Vec<Finding>> {
    let list = ListRef::from_value(value).ok_or_else(|| anyhow!("check(ctx) must return list[Finding]"))?;
    list.iter().map(unpack_finding).collect()
}

fn unpack_finding(value: Value<'_>) -> Result<Finding> {
    let finding = StructRef::from_value(value).ok_or_else(|| anyhow!("finding value must be a struct"))?;
    let severity = required_string_field(finding, "severity")?;
    let message = required_string_field(finding, "message")?;
    let path = optional_string_field(finding, "path")?;
    let line = optional_i32_field(finding, "line")?;
    let column = optional_i32_field(finding, "column")?;
    let remediation = optional_string_field(finding, "remediation")?;
    Ok(Finding {
        severity: match severity.as_str() {
            "fail" => Severity::Error,
            "fail_but_overridable" => Severity::Warning,
            other => bail!("unknown Starlark severity `{other}`"),
        },
        message,
        location: path.map(|path| Location {
            path: PathBuf::from(path),
            line: line.map(|line| line as u32),
            column: column.map(|column| column as u32),
        }),
        remediations: remediation.into_iter().collect(),
        suggested_fix: None,
    })
}

fn required_string_field(value: StructRef<'_>, name: &str) -> Result<String> {
    optional_string_field(value, name)?.ok_or_else(|| anyhow!("finding.{name} is required"))
}

fn optional_string_field(value: StructRef<'_>, name: &str) -> Result<Option<String>> {
    optional_field(value, name, |field| {
        String::unpack_value(field).map_err(|err| anyhow!("{err:?}"))
    })
}

fn optional_i32_field(value: StructRef<'_>, name: &str) -> Result<Option<i32>> {
    optional_field(value, name, |field| {
        i32::unpack_value(field).map_err(|err| anyhow!("{err:?}"))
    })
}

fn optional_field<T>(
    value: StructRef<'_>,
    name: &str,
    unpack: impl FnOnce(Value<'_>) -> Result<Option<T>>,
) -> Result<Option<T>> {
    let Some((_, field)) = value.iter().find(|(key, _)| key.as_str() == name) else {
        return Ok(None);
    };
    if field.is_none() {
        return Ok(None);
    }
    unpack(field)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct MapTree {
        current: HashMap<PathBuf, Vec<u8>>,
        base: HashMap<PathBuf, Vec<u8>>,
    }

    impl SourceTree for MapTree {
        fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
            self.current
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow!("missing current file {}", path.display()))
        }

        fn read_file_versioned(&self, path: &Path, version: TreeVersion) -> Result<Vec<u8>> {
            match version {
                TreeVersion::Current => self.read_file(path),
                TreeVersion::Base => self
                    .base
                    .get(path)
                    .cloned()
                    .ok_or_else(|| anyhow!("missing base file {}", path.display())),
            }
        }

        fn exists(&self, path: &Path) -> bool {
            self.current.contains_key(path)
        }

        fn list_dir(&self, _path: &Path) -> Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }

        fn glob(&self, _pattern: &str) -> Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn evaluates_text_check_and_maps_findings() {
        let source = StarlarkCheckSource::inline(
            "text/no-debug",
            r#"
check_meta(applies_to = ["**/*.txt"])

def check(ctx):
    findings = []
    for file in ctx.files:
        for line in file.added_lines:
            if regex_match("debug", line.text) and glob_match("**/*.txt", file.path):
                findings.append(finding(
                    severity = Severity.fail,
                    message = "debug text added",
                    path = file.path,
                    line = line.number,
                    column = 1,
                    remediation = "Remove debug text",
                ))
    return findings
"#,
        );
        let mut tree = MapTree::default();
        tree.base
            .insert(PathBuf::from("notes/example.txt"), b"hello\n".to_vec());
        tree.current
            .insert(PathBuf::from("notes/example.txt"), b"hello\ndebug mode\n".to_vec());

        let changeset = ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes/example.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]);

        let result = StarlarkCheckRunner::new(source)
            .evaluate_text(&changeset, &tree)
            .expect("evaluate check");

        assert_eq!(result.check_id, "text/no-debug");
        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].severity, Severity::Error);
        assert_eq!(result.findings[0].message, "debug text added");
        assert_eq!(
            result.findings[0].location,
            Some(Location {
                path: PathBuf::from("notes/example.txt"),
                line: Some(2),
                column: Some(1),
            })
        );
        assert_eq!(result.findings[0].remediations, vec!["Remove debug text"]);
    }

    #[test]
    fn skips_when_applies_to_does_not_match_changeset() {
        let source = StarlarkCheckSource::inline(
            "text/no-debug",
            r#"
check_meta(applies_to = ["**/*.md"])

def check(ctx):
    fail("should not run")
"#,
        );
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes/example.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]);

        let result = StarlarkCheckRunner::new(source)
            .evaluate_text(&changeset, &MapTree::default())
            .expect("evaluate check");

        assert!(result.findings.is_empty());
    }

    #[test]
    fn evaluates_check_with_local_and_lib_loads() {
        let source = StarlarkCheckSource::inline(
            "text/no-debug",
            r#"
load("//lib/messages", "message_for")
load(":predicates", "has_debug")

check_meta(applies_to = ["**/*.txt"])

def check(ctx):
    findings = []
    for file in ctx.files:
        for line in file.added_lines:
            if has_debug(line.text):
                findings.append(fail(
                    message = message_for("debug"),
                    path = file.path,
                    line = line.number,
                    column = 1,
                ))
    return findings
"#,
        )
        .with_load_context("checkleft", "checkleft/text/public/no_debug");
        let mut tree = MapTree::default();
        tree.current.insert(
            PathBuf::from("checkleft/lib/messages.checkleft"),
            br#"
def message_for(kind):
    return kind + " text added"
"#
            .to_vec(),
        );
        tree.current.insert(
            PathBuf::from("checkleft/text/public/no_debug/predicates.checkleft"),
            br#"
def has_debug(s):
    return "debug" in s
"#
            .to_vec(),
        );
        tree.base
            .insert(PathBuf::from("notes/example.txt"), b"hello\n".to_vec());
        tree.current
            .insert(PathBuf::from("notes/example.txt"), b"hello\ndebug mode\n".to_vec());

        let changeset = ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes/example.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]);

        let result = StarlarkCheckRunner::new(source)
            .evaluate_text(&changeset, &tree)
            .expect("evaluate check");

        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].message, "debug text added");
        assert_eq!(
            result.findings[0].location,
            Some(Location {
                path: PathBuf::from("notes/example.txt"),
                line: Some(2),
                column: Some(1),
            })
        );
    }

    #[test]
    fn rejects_external_package_loads() {
        let source = StarlarkCheckSource::inline(
            "text/no-debug",
            r#"
load("@dep//lib/messages", "message_for")

check_meta(applies_to = ["**/*.txt"])

def check(ctx):
    return []
"#,
        )
        .with_load_context("checkleft", "checkleft/text/public/no_debug");
        let mut tree = MapTree::default();
        tree.current
            .insert(PathBuf::from("notes/example.txt"), b"hello\n".to_vec());
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes/example.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]);

        let err = StarlarkCheckRunner::new(source)
            .evaluate_text(&changeset, &tree)
            .expect_err("external load should fail");

        assert!(err.to_string().contains("failed to evaluate"));
    }

    #[test]
    fn rejects_load_path_traversal() {
        let source = StarlarkCheckSource::inline(
            "text/no-debug",
            r#"
load(":../secrets", "value")

check_meta(applies_to = ["**/*.txt"])

def check(ctx):
    return []
"#,
        )
        .with_load_context("checkleft", "checkleft/text/public/no_debug");
        let mut tree = MapTree::default();
        tree.current
            .insert(PathBuf::from("notes/example.txt"), b"hello\n".to_vec());
        let changeset = ChangeSet::new(vec![ChangedFile {
            path: PathBuf::from("notes/example.txt"),
            kind: ChangeKind::Modified,
            old_path: None,
        }]);

        let err = StarlarkCheckRunner::new(source)
            .evaluate_text(&changeset, &tree)
            .expect_err("traversal load should fail");

        assert!(err.to_string().contains("failed to evaluate"));
    }
}
