use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use globset::{Glob, GlobSetBuilder};
use starlark::values::structs::AllocStruct;
use starlark::values::{Heap, Value};

use crate::input::{ChangeKind, ChangedFile, SourceTree, TreeVersion};
use crate::starlark::adapter::{AdapterFileSelector, AdapterInput, AdapterPreparedOutput, FormatAdapter};

#[derive(Debug)]
pub(crate) struct TextAdapterOutput {
    files: Vec<TextFilePair>,
}

impl TextAdapterOutput {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn alloc_context<'v>(&self, heap: Heap<'v>) -> Value<'v> {
        let file_values = self
            .files
            .iter()
            .map(|file| alloc_text_file_pair(heap, file))
            .collect::<Vec<_>>();
        heap.alloc(AllocStruct([("files", heap.alloc(file_values))]))
    }
}

pub(crate) struct TextAdapter;

impl FormatAdapter for TextAdapter {
    fn kind(&self) -> &'static str {
        "text"
    }

    fn file_selectors(&self) -> &'static [AdapterFileSelector] {
        &[
            AdapterFileSelector::Ext("txt"),
            AdapterFileSelector::Ext("text"),
            AdapterFileSelector::Ext("md"),
            AdapterFileSelector::Name("CHECKS.yaml"),
            AdapterFileSelector::Name("CHECKS.toml"),
        ]
    }

    fn prepare(&self, input: AdapterInput<'_>) -> Result<AdapterPreparedOutput> {
        Ok(AdapterPreparedOutput::Text(TextAdapterOutput {
            files: collect_text_file_pairs(input.changeset, input.tree, input.applies_to, input.package_scope)?,
        }))
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
    changeset: &crate::input::ChangeSet,
    tree: &dyn SourceTree,
    applies_to: &[String],
    package_scope: Option<&Path>,
) -> Result<Vec<TextFilePair>> {
    let glob_set = build_glob_set(applies_to)?;
    let mut files = Vec::new();
    for changed in &changeset.changed_files {
        if !matches_applies_to(&glob_set, &changed.path, package_scope) {
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

fn matches_applies_to(glob_set: &globset::GlobSet, path: &Path, package_scope: Option<&Path>) -> bool {
    if glob_set.is_match(path) {
        return true;
    }
    let Some(scope) = package_scope else {
        return false;
    };
    if scope.as_os_str().is_empty() {
        return false;
    }
    path.strip_prefix(scope)
        .map(|relative| glob_set.is_match(relative))
        .unwrap_or(false)
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
