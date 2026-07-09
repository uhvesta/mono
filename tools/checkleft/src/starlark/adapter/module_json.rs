use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use globset::{Glob, GlobSetBuilder};
use serde_json::{Map, Value as JsonValue};
use starlark::values::structs::AllocStruct;
use starlark::values::{Heap, Value};

use crate::input::{ChangeKind, ChangedFile, SourceTree, TreeVersion};
use crate::starlark::adapter::{AdapterFileSelector, AdapterInput, AdapterPreparedOutput, FormatAdapter};

#[derive(Debug)]
pub(crate) struct ModuleJsonAdapterOutput {
    files: Vec<ModuleJsonFilePair>,
    deltas: Vec<ModuleJsonDelta>,
}

impl ModuleJsonAdapterOutput {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn alloc_context<'v>(&self, heap: Heap<'v>) -> Value<'v> {
        let files = self
            .files
            .iter()
            .map(|file| alloc_file_pair(heap, file))
            .collect::<Vec<_>>();
        let deltas = self
            .deltas
            .iter()
            .map(|delta| alloc_delta(heap, delta))
            .collect::<Vec<_>>();
        heap.alloc(AllocStruct([
            ("files", heap.alloc(files)),
            ("deltas", heap.alloc(deltas)),
        ]))
    }
}

pub(crate) struct ModuleJsonAdapter;

impl FormatAdapter for ModuleJsonAdapter {
    fn kind(&self) -> &'static str {
        "module_json"
    }

    fn file_selectors(&self) -> &'static [AdapterFileSelector] {
        &[AdapterFileSelector::Name("module-info.json")]
    }

    fn prepare(&self, input: AdapterInput<'_>) -> Result<AdapterPreparedOutput> {
        Ok(AdapterPreparedOutput::ModuleJson(ModuleJsonAdapterOutput::prepare(
            input.changeset,
            input.tree,
            input.applies_to,
            input.package_scope,
        )?))
    }
}

#[derive(Debug)]
struct ModuleJsonFilePair {
    path: PathBuf,
    before: Option<ModuleJsonFile>,
    after: Option<ModuleJsonFile>,
    change_kind: ChangeKind,
}

#[derive(Debug, Clone)]
struct ModuleJsonFile {
    name: String,
    version: String,
    description: Option<String>,
    dependencies: BTreeMap<String, String>,
    dev_dependencies: BTreeMap<String, String>,
    metadata: BTreeMap<String, String>,
}

#[derive(Debug)]
struct ModuleJsonDelta {
    kind: String,
    path: PathBuf,
    key: String,
    before_value: Option<String>,
    after_value: Option<String>,
}

impl ModuleJsonAdapterOutput {
    fn prepare(
        changeset: &crate::input::ChangeSet,
        tree: &dyn SourceTree,
        applies_to: &[String],
        package_scope: Option<&Path>,
    ) -> Result<Self> {
        let glob_set = build_glob_set(applies_to)?;
        let mut files = Vec::new();
        let mut deltas = Vec::new();
        for changed in &changeset.changed_files {
            if !is_module_json_path(&changed.path) && !changed.old_path.as_deref().is_some_and(is_module_json_path) {
                continue;
            }
            if !matches_changed_file(&glob_set, changed, package_scope) {
                continue;
            }

            let before = read_module_json(tree, before_path(changed), TreeVersion::Base).transpose()?;
            let after = read_module_json(tree, &changed.path, TreeVersion::Current).transpose()?;
            deltas.extend(module_json_deltas(&changed.path, before.as_ref(), after.as_ref()));
            files.push(ModuleJsonFilePair {
                path: changed.path.clone(),
                before,
                after,
                change_kind: changed.kind,
            });
        }
        Ok(Self { files, deltas })
    }
}

fn read_module_json(tree: &dyn SourceTree, path: &Path, version: TreeVersion) -> Option<Result<ModuleJsonFile>> {
    let bytes = match tree.read_file_versioned(path, version) {
        Ok(bytes) => bytes,
        Err(_) => return None,
    };
    Some(parse_module_json(path, &bytes))
}

fn parse_module_json(path: &Path, bytes: &[u8]) -> Result<ModuleJsonFile> {
    let value: JsonValue =
        serde_json::from_slice(bytes).with_context(|| format!("{} is not valid JSON", path.display()))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("{} must contain a JSON object", path.display()))?;
    let name = string_field(object, "name")?.unwrap_or_default();
    let version = string_field(object, "version")?.unwrap_or_default();
    let description = string_field(object, "description")?;
    let dependencies = string_map_field(object, "dependencies")?;
    let dev_dependencies = string_map_field(object, "devDependencies")?;
    let metadata = object
        .iter()
        .filter(|(key, _)| !known_module_keys().contains(key.as_str()))
        .map(|(key, value)| serde_json::to_string(value).map(|text| (key.clone(), text)))
        .collect::<std::result::Result<BTreeMap<_, _>, _>>()
        .context("failed to serialize module-info.json metadata")?;
    Ok(ModuleJsonFile {
        name,
        version,
        description,
        dependencies,
        dev_dependencies,
        metadata,
    })
}

fn string_field(object: &Map<String, JsonValue>, key: &str) -> Result<Option<String>> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(|value| Some(value.to_owned()))
        .ok_or_else(|| anyhow!("module-info.json `{key}` must be a string"))
}

fn string_map_field(object: &Map<String, JsonValue>, key: &str) -> Result<BTreeMap<String, String>> {
    let Some(value) = object.get(key) else {
        return Ok(BTreeMap::new());
    };
    let Some(map) = value.as_object() else {
        bail!("module-info.json `{key}` must be an object");
    };
    map.iter()
        .map(|(name, value)| {
            value
                .as_str()
                .map(|version| (name.clone(), version.to_owned()))
                .ok_or_else(|| anyhow!("module-info.json `{key}.{name}` must be a string"))
        })
        .collect()
}

fn module_json_deltas(
    path: &Path,
    before: Option<&ModuleJsonFile>,
    after: Option<&ModuleJsonFile>,
) -> Vec<ModuleJsonDelta> {
    let mut deltas = Vec::new();
    if let (Some(before), Some(after)) = (before, after) {
        compare_string_field(
            path,
            "name",
            "name_changed",
            Some(&before.name),
            Some(&after.name),
            &mut deltas,
        );
        compare_string_field(
            path,
            "version",
            "version_changed",
            Some(&before.version),
            Some(&after.version),
            &mut deltas,
        );
        compare_string_field(
            path,
            "description",
            "description_removed",
            before.description.as_deref(),
            after.description.as_deref(),
            &mut deltas,
        );
        compare_dependency_map(
            path,
            "dependency",
            &before.dependencies,
            &after.dependencies,
            &mut deltas,
        );
        compare_dependency_map(
            path,
            "dev_dependency",
            &before.dev_dependencies,
            &after.dev_dependencies,
            &mut deltas,
        );
        compare_metadata(path, &before.metadata, &after.metadata, &mut deltas);
    }
    if let Some(before) = before {
        if after.is_none() {
            for key in required_keys_present(before) {
                deltas.push(ModuleJsonDelta {
                    kind: "required_key_removed".to_owned(),
                    path: path.to_path_buf(),
                    key,
                    before_value: None,
                    after_value: None,
                });
            }
        }
    }
    if let (Some(before), Some(after)) = (before, after) {
        if !before.name.is_empty() && after.name.is_empty() {
            required_key_removed(path, "name", Some(before.name.clone()), &mut deltas);
        }
        if !before.version.is_empty() && after.version.is_empty() {
            required_key_removed(path, "version", Some(before.version.clone()), &mut deltas);
        }
    }
    deltas
}

fn compare_string_field(
    path: &Path,
    key: &str,
    kind: &str,
    before: Option<&str>,
    after: Option<&str>,
    deltas: &mut Vec<ModuleJsonDelta>,
) {
    if before == after {
        return;
    }
    if kind == "description_removed" && after.is_some() {
        return;
    }
    deltas.push(ModuleJsonDelta {
        kind: kind.to_owned(),
        path: path.to_path_buf(),
        key: key.to_owned(),
        before_value: before.map(ToOwned::to_owned),
        after_value: after.map(ToOwned::to_owned),
    });
}

fn compare_dependency_map(
    path: &Path,
    prefix: &str,
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    deltas: &mut Vec<ModuleJsonDelta>,
) {
    let keys = before.keys().chain(after.keys()).cloned().collect::<BTreeSet<_>>();
    for key in keys {
        match (before.get(&key), after.get(&key)) {
            (Some(before), Some(after)) if before != after => deltas.push(ModuleJsonDelta {
                kind: format!("{prefix}_version_changed"),
                path: path.to_path_buf(),
                key,
                before_value: Some(before.clone()),
                after_value: Some(after.clone()),
            }),
            (Some(before), None) => deltas.push(ModuleJsonDelta {
                kind: format!("{prefix}_removed"),
                path: path.to_path_buf(),
                key,
                before_value: Some(before.clone()),
                after_value: None,
            }),
            (None, Some(after)) => deltas.push(ModuleJsonDelta {
                kind: format!("{prefix}_added"),
                path: path.to_path_buf(),
                key,
                before_value: None,
                after_value: Some(after.clone()),
            }),
            _ => {}
        }
    }
}

fn compare_metadata(
    path: &Path,
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
    deltas: &mut Vec<ModuleJsonDelta>,
) {
    let keys = before.keys().chain(after.keys()).cloned().collect::<BTreeSet<_>>();
    for key in keys {
        let before_value = before.get(&key);
        let after_value = after.get(&key);
        if before_value == after_value {
            continue;
        }
        deltas.push(ModuleJsonDelta {
            kind: "metadata_changed".to_owned(),
            path: path.to_path_buf(),
            key,
            before_value: before_value.cloned(),
            after_value: after_value.cloned(),
        });
    }
}

fn required_key_removed(path: &Path, key: &str, before_value: Option<String>, deltas: &mut Vec<ModuleJsonDelta>) {
    deltas.push(ModuleJsonDelta {
        kind: "required_key_removed".to_owned(),
        path: path.to_path_buf(),
        key: key.to_owned(),
        before_value,
        after_value: None,
    });
}

fn required_keys_present(module: &ModuleJsonFile) -> Vec<String> {
    let mut keys = Vec::new();
    if !module.name.is_empty() {
        keys.push("name".to_owned());
    }
    if !module.version.is_empty() {
        keys.push("version".to_owned());
    }
    keys
}

fn known_module_keys() -> BTreeSet<&'static str> {
    BTreeSet::from(["name", "version", "description", "dependencies", "devDependencies"])
}

fn matches_changed_file(glob_set: &globset::GlobSet, changed: &ChangedFile, package_scope: Option<&Path>) -> bool {
    matches_applies_to(glob_set, &changed.path, package_scope)
        || changed
            .old_path
            .as_deref()
            .is_some_and(|path| matches_applies_to(glob_set, path, package_scope))
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

fn is_module_json_path(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("module-info.json")
}

fn alloc_file_pair<'v>(heap: Heap<'v>, pair: &ModuleJsonFilePair) -> Value<'v> {
    let before = pair
        .before
        .as_ref()
        .map_or_else(Value::new_none, |module| alloc_module_json(heap, module));
    let after = pair
        .after
        .as_ref()
        .map_or_else(Value::new_none, |module| alloc_module_json(heap, module));
    heap.alloc(AllocStruct([
        ("path", heap.alloc(pair.path.to_string_lossy().to_string())),
        ("before", before),
        ("after", after),
        ("change_kind", heap.alloc(change_kind_name(pair.change_kind))),
    ]))
}

fn alloc_module_json<'v>(heap: Heap<'v>, module: &ModuleJsonFile) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("name", heap.alloc(module.name.clone())),
        ("version", heap.alloc(module.version.clone())),
        (
            "description",
            module
                .description
                .as_ref()
                .map_or_else(Value::new_none, |value| heap.alloc(value.clone())),
        ),
        ("dependencies", heap.alloc(module.dependencies.clone())),
        ("dev_dependencies", heap.alloc(module.dev_dependencies.clone())),
        ("metadata", heap.alloc(module.metadata.clone())),
    ]))
}

fn alloc_delta<'v>(heap: Heap<'v>, delta: &ModuleJsonDelta) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("kind", heap.alloc(delta.kind.clone())),
        ("path", heap.alloc(delta.path.to_string_lossy().to_string())),
        ("key", heap.alloc(delta.key.clone())),
        (
            "before_value",
            delta
                .before_value
                .as_ref()
                .map_or_else(Value::new_none, |value| heap.alloc(value.clone())),
        ),
        (
            "after_value",
            delta
                .after_value
                .as_ref()
                .map_or_else(Value::new_none, |value| heap.alloc(value.clone())),
        ),
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
