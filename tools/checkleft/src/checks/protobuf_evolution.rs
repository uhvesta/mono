use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use allocative::Allocative;
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use protobuf::descriptor::{
    DescriptorProto, EnumDescriptorProto, EnumValueDescriptorProto, FieldDescriptorProto,
    FileDescriptorSet, MethodDescriptorProto, ServiceDescriptorProto,
};
use protobuf::descriptor::field_descriptor_proto::{Label, Type};
use protobuf::rt::WireType;
use protobuf::{CodedInputStream, Message, UnknownValue, UnknownValueRef};
use protobuf_parse::Parser;
use serde::{Deserialize, Serialize};
use starlark::environment::{Globals, GlobalsBuilder, LibraryExtension, Module};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::{AstModule, Dialect, DialectTypes};
use starlark::starlark_simple_value;
use starlark::values::{
    list::ListRef, starlark_value_as_type::StarlarkValueAsType, NoSerialize, ProvidesStaticType,
    StarlarkAttrs, StarlarkValue, starlark_attrs, starlark_value,
};
use tempfile::TempDir;
use walkdir::WalkDir;

use crate::check::{Check, ConfiguredCheck};
use crate::input::{ChangeKind, ChangeSet, ChangedFile, SourceTree, TreeVersion};
use crate::output::{CheckResult, Finding, Location, Severity};
use crate::path::validate_relative_path;

const DEFAULT_PROTOBUF_EVOLUTION_GLOB: &str = "**/*.proto";
const DEFAULT_PROTOBUF_EVOLUTION_POLICY: &str =
    include_str!("protobuf_evolution_default_policy.star");
const DEFAULT_PROTOBUF_EVOLUTION_REMEDIATION: &str =
    "Preserve wire compatibility, or move this policy into a repo-specific protobuf evolution Starlark rule if the change is intentional.";

#[derive(Debug, Clone, Default, Allocative)]
struct OptionalAttr<T>(Option<T>);

#[derive(Debug, Clone, Copy, Allocative)]
struct FrozenAttr<T> {
    value: starlark::values::FrozenValue,
    _marker: std::marker::PhantomData<T>,
}

impl<T> FrozenAttr<T> {
    const fn new(value: starlark::values::FrozenValue) -> Self {
        Self {
            value,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T> From<Option<T>> for OptionalAttr<T> {
    fn from(value: Option<T>) -> Self {
        Self(value)
    }
}

impl<T: starlark::values::type_repr::StarlarkTypeRepr> starlark::values::type_repr::StarlarkTypeRepr
    for OptionalAttr<T>
{
    type Canonical = <Option<T> as starlark::values::type_repr::StarlarkTypeRepr>::Canonical;

    fn starlark_type_repr() -> starlark::typing::Ty {
        <Option<T> as starlark::values::type_repr::StarlarkTypeRepr>::starlark_type_repr()
    }
}

impl<'v, T: starlark::values::AllocValue<'v>> starlark::values::AllocValue<'v> for OptionalAttr<T> {
    fn alloc_value(self, heap: &'v starlark::values::Heap) -> starlark::values::Value<'v> {
        match self.0 {
            Some(value) => value.alloc_value(heap),
            None => starlark::values::Value::new_none(),
        }
    }
}

impl<T: starlark::values::type_repr::StarlarkTypeRepr> starlark::values::type_repr::StarlarkTypeRepr
    for FrozenAttr<T>
{
    type Canonical = <T as starlark::values::type_repr::StarlarkTypeRepr>::Canonical;

    fn starlark_type_repr() -> starlark::typing::Ty {
        T::starlark_type_repr()
    }
}

impl<'v, T: starlark::values::type_repr::StarlarkTypeRepr> starlark::values::AllocValue<'v>
    for FrozenAttr<T>
{
    fn alloc_value(self, _heap: &'v starlark::values::Heap) -> starlark::values::Value<'v> {
        self.value.to_value()
    }
}

#[derive(Debug, Default)]
pub struct ProtobufEvolutionCheck;

#[async_trait]
impl Check for ProtobufEvolutionCheck {
    fn id(&self) -> &str {
        "protobuf-evolution"
    }

    fn description(&self) -> &str {
        "checks protobuf schema evolution across base and current revisions"
    }

    fn configure(&self, config: &toml::Value) -> Result<Arc<dyn ConfiguredCheck>> {
        Ok(Arc::new(parse_config(config)?))
    }
}

#[async_trait]
impl ConfiguredCheck for CompiledProtobufEvolutionConfig {
    async fn run(&self, changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<CheckResult> {
        let changed_proto_files = select_changed_proto_files(changeset, &self.include_globs);
        if changed_proto_files.is_empty() {
            return Ok(CheckResult {
                check_id: "protobuf-evolution".to_owned(),
                findings: Vec::new(),
            });
        }

        let snapshots = build_proto_snapshots(changeset, tree)?;
        let current_descriptors = parse_descriptor_set(
            snapshots.current.path(),
            &snapshots.current_inputs,
            self.parser_backend,
        )
        .context("parse current protobuf descriptors")?;
        let base_descriptors = parse_descriptor_set(
            snapshots.base.path(),
            &snapshots.base_inputs,
            self.parser_backend,
        )
        .context("parse base protobuf descriptors")?;
        let current_registry = build_extension_registry_set(
            snapshots.current.path(),
            &self.extension_registries,
            self.parser_backend,
        )
        .context("build current protobuf extension registries")?;
        let base_registry = build_extension_registry_set(
            snapshots.base.path(),
            &self.extension_registries,
            self.parser_backend,
        )
        .context("build base protobuf extension registries")?;

        let current_schemas = extract_file_schemas(
            &current_descriptors.descriptor_set,
            &snapshots.current_targets,
            &current_registry,
        );
        let base_schemas =
            extract_file_schemas(&base_descriptors.descriptor_set, &snapshots.base_targets, &base_registry);

        let context = build_context(
            &changed_proto_files,
            &base_schemas,
            &current_schemas,
            &current_registry.infos,
            base_descriptors.backend,
            current_descriptors.backend,
            self.severity,
        );
        let mut findings = run_starlark_source(
            DEFAULT_PROTOBUF_EVOLUTION_POLICY,
            "<builtin protobuf evolution policy>",
            &context,
        )?;

        if let Some(starlark_path) = self.starlark_path.as_ref() {
            findings.extend(run_starlark_policy(tree, starlark_path, &context)?);
        }

        Ok(CheckResult {
            check_id: "protobuf-evolution".to_owned(),
            findings,
        })
    }
}

#[derive(Debug, Deserialize)]
struct ProtobufEvolutionConfig {
    #[serde(default)]
    include_globs: Vec<String>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    starlark_path: Option<String>,
    #[serde(default)]
    parser_backend: Option<String>,
    #[serde(default)]
    extension_registries: Vec<ExtensionRegistryConfig>,
}

struct CompiledProtobufEvolutionConfig {
    include_globs: GlobSet,
    severity: Severity,
    starlark_path: Option<PathBuf>,
    parser_backend: ParserBackend,
    extension_registries: Vec<CompiledExtensionRegistryConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct ExtensionRegistryConfig {
    name: String,
    include_globs: Vec<String>,
}

#[derive(Debug, Clone)]
struct CompiledExtensionRegistryConfig {
    name: String,
    include_globs: GlobSet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
enum ParserBackend {
    Auto,
    Protoc,
    Pure,
}

fn parse_config(config: &toml::Value) -> Result<CompiledProtobufEvolutionConfig> {
    let parsed: ProtobufEvolutionConfig = config
        .clone()
        .try_into()
        .context("invalid protobuf-evolution config")?;

    let include_globs = if parsed.include_globs.is_empty() {
        compile_globset("include_globs", &[DEFAULT_PROTOBUF_EVOLUTION_GLOB.to_owned()])?
    } else {
        compile_globset("include_globs", &parsed.include_globs)?
    };

    let starlark_path = match parsed.starlark_path {
        Some(path) => {
            let path = PathBuf::from(path);
            validate_relative_path(&path)?;
            Some(path)
        }
        None => None,
    };

    let extension_registries = parsed
        .extension_registries
        .into_iter()
        .map(compile_extension_registry_config)
        .collect::<Result<Vec<_>>>()?;

    Ok(CompiledProtobufEvolutionConfig {
        include_globs,
        severity: Severity::parse_with_default(parsed.severity.as_deref(), Severity::Error),
        starlark_path,
        parser_backend: parse_parser_backend(parsed.parser_backend.as_deref())?,
        extension_registries,
    })
}

fn compile_extension_registry_config(
    config: ExtensionRegistryConfig,
) -> Result<CompiledExtensionRegistryConfig> {
    let name = config.name.trim();
    if name.is_empty() {
        bail!("protobuf extension registry `name` must not be empty");
    }
    if config.include_globs.is_empty() {
        bail!("protobuf extension registry `{name}` must declare at least one `include_globs` entry");
    }
    Ok(CompiledExtensionRegistryConfig {
        name: name.to_owned(),
        include_globs: compile_globset(
            "extension_registries.include_globs",
            &config.include_globs,
        )?,
    })
}

fn parse_parser_backend(raw: Option<&str>) -> Result<ParserBackend> {
    match raw.unwrap_or("auto").trim().to_ascii_lowercase().as_str() {
        "" | "auto" => Ok(ParserBackend::Auto),
        "protoc" => Ok(ParserBackend::Protoc),
        "pure" => Ok(ParserBackend::Pure),
        other => bail!(
            "invalid `parser_backend` `{other}`; expected one of `auto`, `protoc`, or `pure`"
        ),
    }
}

fn compile_globset(field_name: &str, patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern)
            .with_context(|| format!("invalid `{field_name}` glob pattern: {pattern}"))?;
        builder.add(glob);
    }
    builder
        .build()
        .with_context(|| format!("failed to compile `{field_name}` glob patterns"))
}

fn select_changed_proto_files(changeset: &ChangeSet, include_globs: &GlobSet) -> Vec<ChangedProtoFile> {
    let mut seen = BTreeSet::new();
    let mut selected = Vec::new();

    for changed_file in &changeset.changed_files {
        let current_matches = include_globs.is_match(&changed_file.path);
        let previous_path = previous_path_for_changed_file(changed_file);
        let previous_matches = previous_path
            .as_ref()
            .is_some_and(|path| include_globs.is_match(path));
        if !current_matches && !previous_matches {
            continue;
        }

        let key = format!(
            "{}::{:?}",
            changed_file.path.display(),
            previous_path.as_ref().map(|path| path.display().to_string())
        );
        if !seen.insert(key) {
            continue;
        }

        selected.push(ChangedProtoFile {
            current_path: if matches!(changed_file.kind, ChangeKind::Deleted) {
                None
            } else {
                Some(changed_file.path.clone())
            },
            base_path: previous_path,
            kind: changed_file.kind,
        });
    }

    selected
}

#[derive(Debug, Clone)]
struct ChangedProtoFile {
    current_path: Option<PathBuf>,
    base_path: Option<PathBuf>,
    kind: ChangeKind,
}

struct ProtoSnapshots {
    current: TempDir,
    base: TempDir,
    current_inputs: Vec<PathBuf>,
    base_inputs: Vec<PathBuf>,
    current_targets: BTreeSet<String>,
    base_targets: BTreeSet<String>,
}

fn build_proto_snapshots(changeset: &ChangeSet, tree: &dyn SourceTree) -> Result<ProtoSnapshots> {
    let current = TempDir::new().context("create current protobuf snapshot")?;
    let base = TempDir::new().context("create base protobuf snapshot")?;

    for path in tree.glob(DEFAULT_PROTOBUF_EVOLUTION_GLOB)? {
        let bytes = tree
            .read_file(&path)
            .with_context(|| format!("read current proto `{}`", path.display()))?;
        write_snapshot_file(current.path(), &path, &bytes)?;
        write_snapshot_file(base.path(), &path, &bytes)?;
    }

    let mut current_inputs = Vec::new();
    let mut base_inputs = Vec::new();
    let mut current_targets = BTreeSet::new();
    let mut base_targets = BTreeSet::new();

    for changed_file in &changeset.changed_files {
        let current_is_proto = changed_file.path.extension().and_then(|ext| ext.to_str()) == Some("proto");
        let base_path = previous_path_for_changed_file(changed_file);
        let base_is_proto = base_path
            .as_ref()
            .and_then(|path| path.extension().and_then(|ext| ext.to_str()))
            == Some("proto");
        if !current_is_proto && !base_is_proto {
            continue;
        }

        if let Some(current_path) = (!matches!(changed_file.kind, ChangeKind::Deleted))
            .then_some(changed_file.path.clone())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("proto"))
        {
            current_targets.insert(current_path.to_string_lossy().to_string());
            current_inputs.push(current_path);
        }

        if let Some(base_path) = base_path
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("proto"))
        {
            base_targets.insert(base_path.to_string_lossy().to_string());
            base_inputs.push(base_path.clone());

            match changed_file.kind {
                ChangeKind::Added => {
                    remove_snapshot_file(base.path(), &changed_file.path)?;
                }
                ChangeKind::Modified => {
                    let bytes = tree.read_file_versioned(&base_path, TreeVersion::Base).with_context(|| {
                        format!("read base proto `{}`", base_path.display())
                    })?;
                    write_snapshot_file(base.path(), &base_path, &bytes)?;
                }
                ChangeKind::Deleted => {
                    let bytes = tree.read_file_versioned(&base_path, TreeVersion::Base).with_context(|| {
                        format!("read deleted base proto `{}`", base_path.display())
                    })?;
                    write_snapshot_file(base.path(), &base_path, &bytes)?;
                }
                ChangeKind::Renamed => {
                    remove_snapshot_file(base.path(), &changed_file.path)?;
                    let bytes = tree.read_file_versioned(&base_path, TreeVersion::Base).with_context(|| {
                        format!("read renamed base proto `{}`", base_path.display())
                    })?;
                    write_snapshot_file(base.path(), &base_path, &bytes)?;
                }
            }
        }
    }

    current_inputs.sort();
    current_inputs.dedup();
    base_inputs.sort();
    base_inputs.dedup();

    Ok(ProtoSnapshots {
        current,
        base,
        current_inputs,
        base_inputs,
        current_targets,
        base_targets,
    })
}

fn write_snapshot_file(root: &Path, relative_path: &Path, bytes: &[u8]) -> Result<()> {
    let full_path = root.join(relative_path);
    if let Some(parent) = full_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create snapshot directory `{}`", parent.display()))?;
    }
    fs::write(&full_path, bytes)
        .with_context(|| format!("write snapshot proto `{}`", full_path.display()))
}

fn remove_snapshot_file(root: &Path, relative_path: &Path) -> Result<()> {
    let full_path = root.join(relative_path);
    match fs::remove_file(&full_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("remove snapshot proto `{}`", full_path.display())),
    }
}

struct ParsedDescriptorSet {
    descriptor_set: FileDescriptorSet,
    backend: ParserBackend,
}

fn build_extension_registry_set(
    snapshot_root: &Path,
    registries: &[CompiledExtensionRegistryConfig],
    backend: ParserBackend,
) -> Result<ExtensionRegistrySet> {
    let mut infos = Vec::new();
    let mut by_extendee = BTreeMap::<String, BTreeMap<u32, RegisteredExtension>>::new();
    let mut by_full_name = BTreeMap::<String, RegisteredExtension>::new();
    let mut message_types = BTreeMap::<String, RegisteredMessageType>::new();
    let mut enum_types = BTreeMap::<String, RegisteredEnumType>::new();

    for registry in registries {
        let inputs = collect_snapshot_proto_inputs(snapshot_root, &registry.include_globs)?;
        let target_paths = inputs
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect::<BTreeSet<_>>();
        let parsed = parse_descriptor_set(snapshot_root, &inputs, backend).with_context(|| {
            format!("parse protobuf extension registry `{}`", registry.name)
        })?;
        let entries =
            extract_registered_extensions(&parsed.descriptor_set, &target_paths, &registry.name);
        let (registry_message_types, registry_enum_types) =
            extract_registered_types(&parsed.descriptor_set, &target_paths);
        let mut extendees = entries
            .iter()
            .map(|entry| entry.extendee.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        extendees.sort();
        infos.push(ExtensionRegistryInfo {
            name: registry.name.clone(),
            extension_count: i32::try_from(entries.len()).unwrap_or(i32::MAX),
            files: inputs
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect(),
            extendees,
        });
        for entry in entries {
            if let Some(existing) = by_extendee
                .get(&entry.extendee)
                .and_then(|entries| entries.get(&entry.field_number))
            {
                bail!(
                    "protobuf extension registry collision: `{}` and `{}` both declare extendee `{}` field number `{}`",
                    existing.source_path,
                    entry.source_path,
                    entry.extendee,
                    entry.field_number
                );
            }
            if let Some(existing) = by_full_name.get(&entry.full_name) {
                bail!(
                    "protobuf extension registry collision: `{}` and `{}` both declare extension `{}`",
                    existing.source_path,
                    entry.source_path,
                    entry.full_name
                );
            }
            by_full_name.insert(entry.full_name.clone(), entry.clone());
            by_extendee
                .entry(entry.extendee.clone())
                .or_default()
                .insert(entry.field_number, entry);
        }
        for (name, message_type) in registry_message_types {
            message_types.entry(name).or_insert(message_type);
        }
        for (name, enum_type) in registry_enum_types {
            enum_types.entry(name).or_insert(enum_type);
        }
    }

    Ok(ExtensionRegistrySet {
        infos,
        by_extendee,
        message_types,
        enum_types,
    })
}

fn collect_snapshot_proto_inputs(snapshot_root: &Path, include_globs: &GlobSet) -> Result<Vec<PathBuf>> {
    let mut inputs = WalkDir::new(snapshot_root)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            let relative = entry.path().strip_prefix(snapshot_root).ok()?.to_path_buf();
            include_globs.is_match(&relative).then_some(relative)
        })
        .collect::<Vec<_>>();
    inputs.sort();
    inputs.dedup();
    Ok(inputs)
}

fn parse_descriptor_set(
    snapshot_root: &Path,
    inputs: &[PathBuf],
    backend: ParserBackend,
) -> Result<ParsedDescriptorSet> {
    if inputs.is_empty() {
        return Ok(ParsedDescriptorSet {
            descriptor_set: FileDescriptorSet::new(),
            backend,
        });
    }

    match backend {
        ParserBackend::Pure => run_parser(snapshot_root, inputs, ParserBackend::Pure),
        ParserBackend::Protoc => run_parser(snapshot_root, inputs, ParserBackend::Protoc),
        ParserBackend::Auto => match run_parser(snapshot_root, inputs, ParserBackend::Protoc) {
            Ok(parsed) => Ok(parsed),
            Err(protoc_error) => run_parser(snapshot_root, inputs, ParserBackend::Pure).map_err(
                |pure_error| {
                    anyhow!(
                        "failed to parse protobuf descriptors with `protoc` and pure backends; protoc error: {protoc_error}; pure error: {pure_error}"
                    )
                },
            ),
        },
    }
}

fn run_parser(
    snapshot_root: &Path,
    inputs: &[PathBuf],
    backend: ParserBackend,
) -> Result<ParsedDescriptorSet> {
    let mut parser = Parser::new();
    match backend {
        ParserBackend::Auto => unreachable!("auto backend is resolved before parser invocation"),
        ParserBackend::Protoc => {
            parser.protoc();
            parser.capture_stderr();
        }
        ParserBackend::Pure => {
            parser.pure();
        }
    }
    parser.include(snapshot_root);
    parser.inputs(inputs.iter().map(|path| snapshot_root.join(path)));
    Ok(ParsedDescriptorSet {
        descriptor_set: parser.file_descriptor_set().map_err(|error| anyhow!(error))?,
        backend,
    })
}

fn previous_path_for_changed_file(changed_file: &ChangedFile) -> Option<PathBuf> {
    if matches!(changed_file.kind, ChangeKind::Added) {
        None
    } else {
        Some(
            changed_file
                .old_path
                .clone()
                .unwrap_or_else(|| changed_file.path.clone()),
        )
    }
}

#[derive(Debug, Clone, Serialize)]
struct StarlarkProtoContext {
    config: StarlarkPolicyConfig,
    parser: StarlarkParserContext,
    registries: Vec<ExtensionRegistryInfo>,
    files: Vec<DescriptorPair>,
    deltas: Vec<SchemaDelta>,
}

#[derive(Debug, Clone, Serialize)]
struct StarlarkPolicyConfig {
    default_severity: String,
    default_remediation: String,
}

#[derive(Debug, Clone, Serialize)]
struct StarlarkParserContext {
    before_backend: String,
    after_backend: String,
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct SeverityEnumValue {
    value: &'static str,
}
starlark_simple_value!(SeverityEnumValue);

#[starlark_value(type = "Severity")]
impl<'v> StarlarkValue<'v> for SeverityEnumValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct ParserBackendEnumValue {
    value: &'static str,
}
starlark_simple_value!(ParserBackendEnumValue);

#[starlark_value(type = "ParserBackend")]
impl<'v> StarlarkValue<'v> for ParserBackendEnumValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct FieldLabelEnumValue {
    value: &'static str,
}
starlark_simple_value!(FieldLabelEnumValue);

#[starlark_value(type = "FieldLabel")]
impl<'v> StarlarkValue<'v> for FieldLabelEnumValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct FieldKindEnumValue {
    value: &'static str,
}
starlark_simple_value!(FieldKindEnumValue);

#[starlark_value(type = "FieldKind")]
impl<'v> StarlarkValue<'v> for FieldKindEnumValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct DeltaKindEnumValue {
    value: &'static str,
}
starlark_simple_value!(DeltaKindEnumValue);

#[starlark_value(type = "DeltaKind")]
impl<'v> StarlarkValue<'v> for DeltaKindEnumValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct ProtoContextValue {
    #[starlark(clone)]
    config: PolicyConfigValue,
    #[starlark(clone)]
    parser: ParserInfoValue,
    #[starlark(clone)]
    registries: Vec<ExtensionRegistryInfoValue>,
    #[starlark(clone)]
    files: Vec<DescriptorPairValue>,
    #[starlark(clone)]
    deltas: Vec<SchemaDeltaValue>,
}
starlark_simple_value!(ProtoContextValue);

#[starlark_value(type = "ProtoContext")]
impl<'v> StarlarkValue<'v> for ProtoContextValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct PolicyConfigValue {
    default_severity: FrozenAttr<SeverityEnumValue>,
    default_remediation: String,
}
starlark_simple_value!(PolicyConfigValue);

#[starlark_value(type = "PolicyConfig")]
impl<'v> StarlarkValue<'v> for PolicyConfigValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct ParserInfoValue {
    before_backend: FrozenAttr<ParserBackendEnumValue>,
    after_backend: FrozenAttr<ParserBackendEnumValue>,
}
starlark_simple_value!(ParserInfoValue);

#[starlark_value(type = "ParserInfo")]
impl<'v> StarlarkValue<'v> for ParserInfoValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct ExtensionRegistryInfoValue {
    name: String,
    extension_count: i32,
    #[starlark(clone)]
    files: Vec<String>,
    #[starlark(clone)]
    extendees: Vec<String>,
}
starlark_simple_value!(ExtensionRegistryInfoValue);

#[starlark_value(type = "ExtensionRegistryInfo")]
impl<'v> StarlarkValue<'v> for ExtensionRegistryInfoValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct DescriptorPairValue {
    path: String,
    #[starlark(clone)]
    before: OptionalAttr<FileDescriptorValue>,
    #[starlark(clone)]
    after: OptionalAttr<FileDescriptorValue>,
}
starlark_simple_value!(DescriptorPairValue);

#[starlark_value(type = "DescriptorPair")]
impl<'v> StarlarkValue<'v> for DescriptorPairValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct FileDescriptorValue {
    path: String,
    package: String,
    syntax: String,
    #[starlark(clone)]
    options: DescriptorOptionsValue,
    #[starlark(clone)]
    messages: Vec<MessageDescriptorValue>,
    #[starlark(clone)]
    enums: Vec<EnumDescriptorValue>,
    #[starlark(clone)]
    services: Vec<ServiceDescriptorValue>,
    #[starlark(clone)]
    extensions: Vec<FieldDescriptorValue>,
}
starlark_simple_value!(FileDescriptorValue);

#[starlark_value(type = "FileDescriptor")]
impl<'v> StarlarkValue<'v> for FileDescriptorValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct MessageDescriptorValue {
    full_name: String,
    name: String,
    #[starlark(clone)]
    options: DescriptorOptionsValue,
    is_map_entry: bool,
    #[starlark(clone)]
    fields: Vec<FieldDescriptorValue>,
    #[starlark(clone)]
    oneofs: Vec<OneofDescriptorValue>,
    #[starlark(clone)]
    extensions: Vec<FieldDescriptorValue>,
    #[starlark(clone)]
    reserved_ranges: Vec<ReservedRangeValue>,
    #[starlark(clone)]
    reserved_names: Vec<String>,
    #[starlark(clone)]
    nested_messages: Vec<MessageDescriptorValue>,
    #[starlark(clone)]
    nested_enums: Vec<EnumDescriptorValue>,
}
starlark_simple_value!(MessageDescriptorValue);

#[starlark_value(type = "MessageDescriptor")]
impl<'v> StarlarkValue<'v> for MessageDescriptorValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct FieldDescriptorValue {
    full_name: String,
    name: String,
    number: i32,
    label: FrozenAttr<FieldLabelEnumValue>,
    kind: FrozenAttr<FieldKindEnumValue>,
    type_name: OptionalAttr<String>,
    json_name: OptionalAttr<String>,
    oneof_index: OptionalAttr<i32>,
    oneof_name: OptionalAttr<String>,
    proto3_optional: bool,
    extendee: OptionalAttr<String>,
    #[starlark(clone)]
    options: DescriptorOptionsValue,
}
starlark_simple_value!(FieldDescriptorValue);

#[starlark_value(type = "FieldDescriptor")]
impl<'v> StarlarkValue<'v> for FieldDescriptorValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct EnumDescriptorValue {
    full_name: String,
    name: String,
    #[starlark(clone)]
    options: DescriptorOptionsValue,
    #[starlark(clone)]
    reserved_ranges: Vec<ReservedRangeValue>,
    #[starlark(clone)]
    reserved_names: Vec<String>,
    #[starlark(clone)]
    values: Vec<EnumValueDescriptorValue>,
}
starlark_simple_value!(EnumDescriptorValue);

#[starlark_value(type = "EnumDescriptor")]
impl<'v> StarlarkValue<'v> for EnumDescriptorValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct EnumValueDescriptorValue {
    full_name: String,
    name: String,
    number: i32,
    #[starlark(clone)]
    options: DescriptorOptionsValue,
}
starlark_simple_value!(EnumValueDescriptorValue);

#[starlark_value(type = "EnumValueDescriptor")]
impl<'v> StarlarkValue<'v> for EnumValueDescriptorValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct OneofDescriptorValue {
    full_name: String,
    name: String,
    #[starlark(clone)]
    options: DescriptorOptionsValue,
}
starlark_simple_value!(OneofDescriptorValue);

#[starlark_value(type = "OneofDescriptor")]
impl<'v> StarlarkValue<'v> for OneofDescriptorValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct ServiceDescriptorValue {
    full_name: String,
    name: String,
    #[starlark(clone)]
    options: DescriptorOptionsValue,
    #[starlark(clone)]
    methods: Vec<MethodDescriptorValue>,
}
starlark_simple_value!(ServiceDescriptorValue);

#[starlark_value(type = "ServiceDescriptor")]
impl<'v> StarlarkValue<'v> for ServiceDescriptorValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct MethodDescriptorValue {
    full_name: String,
    name: String,
    input_type: String,
    output_type: String,
    client_streaming: bool,
    server_streaming: bool,
    #[starlark(clone)]
    options: DescriptorOptionsValue,
}
starlark_simple_value!(MethodDescriptorValue);

#[starlark_value(type = "MethodDescriptor")]
impl<'v> StarlarkValue<'v> for MethodDescriptorValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct ReservedRangeValue {
    start: i32,
    end: i32,
}
starlark_simple_value!(ReservedRangeValue);

#[starlark_value(type = "ReservedRange")]
impl<'v> StarlarkValue<'v> for ReservedRangeValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct DescriptorOptionsValue {
    fingerprint: String,
    has_unknown_fields: bool,
    #[starlark(clone)]
    uninterpreted: Vec<UninterpretedOptionValue>,
    #[starlark(clone)]
    extensions: Vec<OptionExtensionValue>,
}
starlark_simple_value!(DescriptorOptionsValue);

#[starlark_value(type = "DescriptorOptions")]
impl<'v> StarlarkValue<'v> for DescriptorOptionsValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct OptionExtensionValue {
    registry_name: String,
    full_name: String,
    extendee: String,
    field_number: i32,
    kind: FrozenAttr<FieldKindEnumValue>,
    type_name: OptionalAttr<String>,
    is_repeated: bool,
    #[starlark(clone)]
    values: Vec<OptionValueValue>,
    decoded: bool,
}
starlark_simple_value!(OptionExtensionValue);

#[starlark_value(type = "OptionExtension")]
impl<'v> StarlarkValue<'v> for OptionExtensionValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct OptionFieldValue {
    name: String,
    full_name: String,
    number: i32,
    kind: FrozenAttr<FieldKindEnumValue>,
    type_name: OptionalAttr<String>,
    is_repeated: bool,
    #[starlark(clone)]
    values: Vec<OptionValueValue>,
    decoded: bool,
}
starlark_simple_value!(OptionFieldValue);

#[starlark_value(type = "OptionField")]
impl<'v> StarlarkValue<'v> for OptionFieldValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct OptionValueKindEnumValue {
    value: &'static str,
}
starlark_simple_value!(OptionValueKindEnumValue);

#[starlark_value(type = "OptionValueKind")]
impl<'v> StarlarkValue<'v> for OptionValueKindEnumValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct OptionValueValue {
    kind: FrozenAttr<OptionValueKindEnumValue>,
    bool_value: OptionalAttr<bool>,
    int_value: OptionalAttr<i64>,
    float_value: OptionalAttr<f64>,
    enum_name: OptionalAttr<String>,
    string_value: OptionalAttr<String>,
    bytes_hex: OptionalAttr<String>,
    message_hex: OptionalAttr<String>,
    #[starlark(clone)]
    message_fields: Vec<OptionFieldValue>,
    raw_repr: String,
    decoded: bool,
}
starlark_simple_value!(OptionValueValue);

#[starlark_value(type = "OptionValue")]
impl<'v> StarlarkValue<'v> for OptionValueValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct UninterpretedOptionValue {
    name: String,
    value: String,
}
starlark_simple_value!(UninterpretedOptionValue);

#[starlark_value(type = "UninterpretedOption")]
impl<'v> StarlarkValue<'v> for UninterpretedOptionValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct SchemaDeltaValue {
    kind: FrozenAttr<DeltaKindEnumValue>,
    path: String,
    symbol: String,
    before_kind: OptionalAttr<FrozenAttr<FieldKindEnumValue>>,
    after_kind: OptionalAttr<FrozenAttr<FieldKindEnumValue>>,
    before_type_name: OptionalAttr<String>,
    after_type_name: OptionalAttr<String>,
    before_label: OptionalAttr<FrozenAttr<FieldLabelEnumValue>>,
    after_label: OptionalAttr<FrozenAttr<FieldLabelEnumValue>>,
    before_number: OptionalAttr<i32>,
    after_number: OptionalAttr<i32>,
    field_number: OptionalAttr<i32>,
    number: OptionalAttr<i32>,
    before_package: OptionalAttr<String>,
    after_package: OptionalAttr<String>,
    before_syntax: OptionalAttr<String>,
    after_syntax: OptionalAttr<String>,
    before_input_type: OptionalAttr<String>,
    after_input_type: OptionalAttr<String>,
    before_output_type: OptionalAttr<String>,
    after_output_type: OptionalAttr<String>,
    before_oneof: OptionalAttr<String>,
    after_oneof: OptionalAttr<String>,
    before_option_fingerprint: OptionalAttr<String>,
    after_option_fingerprint: OptionalAttr<String>,
    before_client_streaming: OptionalAttr<bool>,
    after_client_streaming: OptionalAttr<bool>,
    before_server_streaming: OptionalAttr<bool>,
    after_server_streaming: OptionalAttr<bool>,
    before_map_entry: OptionalAttr<bool>,
    after_map_entry: OptionalAttr<bool>,
    range_start: OptionalAttr<i32>,
    range_end: OptionalAttr<i32>,
    name: OptionalAttr<String>,
    registry_name: OptionalAttr<String>,
    before_raw_value: OptionalAttr<String>,
    after_raw_value: OptionalAttr<String>,
}
starlark_simple_value!(SchemaDeltaValue);

#[starlark_value(type = "SchemaDelta")]
impl<'v> StarlarkValue<'v> for SchemaDeltaValue {
    starlark_attrs!();
}

#[derive(
    Debug, Clone, StarlarkAttrs, ProvidesStaticType, NoSerialize, Allocative,
)]
struct FindingValue {
    severity: FrozenAttr<SeverityEnumValue>,
    message: String,
    path: OptionalAttr<String>,
    line: OptionalAttr<i32>,
    column: OptionalAttr<i32>,
    remediation: OptionalAttr<String>,
}
starlark_simple_value!(FindingValue);

#[starlark_value(type = "Finding")]
impl<'v> StarlarkValue<'v> for FindingValue {
    starlark_attrs!();
}

macro_rules! impl_debug_display {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl fmt::Display for $ty {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    write!(f, "{:?}", self)
                }
            }
        )+
    };
}

impl_debug_display!(
    SeverityEnumValue,
    ParserBackendEnumValue,
    FieldLabelEnumValue,
    FieldKindEnumValue,
    DeltaKindEnumValue,
    ProtoContextValue,
    PolicyConfigValue,
    ParserInfoValue,
    ExtensionRegistryInfoValue,
    DescriptorPairValue,
    FileDescriptorValue,
    MessageDescriptorValue,
    FieldDescriptorValue,
    EnumDescriptorValue,
    EnumValueDescriptorValue,
    OneofDescriptorValue,
    ServiceDescriptorValue,
    MethodDescriptorValue,
    ReservedRangeValue,
    DescriptorOptionsValue,
    OptionExtensionValue,
    OptionFieldValue,
    OptionValueKindEnumValue,
    OptionValueValue,
    UninterpretedOptionValue,
    SchemaDeltaValue,
    FindingValue,
);

#[derive(Debug, Clone, Serialize)]
struct DescriptorPair {
    path: String,
    before: Option<FileSchema>,
    after: Option<FileSchema>,
}

#[derive(Debug, Clone, Serialize)]
struct FileSchema {
    path: String,
    package: String,
    syntax: String,
    options: DescriptorOptionsSchema,
    messages: Vec<MessageSchema>,
    enums: Vec<EnumSchema>,
    services: Vec<ServiceSchema>,
    extensions: Vec<FieldSchema>,
}

#[derive(Debug, Clone, Serialize)]
struct MessageSchema {
    full_name: String,
    name: String,
    options: DescriptorOptionsSchema,
    is_map_entry: bool,
    fields: Vec<FieldSchema>,
    oneofs: Vec<OneofSchema>,
    extensions: Vec<FieldSchema>,
    reserved_ranges: Vec<ReservedRangeSchema>,
    reserved_names: Vec<String>,
    nested_messages: Vec<MessageSchema>,
    nested_enums: Vec<EnumSchema>,
}

#[derive(Debug, Clone, Serialize)]
struct FieldSchema {
    full_name: String,
    name: String,
    number: i32,
    label: String,
    kind: String,
    type_name: Option<String>,
    json_name: Option<String>,
    oneof_index: Option<i32>,
    oneof_name: Option<String>,
    proto3_optional: bool,
    extendee: Option<String>,
    options: DescriptorOptionsSchema,
}

#[derive(Debug, Clone, Serialize)]
struct EnumSchema {
    full_name: String,
    name: String,
    options: DescriptorOptionsSchema,
    reserved_ranges: Vec<ReservedRangeSchema>,
    reserved_names: Vec<String>,
    values: Vec<EnumValueSchema>,
}

#[derive(Debug, Clone, Serialize)]
struct EnumValueSchema {
    full_name: String,
    name: String,
    number: i32,
    options: DescriptorOptionsSchema,
}

#[derive(Debug, Clone, Serialize)]
struct OneofSchema {
    full_name: String,
    name: String,
    options: DescriptorOptionsSchema,
}

#[derive(Debug, Clone, Serialize)]
struct ServiceSchema {
    full_name: String,
    name: String,
    options: DescriptorOptionsSchema,
    methods: Vec<MethodSchema>,
}

#[derive(Debug, Clone, Serialize)]
struct MethodSchema {
    full_name: String,
    name: String,
    input_type: String,
    output_type: String,
    client_streaming: bool,
    server_streaming: bool,
    options: DescriptorOptionsSchema,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct ReservedRangeSchema {
    start: i32,
    end: i32,
}

#[derive(Debug, Clone, Serialize, Default)]
struct DescriptorOptionsSchema {
    fingerprint: String,
    has_unknown_fields: bool,
    uninterpreted: Vec<UninterpretedOptionSchema>,
    extensions: Vec<OptionExtensionSchema>,
}

#[derive(Debug, Clone, Serialize)]
struct UninterpretedOptionSchema {
    name: String,
    value: String,
}

#[derive(Debug, Clone, Serialize)]
struct OptionExtensionSchema {
    registry_name: String,
    full_name: String,
    extendee: String,
    field_number: i32,
    kind: String,
    type_name: Option<String>,
    is_repeated: bool,
    values: Vec<OptionValueSchema>,
    decoded: bool,
}

#[derive(Debug, Clone, Serialize)]
struct OptionFieldSchema {
    name: String,
    full_name: String,
    number: i32,
    kind: String,
    type_name: Option<String>,
    is_repeated: bool,
    values: Vec<OptionValueSchema>,
    decoded: bool,
}

#[derive(Debug, Clone, Serialize)]
struct OptionValueSchema {
    kind: String,
    bool_value: Option<bool>,
    int_value: Option<i64>,
    float_value: Option<f64>,
    enum_name: Option<String>,
    string_value: Option<String>,
    bytes_hex: Option<String>,
    message_hex: Option<String>,
    message_fields: Vec<OptionFieldSchema>,
    raw_repr: String,
    decoded: bool,
}

#[derive(Debug, Clone, Serialize)]
struct ExtensionRegistryInfo {
    name: String,
    extension_count: i32,
    files: Vec<String>,
    extendees: Vec<String>,
}

#[derive(Debug, Clone)]
struct ExtensionRegistrySet {
    infos: Vec<ExtensionRegistryInfo>,
    by_extendee: BTreeMap<String, BTreeMap<u32, RegisteredExtension>>,
    message_types: BTreeMap<String, RegisteredMessageType>,
    enum_types: BTreeMap<String, RegisteredEnumType>,
}

#[derive(Debug, Clone)]
struct RegisteredExtension {
    registry_name: String,
    source_path: String,
    full_name: String,
    extendee: String,
    field_number: u32,
    kind: String,
    type_name: Option<String>,
    is_repeated: bool,
}

#[derive(Debug, Clone)]
struct RegisteredMessageType {
    fields_by_number: BTreeMap<u32, RegisteredMessageField>,
}

#[derive(Debug, Clone)]
struct RegisteredMessageField {
    name: String,
    full_name: String,
    number: u32,
    kind: String,
    type_name: Option<String>,
    is_repeated: bool,
}

#[derive(Debug, Clone)]
struct RegisteredEnumType {
    values_by_number: BTreeMap<i32, String>,
}

#[derive(Debug, Clone)]
enum OptionWireValue {
    Fixed32(u32),
    Fixed64(u64),
    Varint(u64),
    LengthDelimited(Vec<u8>),
}

impl From<UnknownValueRef<'_>> for OptionWireValue {
    fn from(value: UnknownValueRef<'_>) -> Self {
        match value {
            UnknownValueRef::Fixed32(raw) => Self::Fixed32(raw),
            UnknownValueRef::Fixed64(raw) => Self::Fixed64(raw),
            UnknownValueRef::Varint(raw) => Self::Varint(raw),
            UnknownValueRef::LengthDelimited(bytes) => Self::LengthDelimited(bytes.to_vec()),
        }
    }
}

impl From<UnknownValue> for OptionWireValue {
    fn from(value: UnknownValue) -> Self {
        match value {
            UnknownValue::Fixed32(raw) => Self::Fixed32(raw),
            UnknownValue::Fixed64(raw) => Self::Fixed64(raw),
            UnknownValue::Varint(raw) => Self::Varint(raw),
            UnknownValue::LengthDelimited(bytes) => Self::LengthDelimited(bytes),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct SchemaDelta {
    kind: String,
    path: String,
    symbol: String,
    details: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone)]
struct FlatFileSchema {
    file: FileSchema,
    messages: BTreeMap<String, FlatMessageSchema>,
    enums: BTreeMap<String, FlatEnumSchema>,
    services: BTreeMap<String, ServiceSchema>,
    extensions_by_number: BTreeMap<i32, FieldSchema>,
    extensions_by_name: BTreeMap<String, FieldSchema>,
}

#[derive(Debug, Clone)]
struct FlatMessageSchema {
    full_name: String,
    fields_by_number: BTreeMap<i32, FieldSchema>,
    fields_by_name: BTreeMap<String, FieldSchema>,
    oneofs_by_name: BTreeMap<String, OneofSchema>,
    extensions_by_number: BTreeMap<i32, FieldSchema>,
    extensions_by_name: BTreeMap<String, FieldSchema>,
    reserved_ranges: BTreeSet<ReservedRangeSchema>,
    reserved_names: BTreeSet<String>,
    options: DescriptorOptionsSchema,
    is_map_entry: bool,
}

#[derive(Debug, Clone)]
struct FlatEnumSchema {
    full_name: String,
    values_by_number: BTreeMap<i32, EnumValueSchema>,
    values_by_name: BTreeMap<String, EnumValueSchema>,
    reserved_ranges: BTreeSet<ReservedRangeSchema>,
    reserved_names: BTreeSet<String>,
    options: DescriptorOptionsSchema,
}

fn extract_file_schemas(
    descriptor_set: &FileDescriptorSet,
    target_paths: &BTreeSet<String>,
    registry: &ExtensionRegistrySet,
) -> BTreeMap<String, FlatFileSchema> {
    let mut files = BTreeMap::new();
    for file in &descriptor_set.file {
        let path = file.name().to_owned();
        if !target_paths.contains(&path) {
            continue;
        }

        let (messages, flat_messages, nested_flat_enums) =
            collect_messages(file.package(), "", &file.message_type, registry);
        let (enums, mut flat_enums) = collect_enums(file.package(), "", &file.enum_type, registry);
        let (services, flat_services) = collect_services(file.package(), &file.service, registry);
        let extensions = collect_extensions(file.package(), "", &file.extension, &[], registry);
        let extensions_by_number = extensions
            .iter()
            .cloned()
            .map(|field| (field.number, field))
            .collect::<BTreeMap<_, _>>();
        let extensions_by_name = extensions
            .iter()
            .cloned()
            .map(|field| (field.name.clone(), field))
            .collect::<BTreeMap<_, _>>();
        flat_enums.extend(nested_flat_enums);
        files.insert(
            path.clone(),
            FlatFileSchema {
                file: FileSchema {
                    path,
                    package: file.package().to_owned(),
                    syntax: file.syntax().to_owned(),
                    options: descriptor_options_schema(
                        file.options.as_ref(),
                        ".google.protobuf.FileOptions",
                        registry,
                    ),
                    messages,
                    enums,
                    services,
                    extensions,
                },
                messages: flat_messages,
                enums: flat_enums,
                services: flat_services,
                extensions_by_number,
                extensions_by_name,
            },
        );
    }
    files
}

fn collect_messages(
    package: &str,
    parent: &str,
    messages: &[DescriptorProto],
    registry: &ExtensionRegistrySet,
) -> (
    Vec<MessageSchema>,
    BTreeMap<String, FlatMessageSchema>,
    BTreeMap<String, FlatEnumSchema>,
) {
    let mut output = Vec::new();
    let mut flat = BTreeMap::new();
    let mut flat_enums = BTreeMap::new();

    for message in messages {
        let name = message.name().to_owned();
        let full_name = join_proto_name(package, parent, &name);
        let oneofs = message
            .oneof_decl
            .iter()
            .map(|oneof| OneofSchema {
                full_name: format!("{full_name}.{}", oneof.name()),
                name: oneof.name().to_owned(),
                options: descriptor_options_schema(
                    oneof.options.as_ref(),
                    ".google.protobuf.OneofOptions",
                    registry,
                ),
            })
            .collect::<Vec<_>>();
        let oneof_names = oneofs
            .iter()
            .map(|oneof| oneof.name.clone())
            .collect::<Vec<_>>();

        let fields = message
            .field
            .iter()
            .map(|field| field_schema(field, &full_name, &oneof_names, registry))
            .collect::<Vec<_>>();
        let extensions = collect_extensions(package, &full_name, &message.extension, &[], registry);
        let reserved_ranges = message
            .reserved_range
            .iter()
            .map(|range| ReservedRangeSchema {
                start: range.start(),
                end: range.end(),
            })
            .collect::<Vec<_>>();
        let reserved_names = message.reserved_name.clone();

        let fields_by_number = fields
            .iter()
            .cloned()
            .map(|field| (field.number, field))
            .collect::<BTreeMap<_, _>>();
        let fields_by_name = fields
            .iter()
            .cloned()
            .map(|field| (field.name.clone(), field))
            .collect::<BTreeMap<_, _>>();
        let extensions_by_number = extensions
            .iter()
            .cloned()
            .map(|field| (field.number, field))
            .collect::<BTreeMap<_, _>>();
        let extensions_by_name = extensions
            .iter()
            .cloned()
            .map(|field| (field.name.clone(), field))
            .collect::<BTreeMap<_, _>>();

        let (nested_messages, nested_flat_messages, nested_flat_enums_from_messages) =
            collect_messages(package, &full_name, &message.nested_type, registry);
        let (nested_enums, nested_flat_enums) =
            collect_enums(package, &full_name, &message.enum_type, registry);

        flat.insert(
            full_name.clone(),
            FlatMessageSchema {
                full_name: full_name.clone(),
                fields_by_number,
                fields_by_name,
                oneofs_by_name: oneofs
                    .iter()
                    .cloned()
                    .map(|oneof| (oneof.name.clone(), oneof))
                    .collect(),
                extensions_by_number,
                extensions_by_name,
                reserved_ranges: reserved_ranges.iter().cloned().collect(),
                reserved_names: reserved_names.iter().cloned().collect(),
                options: descriptor_options_schema(
                    message.options.as_ref(),
                    ".google.protobuf.MessageOptions",
                    registry,
                ),
                is_map_entry: message
                    .options
                    .as_ref()
                    .and_then(|options| options.map_entry)
                    .unwrap_or(false),
            },
        );
        flat.extend(nested_flat_messages);
        flat_enums.extend(nested_flat_enums_from_messages);
        flat_enums.extend(nested_flat_enums.clone());

        let schema = MessageSchema {
            full_name,
            name,
            options: descriptor_options_schema(
                message.options.as_ref(),
                ".google.protobuf.MessageOptions",
                registry,
            ),
            is_map_entry: message
                .options
                .as_ref()
                .and_then(|options| options.map_entry)
                .unwrap_or(false),
            fields,
            oneofs,
            extensions,
            reserved_ranges,
            reserved_names,
            nested_messages,
            nested_enums: nested_enums.clone(),
        };
        output.push(schema);
    }

    (output, flat, flat_enums)
}

fn collect_enums(
    package: &str,
    parent: &str,
    enums: &[EnumDescriptorProto],
    registry: &ExtensionRegistrySet,
) -> (Vec<EnumSchema>, BTreeMap<String, FlatEnumSchema>) {
    let mut output = Vec::new();
    let mut flat = BTreeMap::new();

    for enum_proto in enums {
        let name = enum_proto.name().to_owned();
        let full_name = join_proto_name(package, parent, &name);
        let values = enum_proto
            .value
            .iter()
            .map(|value| enum_value_schema(value, &full_name, registry))
            .collect::<Vec<_>>();
        let values_by_number = values
            .iter()
            .cloned()
            .map(|value| (value.number, value))
            .collect::<BTreeMap<_, _>>();
        let values_by_name = values
            .iter()
            .cloned()
            .map(|value| (value.name.clone(), value))
            .collect::<BTreeMap<_, _>>();
        let reserved_ranges = enum_proto
            .reserved_range
            .iter()
            .map(|range| ReservedRangeSchema {
                start: range.start(),
                end: range.end(),
            })
            .collect::<Vec<_>>();
        let reserved_names = enum_proto.reserved_name.clone();

        output.push(EnumSchema {
            full_name: full_name.clone(),
            name: name.clone(),
            options: descriptor_options_schema(
                enum_proto.options.as_ref(),
                ".google.protobuf.EnumOptions",
                registry,
            ),
            reserved_ranges: reserved_ranges.clone(),
            reserved_names: reserved_names.clone(),
            values,
        });
        flat.insert(
            full_name,
            FlatEnumSchema {
                full_name: join_proto_name(package, parent, &name),
                values_by_number,
                values_by_name,
                reserved_ranges: reserved_ranges.into_iter().collect(),
                reserved_names: reserved_names.into_iter().collect(),
                options: descriptor_options_schema(
                    enum_proto.options.as_ref(),
                    ".google.protobuf.EnumOptions",
                    registry,
                ),
            },
        );
    }

    (output, flat)
}

fn collect_services(
    package: &str,
    services: &[ServiceDescriptorProto],
    registry: &ExtensionRegistrySet,
) -> (Vec<ServiceSchema>, BTreeMap<String, ServiceSchema>) {
    let mut output = Vec::new();
    let mut flat = BTreeMap::new();
    for service in services {
        let full_name = join_proto_name(package, "", service.name());
        let methods = service
            .method
            .iter()
            .map(|method| method_schema(method, &full_name, registry))
            .collect::<Vec<_>>();
        let schema = ServiceSchema {
            full_name: full_name.clone(),
            name: service.name().to_owned(),
            options: descriptor_options_schema(
                service.options.as_ref(),
                ".google.protobuf.ServiceOptions",
                registry,
            ),
            methods,
        };
        flat.insert(full_name, schema.clone());
        output.push(schema);
    }
    (output, flat)
}

fn extract_registered_extensions(
    descriptor_set: &FileDescriptorSet,
    target_paths: &BTreeSet<String>,
    registry_name: &str,
) -> Vec<RegisteredExtension> {
    let mut output = Vec::new();
    for file in &descriptor_set.file {
        let path = file.name().to_owned();
        if !target_paths.contains(&path) {
            continue;
        }
        output.extend(extract_registered_extensions_from_fields(
            registry_name,
            &path,
            file.package(),
            "",
            &file.extension,
        ));
        output.extend(extract_registered_extensions_from_messages(
            registry_name,
            &path,
            file.package(),
            "",
            &file.message_type,
        ));
    }
    output.sort_by(|left, right| {
        left.extendee
            .cmp(&right.extendee)
            .then(left.field_number.cmp(&right.field_number))
            .then(left.full_name.cmp(&right.full_name))
    });
    output
}

fn extract_registered_types(
    descriptor_set: &FileDescriptorSet,
    target_paths: &BTreeSet<String>,
) -> (
    BTreeMap<String, RegisteredMessageType>,
    BTreeMap<String, RegisteredEnumType>,
) {
    let mut message_types = BTreeMap::new();
    let mut enum_types = BTreeMap::new();
    for file in &descriptor_set.file {
        let path = file.name().to_owned();
        if !target_paths.contains(&path) {
            continue;
        }
        collect_registered_enums(
            file.package(),
            "",
            &file.enum_type,
            &mut enum_types,
        );
        collect_registered_messages(
            file.package(),
            "",
            &file.message_type,
            &mut message_types,
            &mut enum_types,
        );
    }
    (message_types, enum_types)
}

fn collect_registered_messages(
    package: &str,
    parent: &str,
    messages: &[DescriptorProto],
    message_types: &mut BTreeMap<String, RegisteredMessageType>,
    enum_types: &mut BTreeMap<String, RegisteredEnumType>,
) {
    for message in messages {
        let full_name = normalize_proto_type_name(&join_proto_name(package, parent, message.name()));
        let fields_by_number = message
            .field
            .iter()
            .filter_map(|field| {
                Some((
                    u32::try_from(field.number()).ok()?,
                    RegisteredMessageField {
                        name: field.name().to_owned(),
                        full_name: format!("{full_name}.{}", field.name()),
                        number: u32::try_from(field.number()).ok()?,
                        kind: describe_field_kind(field),
                        type_name: non_empty(field.type_name.as_deref()),
                        is_repeated: field
                            .label
                            .as_ref()
                            .map(|label| label.enum_value_or_default())
                            == Some(Label::LABEL_REPEATED),
                    },
                ))
            })
            .collect();
        message_types.insert(
            full_name.clone(),
            RegisteredMessageType {
                fields_by_number,
            },
        );
        collect_registered_enums(
            package,
            &full_name,
            &message.enum_type,
            enum_types,
        );
        collect_registered_messages(
            package,
            &full_name,
            &message.nested_type,
            message_types,
            enum_types,
        );
    }
}

fn collect_registered_enums(
    package: &str,
    parent: &str,
    enums: &[EnumDescriptorProto],
    enum_types: &mut BTreeMap<String, RegisteredEnumType>,
) {
    for enum_proto in enums {
        let full_name = normalize_proto_type_name(&join_proto_name(package, parent, enum_proto.name()));
        enum_types.insert(
            full_name.clone(),
            RegisteredEnumType {
                values_by_number: enum_proto
                    .value
                    .iter()
                    .map(|value| (value.number(), value.name().to_owned()))
                    .collect(),
            },
        );
    }
}

fn extract_registered_extensions_from_messages(
    registry_name: &str,
    source_path: &str,
    package: &str,
    parent: &str,
    messages: &[DescriptorProto],
) -> Vec<RegisteredExtension> {
    let mut output = Vec::new();
    for message in messages {
        let full_name = join_proto_name(package, parent, message.name());
        output.extend(extract_registered_extensions_from_fields(
            registry_name,
            source_path,
            package,
            &full_name,
            &message.extension,
        ));
        output.extend(extract_registered_extensions_from_messages(
            registry_name,
            source_path,
            package,
            &full_name,
            &message.nested_type,
        ));
    }
    output
}

fn extract_registered_extensions_from_fields(
    registry_name: &str,
    source_path: &str,
    package: &str,
    parent: &str,
    fields: &[FieldDescriptorProto],
) -> Vec<RegisteredExtension> {
    let parent_full_name = if parent.is_empty() {
        package.to_owned()
    } else {
        parent.to_owned()
    };
    fields
        .iter()
        .filter_map(|field| {
            let extendee = non_empty(field.extendee.as_deref())?;
            let name = field.name().to_owned();
            let full_name = if parent_full_name.is_empty() {
                name.clone()
            } else {
                format!("{parent_full_name}.{name}")
            };
            Some(RegisteredExtension {
                registry_name: registry_name.to_owned(),
                source_path: source_path.to_owned(),
                full_name,
                extendee,
                field_number: u32::try_from(field.number()).ok()?,
                kind: describe_field_kind(field),
                type_name: non_empty(field.type_name.as_deref()),
                is_repeated: field
                    .label
                    .as_ref()
                    .map(|label| label.enum_value_or_default())
                    == Some(Label::LABEL_REPEATED),
            })
        })
        .collect()
}

fn collect_extensions(
    package: &str,
    parent: &str,
    extensions: &[FieldDescriptorProto],
    oneof_names: &[String],
    registry: &ExtensionRegistrySet,
) -> Vec<FieldSchema> {
    let parent_full_name = if parent.is_empty() {
        package.to_owned()
    } else {
        parent.to_owned()
    };
    extensions
        .iter()
        .map(|field| field_schema(field, &parent_full_name, oneof_names, registry))
        .collect()
}

fn field_schema(
    field: &FieldDescriptorProto,
    parent_full_name: &str,
    oneof_names: &[String],
    registry: &ExtensionRegistrySet,
) -> FieldSchema {
    let field_name = field.name().to_owned();
    let full_name = if parent_full_name.is_empty() {
        field_name.clone()
    } else if parent_full_name.ends_with('.') {
        format!("{parent_full_name}{field_name}")
    } else {
        format!("{parent_full_name}.{field_name}")
    };
    FieldSchema {
        full_name,
        name: field_name,
        number: field.number.unwrap_or_default(),
        label: describe_field_label(field),
        kind: describe_field_kind(field),
        type_name: non_empty(field.type_name.as_deref()),
        json_name: non_empty(field.json_name.as_deref()),
        oneof_index: field.oneof_index,
        oneof_name: field
            .oneof_index
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| oneof_names.get(index).cloned()),
        proto3_optional: field.proto3_optional.unwrap_or(false),
        extendee: non_empty(field.extendee.as_deref()),
        options: descriptor_options_schema(
            field.options.as_ref(),
            ".google.protobuf.FieldOptions",
            registry,
        ),
    }
}

fn enum_value_schema(
    value: &EnumValueDescriptorProto,
    parent_full_name: &str,
    registry: &ExtensionRegistrySet,
) -> EnumValueSchema {
    EnumValueSchema {
        full_name: format!("{parent_full_name}.{}", value.name()),
        name: value.name().to_owned(),
        number: value.number.unwrap_or_default(),
        options: descriptor_options_schema(
            value.options.as_ref(),
            ".google.protobuf.EnumValueOptions",
            registry,
        ),
    }
}

fn method_schema(
    method: &MethodDescriptorProto,
    service_full_name: &str,
    registry: &ExtensionRegistrySet,
) -> MethodSchema {
    MethodSchema {
        full_name: format!("{service_full_name}.{}", method.name()),
        name: method.name().to_owned(),
        input_type: method.input_type().to_owned(),
        output_type: method.output_type().to_owned(),
        client_streaming: method.client_streaming.unwrap_or(false),
        server_streaming: method.server_streaming.unwrap_or(false),
        options: descriptor_options_schema(
            method.options.as_ref(),
            ".google.protobuf.MethodOptions",
            registry,
        ),
    }
}

fn join_proto_name(package: &str, parent: &str, name: &str) -> String {
    match (package.is_empty(), parent.is_empty()) {
        (false, true) => format!("{package}.{name}"),
        (false, false) => format!("{parent}.{name}"),
        (true, false) => format!("{parent}.{name}"),
        (true, true) => name.to_owned(),
    }
}

fn describe_field_label(field: &FieldDescriptorProto) -> String {
    match field
        .label
        .as_ref()
        .map(|label| label.enum_value_or_default())
        .unwrap_or_default()
    {
        Label::LABEL_OPTIONAL => "optional".to_owned(),
        Label::LABEL_REQUIRED => "required".to_owned(),
        Label::LABEL_REPEATED => "repeated".to_owned(),
    }
}

fn describe_field_kind(field: &FieldDescriptorProto) -> String {
    match field
        .type_
        .as_ref()
        .map(|kind| kind.enum_value_or_default())
        .unwrap_or_default()
    {
        Type::TYPE_DOUBLE => "double".to_owned(),
        Type::TYPE_FLOAT => "float".to_owned(),
        Type::TYPE_INT64 => "int64".to_owned(),
        Type::TYPE_UINT64 => "uint64".to_owned(),
        Type::TYPE_INT32 => "int32".to_owned(),
        Type::TYPE_FIXED64 => "fixed64".to_owned(),
        Type::TYPE_FIXED32 => "fixed32".to_owned(),
        Type::TYPE_BOOL => "bool".to_owned(),
        Type::TYPE_STRING => "string".to_owned(),
        Type::TYPE_GROUP => "group".to_owned(),
        Type::TYPE_MESSAGE => "message".to_owned(),
        Type::TYPE_BYTES => "bytes".to_owned(),
        Type::TYPE_UINT32 => "uint32".to_owned(),
        Type::TYPE_ENUM => "enum".to_owned(),
        Type::TYPE_SFIXED32 => "sfixed32".to_owned(),
        Type::TYPE_SFIXED64 => "sfixed64".to_owned(),
        Type::TYPE_SINT32 => "sint32".to_owned(),
        Type::TYPE_SINT64 => "sint64".to_owned(),
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn normalize_proto_type_name(value: &str) -> String {
    value.trim_start_matches('.').to_owned()
}

fn descriptor_options_schema<M: Message + fmt::Debug>(
    options: Option<&M>,
    extendee: &str,
    registry: &ExtensionRegistrySet,
) -> DescriptorOptionsSchema {
    let Some(options) = options else {
        return DescriptorOptionsSchema::default();
    };

    DescriptorOptionsSchema {
        fingerprint: bytes_to_hex(&options.write_to_bytes().unwrap_or_default()),
        has_unknown_fields: options.special_fields().unknown_fields().iter().next().is_some(),
        uninterpreted: collect_uninterpreted_options(format!("{options:?}")),
        extensions: resolve_option_extensions(extendee, options, registry),
    }
}

fn resolve_option_extensions<M: Message + fmt::Debug>(
    extendee: &str,
    options: &M,
    registry: &ExtensionRegistrySet,
) -> Vec<OptionExtensionSchema> {
    let Some(by_number) = registry.by_extendee.get(extendee) else {
        return Vec::new();
    };

    let mut grouped = BTreeMap::<u32, Vec<OptionWireValue>>::new();
    for (field_number, value) in options.special_fields().unknown_fields().iter() {
        grouped.entry(field_number).or_default().push(value.into());
    }

    let mut resolved = Vec::new();
    for (field_number, values) in grouped {
        let Some(extension) = by_number.get(&field_number) else {
            continue;
        };
        let decoded_values = decode_option_values(
            values,
            &extension.kind,
            extension.type_name.as_deref(),
            extension.is_repeated,
            registry,
        );
        resolved.push(OptionExtensionSchema {
            registry_name: extension.registry_name.clone(),
            full_name: extension.full_name.clone(),
            extendee: extension.extendee.clone(),
            field_number: i32::try_from(extension.field_number).unwrap_or_default(),
            kind: extension.kind.clone(),
            type_name: extension.type_name.clone(),
            is_repeated: extension.is_repeated,
            decoded: decoded_values.iter().all(|value| value.decoded),
            values: decoded_values,
        });
    }
    resolved.sort_by(|left, right| {
        left.registry_name
            .cmp(&right.registry_name)
            .then(left.full_name.cmp(&right.full_name))
            .then(left.field_number.cmp(&right.field_number))
    });
    resolved
}

fn decode_option_values(
    values: Vec<OptionWireValue>,
    kind: &str,
    type_name: Option<&str>,
    is_repeated: bool,
    registry: &ExtensionRegistrySet,
) -> Vec<OptionValueSchema> {
    let mut decoded = Vec::new();
    for value in values {
        if is_repeated && is_packed_kind(kind) {
            if let OptionWireValue::LengthDelimited(bytes) = &value {
                if let Ok(mut unpacked) = decode_packed_option_values(bytes, kind, type_name, registry)
                {
                    decoded.append(&mut unpacked);
                    continue;
                }
            }
        }
        decoded.push(decode_option_value(value, kind, type_name, registry));
    }
    decoded
}

fn decode_option_value(
    value: OptionWireValue,
    kind: &str,
    type_name: Option<&str>,
    registry: &ExtensionRegistrySet,
) -> OptionValueSchema {
    match kind {
        "bool" => match value {
            OptionWireValue::Varint(raw) => OptionValueSchema {
                kind: "bool".to_owned(),
                bool_value: Some(raw != 0),
                int_value: None,
                float_value: None,
                enum_name: None,
                string_value: None,
                bytes_hex: None,
                message_hex: None,
                message_fields: Vec::new(),
                raw_repr: (raw != 0).to_string(),
                decoded: true,
            },
            other => undecoded_option_value("unknown", wire_debug_repr(&other)),
        },
        "string" => match value {
            OptionWireValue::LengthDelimited(bytes) => match String::from_utf8(bytes.clone()) {
                Ok(text) => OptionValueSchema {
                    kind: "string".to_owned(),
                    bool_value: None,
                    int_value: None,
                    float_value: None,
                    enum_name: None,
                    string_value: Some(text.clone()),
                    bytes_hex: None,
                    message_hex: None,
                    message_fields: Vec::new(),
                    raw_repr: text,
                    decoded: true,
                },
                Err(_) => undecoded_option_value("bytes", format!("0x{}", bytes_to_hex(&bytes))),
            },
            other => undecoded_option_value("unknown", wire_debug_repr(&other)),
        },
        "bytes" => match value {
            OptionWireValue::LengthDelimited(bytes) => {
                let hex = bytes_to_hex(&bytes);
                OptionValueSchema {
                    kind: "bytes".to_owned(),
                    bool_value: None,
                    int_value: None,
                    float_value: None,
                    enum_name: None,
                    string_value: None,
                    bytes_hex: Some(hex.clone()),
                    message_hex: None,
                    message_fields: Vec::new(),
                    raw_repr: format!("0x{hex}"),
                    decoded: true,
                }
            }
            other => undecoded_option_value("unknown", wire_debug_repr(&other)),
        },
        "double" => match value {
            OptionWireValue::Fixed64(raw) => {
                let decoded = f64::from_bits(raw);
                OptionValueSchema {
                    kind: "float".to_owned(),
                    bool_value: None,
                    int_value: None,
                    float_value: Some(decoded),
                    enum_name: None,
                    string_value: None,
                    bytes_hex: None,
                    message_hex: None,
                    message_fields: Vec::new(),
                    raw_repr: decoded.to_string(),
                    decoded: true,
                }
            }
            other => undecoded_option_value("unknown", wire_debug_repr(&other)),
        },
        "float" => match value {
            OptionWireValue::Fixed32(raw) => {
                let decoded = f32::from_bits(raw) as f64;
                OptionValueSchema {
                    kind: "float".to_owned(),
                    bool_value: None,
                    int_value: None,
                    float_value: Some(decoded),
                    enum_name: None,
                    string_value: None,
                    bytes_hex: None,
                    message_hex: None,
                    message_fields: Vec::new(),
                    raw_repr: decoded.to_string(),
                    decoded: true,
                }
            }
            other => undecoded_option_value("unknown", wire_debug_repr(&other)),
        },
        "fixed32" => decode_int32_value(value, |raw| i64::from(raw)),
        "sfixed32" => decode_int32_value(value, |raw| i64::from(raw as i32)),
        "fixed64" => decode_int64_value(value, |raw| raw as i64),
        "sfixed64" => decode_int64_value(value, |raw| raw as i64),
        "sint32" => decode_varint_value(value, |raw| i64::from(decode_zig_zag_32(raw as u32))),
        "sint64" => decode_varint_value(value, decode_zig_zag_64),
        "int32" => decode_varint_value(value, |raw| i64::from(raw as u32 as i32)),
        "uint32" => decode_varint_value(value, |raw| i64::from(raw as u32)),
        "int64" => decode_varint_value(value, |raw| raw as i64),
        "uint64" => decode_varint_value(value, |raw| raw as i64),
        "enum" => match value {
            OptionWireValue::Varint(raw) => {
                let enum_number = raw as i32;
                let enum_name = type_name
                    .map(normalize_proto_type_name)
                    .and_then(|name| registry.enum_types.get(&name).cloned())
                    .and_then(|enum_type| enum_type.values_by_number.get(&enum_number).cloned());
                OptionValueSchema {
                    kind: "enum".to_owned(),
                    bool_value: None,
                    int_value: Some(i64::from(enum_number)),
                    float_value: None,
                    enum_name: enum_name.clone(),
                    string_value: enum_name.clone(),
                    bytes_hex: None,
                    message_hex: None,
                    message_fields: Vec::new(),
                    raw_repr: enum_name.unwrap_or_else(|| enum_number.to_string()),
                    decoded: true,
                }
            }
            other => undecoded_option_value("unknown", wire_debug_repr(&other)),
        },
        "message" => match value {
            OptionWireValue::LengthDelimited(bytes) => {
                let hex = bytes_to_hex(&bytes);
                let Some(type_name) = type_name.map(normalize_proto_type_name) else {
                    return OptionValueSchema {
                        kind: "message".to_owned(),
                        bool_value: None,
                        int_value: None,
                        float_value: None,
                        enum_name: None,
                        string_value: None,
                        bytes_hex: None,
                        message_hex: Some(hex.clone()),
                        message_fields: Vec::new(),
                        raw_repr: format!("0x{hex}"),
                        decoded: false,
                    };
                };
                match decode_message_fields(&bytes, &type_name, registry) {
                    Ok(message_fields) => OptionValueSchema {
                        kind: "message".to_owned(),
                        bool_value: None,
                        int_value: None,
                        float_value: None,
                        enum_name: None,
                        string_value: None,
                        bytes_hex: None,
                        message_hex: Some(hex.clone()),
                        decoded: message_fields.iter().all(|field| field.decoded),
                        message_fields,
                        raw_repr: format!("0x{hex}"),
                    },
                    Err(_) => OptionValueSchema {
                        kind: "message".to_owned(),
                        bool_value: None,
                        int_value: None,
                        float_value: None,
                        enum_name: None,
                        string_value: None,
                        bytes_hex: None,
                        message_hex: Some(hex.clone()),
                        message_fields: Vec::new(),
                        raw_repr: format!("0x{hex}"),
                        decoded: false,
                    },
                }
            }
            other => undecoded_option_value("unknown", wire_debug_repr(&other)),
        },
        _ => match value {
            OptionWireValue::Varint(raw) => OptionValueSchema {
                kind: "int".to_owned(),
                bool_value: None,
                int_value: Some(raw as i64),
                float_value: None,
                enum_name: None,
                string_value: None,
                bytes_hex: None,
                message_hex: None,
                message_fields: Vec::new(),
                raw_repr: raw.to_string(),
                decoded: true,
            },
            OptionWireValue::Fixed32(raw) => OptionValueSchema {
                kind: "int".to_owned(),
                bool_value: None,
                int_value: Some(i64::from(raw)),
                float_value: None,
                enum_name: None,
                string_value: None,
                bytes_hex: None,
                message_hex: None,
                message_fields: Vec::new(),
                raw_repr: raw.to_string(),
                decoded: true,
            },
            OptionWireValue::Fixed64(raw) => OptionValueSchema {
                kind: "int".to_owned(),
                bool_value: None,
                int_value: Some(raw as i64),
                float_value: None,
                enum_name: None,
                string_value: None,
                bytes_hex: None,
                message_hex: None,
                message_fields: Vec::new(),
                raw_repr: raw.to_string(),
                decoded: true,
            },
            OptionWireValue::LengthDelimited(bytes) => {
                let hex = bytes_to_hex(&bytes);
                undecoded_option_value("bytes", format!("0x{hex}"))
            }
        },
    }
}

fn undecoded_option_value(kind: &str, raw_repr: String) -> OptionValueSchema {
    OptionValueSchema {
        kind: kind.to_owned(),
        bool_value: None,
        int_value: None,
        float_value: None,
        enum_name: None,
        string_value: None,
        bytes_hex: None,
        message_hex: None,
        message_fields: Vec::new(),
        raw_repr,
        decoded: false,
    }
}

fn decode_int32_value(
    value: OptionWireValue,
    convert: impl Fn(u32) -> i64,
) -> OptionValueSchema {
    match value {
        OptionWireValue::Fixed32(raw) => OptionValueSchema {
            kind: "int".to_owned(),
            bool_value: None,
            int_value: Some(convert(raw)),
            float_value: None,
            enum_name: None,
            string_value: None,
            bytes_hex: None,
            message_hex: None,
            message_fields: Vec::new(),
            raw_repr: convert(raw).to_string(),
            decoded: true,
        },
        other => undecoded_option_value("unknown", wire_debug_repr(&other)),
    }
}

fn decode_int64_value(
    value: OptionWireValue,
    convert: impl Fn(u64) -> i64,
) -> OptionValueSchema {
    match value {
        OptionWireValue::Fixed64(raw) => OptionValueSchema {
            kind: "int".to_owned(),
            bool_value: None,
            int_value: Some(convert(raw)),
            float_value: None,
            enum_name: None,
            string_value: None,
            bytes_hex: None,
            message_hex: None,
            message_fields: Vec::new(),
            raw_repr: convert(raw).to_string(),
            decoded: true,
        },
        other => undecoded_option_value("unknown", wire_debug_repr(&other)),
    }
}

fn decode_varint_value(
    value: OptionWireValue,
    convert: impl Fn(u64) -> i64,
) -> OptionValueSchema {
    match value {
        OptionWireValue::Varint(raw) => OptionValueSchema {
            kind: "int".to_owned(),
            bool_value: None,
            int_value: Some(convert(raw)),
            float_value: None,
            enum_name: None,
            string_value: None,
            bytes_hex: None,
            message_hex: None,
            message_fields: Vec::new(),
            raw_repr: convert(raw).to_string(),
            decoded: true,
        },
        other => undecoded_option_value("unknown", wire_debug_repr(&other)),
    }
}

fn is_packed_kind(kind: &str) -> bool {
    !matches!(kind, "string" | "bytes" | "message" | "group")
}

fn decode_packed_option_values(
    bytes: &[u8],
    kind: &str,
    type_name: Option<&str>,
    registry: &ExtensionRegistrySet,
) -> Result<Vec<OptionValueSchema>> {
    let mut input = CodedInputStream::from_bytes(bytes);
    let mut values = Vec::new();
    while !input.eof()? {
        let value = match kind {
            "bool" => OptionWireValue::Varint(u64::from(input.read_bool()?)),
            "int32" | "uint32" | "enum" => OptionWireValue::Varint(u64::from(input.read_uint32()?)),
            "int64" | "uint64" => OptionWireValue::Varint(input.read_uint64()?),
            "sint32" => OptionWireValue::Varint(input.read_uint32()? as u64),
            "sint64" => OptionWireValue::Varint(input.read_uint64()?),
            "fixed32" | "sfixed32" | "float" => OptionWireValue::Fixed32(input.read_fixed32()?),
            "fixed64" | "sfixed64" | "double" => OptionWireValue::Fixed64(input.read_fixed64()?),
            _ => bail!("unsupported packed kind `{kind}`"),
        };
        values.push(decode_option_value(value, kind, type_name, registry));
    }
    Ok(values)
}

fn decode_message_fields(
    bytes: &[u8],
    type_name: &str,
    registry: &ExtensionRegistrySet,
) -> Result<Vec<OptionFieldSchema>> {
    let Some(message_type) = registry.message_types.get(type_name) else {
        bail!("unknown registered message type `{type_name}`");
    };
    let mut input = CodedInputStream::from_bytes(bytes);
    let mut grouped = BTreeMap::<u32, Vec<OptionWireValue>>::new();
    while let Some(tag) = input.read_raw_tag_or_eof()? {
        let field_number = tag >> 3;
        let wire_type = WireType::new(tag & 0x7)
            .ok_or_else(|| anyhow!("invalid wire type in `{type_name}`"))?;
        let value = input.read_unknown(wire_type)?;
        grouped.entry(field_number).or_default().push(value.into());
    }

    let mut fields = Vec::new();
    for (field_number, values) in grouped {
        let Some(field) = message_type.fields_by_number.get(&field_number) else {
            continue;
        };
        let decoded_values = decode_option_values(
            values,
            &field.kind,
            field.type_name.as_deref(),
            field.is_repeated,
            registry,
        );
        fields.push(OptionFieldSchema {
            name: field.name.clone(),
            full_name: field.full_name.clone(),
            number: i32::try_from(field.number).unwrap_or(i32::MAX),
            kind: field.kind.clone(),
            type_name: field.type_name.clone(),
            is_repeated: field.is_repeated,
            decoded: decoded_values.iter().all(|value| value.decoded),
            values: decoded_values,
        });
    }
    fields.sort_by(|left, right| left.number.cmp(&right.number));
    Ok(fields)
}

fn decode_zig_zag_32(raw: u32) -> i32 {
    ((raw >> 1) as i32) ^ -((raw & 1) as i32)
}

fn decode_zig_zag_64(raw: u64) -> i64 {
    ((raw >> 1) as i64) ^ -((raw & 1) as i64)
}

fn wire_debug_repr(value: &OptionWireValue) -> String {
    match value {
        OptionWireValue::Fixed32(raw) => format!("Fixed32({raw})"),
        OptionWireValue::Fixed64(raw) => format!("Fixed64({raw})"),
        OptionWireValue::Varint(raw) => format!("Varint({raw})"),
        OptionWireValue::LengthDelimited(bytes) => format!("LengthDelimited(0x{})", bytes_to_hex(bytes)),
    }
}

fn collect_uninterpreted_options(debug_repr: String) -> Vec<UninterpretedOptionSchema> {
    let marker = "uninterpreted_option:";
    let mut output = Vec::new();
    for line in debug_repr.lines() {
        if let Some(index) = line.find(marker) {
            output.push(UninterpretedOptionSchema {
                name: "uninterpreted_option".to_owned(),
                value: line[index + marker.len()..].trim().to_owned(),
            });
        }
    }
    output
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(nibble_to_hex(byte >> 4));
        output.push(nibble_to_hex(byte & 0x0f));
    }
    output
}

fn nibble_to_hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'a' + (nibble - 10)) as char,
        _ => unreachable!("hex nibble must be in range"),
    }
}

fn build_context(
    changed_files: &[ChangedProtoFile],
    before: &BTreeMap<String, FlatFileSchema>,
    after: &BTreeMap<String, FlatFileSchema>,
    registries: &[ExtensionRegistryInfo],
    before_backend: ParserBackend,
    after_backend: ParserBackend,
    severity: Severity,
) -> StarlarkProtoContext {
    let mut paths = BTreeSet::new();
    for changed_file in changed_files {
        if let Some(path) = changed_file.base_path.as_ref() {
            paths.insert(path.to_string_lossy().to_string());
        }
        if let Some(path) = changed_file.current_path.as_ref() {
            paths.insert(path.to_string_lossy().to_string());
        }
        match changed_file.kind {
            ChangeKind::Added | ChangeKind::Modified | ChangeKind::Deleted | ChangeKind::Renamed => {}
        }
    }

    let mut files = Vec::new();
    let mut deltas = Vec::new();

    for path in paths {
        let before_file = before.get(&path).cloned();
        let after_file = after.get(&path).cloned();
        files.push(DescriptorPair {
            path: path.clone(),
            before: before_file.as_ref().map(|file| file.file.clone()),
            after: after_file.as_ref().map(|file| file.file.clone()),
        });
        deltas.extend(diff_file(&path, before_file.as_ref(), after_file.as_ref()));
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));
    deltas.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.kind.cmp(&right.kind))
            .then(left.symbol.cmp(&right.symbol))
    });

    StarlarkProtoContext {
        config: StarlarkPolicyConfig {
            default_severity: severity_name(severity).to_owned(),
            default_remediation: DEFAULT_PROTOBUF_EVOLUTION_REMEDIATION.to_owned(),
        },
        parser: StarlarkParserContext {
            before_backend: parser_backend_name(before_backend).to_owned(),
            after_backend: parser_backend_name(after_backend).to_owned(),
        },
        registries: registries.to_vec(),
        files,
        deltas,
    }
}

fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
    }
}

fn parser_backend_name(backend: ParserBackend) -> &'static str {
    match backend {
        ParserBackend::Auto => "auto",
        ParserBackend::Protoc => "protoc",
        ParserBackend::Pure => "pure",
    }
}

fn severity_attr(name: &str) -> FrozenAttr<SeverityEnumValue> {
    FrozenAttr::new(match name {
        "error" => severity_error(),
        "warning" => severity_warning(),
        "info" => severity_info(),
        other => panic!("unsupported severity enum `{other}`"),
    })
}

fn parser_backend_attr(name: &str) -> FrozenAttr<ParserBackendEnumValue> {
    FrozenAttr::new(match name {
        "auto" => parser_backend_auto(),
        "protoc" => parser_backend_protoc(),
        "pure" => parser_backend_pure(),
        other => panic!("unsupported parser backend enum `{other}`"),
    })
}

fn field_label_attr(name: &str) -> FrozenAttr<FieldLabelEnumValue> {
    FrozenAttr::new(match name {
        "optional" => field_label_optional(),
        "required" => field_label_required(),
        "repeated" => field_label_repeated(),
        other => panic!("unsupported field label enum `{other}`"),
    })
}

fn field_kind_attr(name: &str) -> FrozenAttr<FieldKindEnumValue> {
    FrozenAttr::new(match name {
        "double" => field_kind_double(),
        "float" => field_kind_float(),
        "int64" => field_kind_int64(),
        "uint64" => field_kind_uint64(),
        "int32" => field_kind_int32(),
        "fixed64" => field_kind_fixed64(),
        "fixed32" => field_kind_fixed32(),
        "bool" => field_kind_bool(),
        "string" => field_kind_string(),
        "group" => field_kind_group(),
        "message" => field_kind_message(),
        "bytes" => field_kind_bytes(),
        "uint32" => field_kind_uint32(),
        "enum" => field_kind_enum(),
        "sfixed32" => field_kind_sfixed32(),
        "sfixed64" => field_kind_sfixed64(),
        "sint32" => field_kind_sint32(),
        "sint64" => field_kind_sint64(),
        other => panic!("unsupported field kind enum `{other}`"),
    })
}

fn delta_kind_attr(name: &str) -> FrozenAttr<DeltaKindEnumValue> {
    FrozenAttr::new(delta_kind_value(name))
}

fn option_value_kind_attr(name: &str) -> FrozenAttr<OptionValueKindEnumValue> {
    FrozenAttr::new(match name {
        "bool" => option_value_kind_bool(),
        "int" => option_value_kind_int(),
        "enum" => option_value_kind_enum(),
        "float" => option_value_kind_float(),
        "string" => option_value_kind_string(),
        "bytes" => option_value_kind_bytes(),
        "message" => option_value_kind_message(),
        "unknown" => option_value_kind_unknown(),
        other => panic!("unsupported option value kind enum `{other}`"),
    })
}

fn diff_file(
    path: &str,
    before: Option<&FlatFileSchema>,
    after: Option<&FlatFileSchema>,
) -> Vec<SchemaDelta> {
    let mut deltas = Vec::new();
    let Some(before) = before else {
        return deltas;
    };

    if after.is_none() {
        for message in before.messages.values() {
            deltas.push(delta(path, "message_removed", &message.full_name, BTreeMap::new()));
        }
        for enum_schema in before.enums.values() {
            deltas.push(delta(path, "enum_removed", &enum_schema.full_name, BTreeMap::new()));
        }
        return deltas;
    }
    let after = after.expect("checked above");

    if before.file.package != after.file.package {
        deltas.push(delta(
            path,
            "package_changed",
            path,
            btreemap([
                ("before_package", maybe_json_string(Some(before.file.package.clone()))),
                ("after_package", maybe_json_string(Some(after.file.package.clone()))),
            ]),
        ));
    }

    if before.file.syntax != after.file.syntax {
        deltas.push(delta(
            path,
            "syntax_changed",
            path,
            btreemap([
                ("before_syntax", maybe_json_string(Some(before.file.syntax.clone()))),
                ("after_syntax", maybe_json_string(Some(after.file.syntax.clone()))),
            ]),
        ));
    }

    push_option_delta(
        &mut deltas,
        path,
        "file_options_changed",
        path,
        &before.file.options,
        &after.file.options,
    );
    diff_extensions(
        &mut deltas,
        path,
        &before.file.package,
        &before.extensions_by_number,
        &before.extensions_by_name,
        &after.extensions_by_number,
        &after.extensions_by_name,
    );

    for (message_name, before_message) in &before.messages {
        let Some(after_message) = after.messages.get(message_name) else {
            deltas.push(delta(path, "message_removed", &before_message.full_name, BTreeMap::new()));
            continue;
        };
        if before_message.is_map_entry != after_message.is_map_entry {
            deltas.push(delta(
                path,
                "map_entry_changed",
                &before_message.full_name,
                btreemap([
                    ("before_map_entry", serde_json::Value::Bool(before_message.is_map_entry)),
                    ("after_map_entry", serde_json::Value::Bool(after_message.is_map_entry)),
                ]),
            ));
        }
        if before_message.reserved_ranges != after_message.reserved_ranges {
            for range in before_message
                .reserved_ranges
                .difference(&after_message.reserved_ranges)
            {
                deltas.push(delta(
                    path,
                    "message_reserved_range_removed",
                    &before_message.full_name,
                    btreemap([
                        ("range_start", serde_json::Value::from(range.start)),
                        ("range_end", serde_json::Value::from(range.end)),
                    ]),
                ));
            }
        }
        if before_message.reserved_names != after_message.reserved_names {
            for name in before_message
                .reserved_names
                .difference(&after_message.reserved_names)
            {
                deltas.push(delta(
                    path,
                    "message_reserved_name_removed",
                    &before_message.full_name,
                    btreemap([("name", serde_json::Value::String(name.clone()))]),
                ));
            }
        }
        push_option_delta(
            &mut deltas,
            path,
            "message_options_changed",
            &before_message.full_name,
            &before_message.options,
            &after_message.options,
        );
        for (oneof_name, before_oneof) in &before_message.oneofs_by_name {
            let Some(after_oneof) = after_message.oneofs_by_name.get(oneof_name) else {
                deltas.push(delta(
                    path,
                    "oneof_removed",
                    &format!("{}.{}", before_message.full_name, oneof_name),
                    BTreeMap::new(),
                ));
                continue;
            };
            push_option_delta(
                &mut deltas,
                path,
                "oneof_options_changed",
                &before_oneof.full_name,
                &before_oneof.options,
                &after_oneof.options,
            );
        }
        diff_extensions(
            &mut deltas,
            path,
            &before_message.full_name,
            &before_message.extensions_by_number,
            &before_message.extensions_by_name,
            &after_message.extensions_by_number,
            &after_message.extensions_by_name,
        );

        for (field_number, before_field) in &before_message.fields_by_number {
            match after_message.fields_by_number.get(field_number) {
                Some(after_field) => {
                    if field_type_key(before_field) != field_type_key(after_field) {
                        deltas.push(delta(
                            path,
                            "field_type_changed",
                            &format!("{}.{}", before_message.full_name, before_field.name),
                            btreemap([
                                ("before_kind", serde_json::Value::String(before_field.kind.clone())),
                                ("after_kind", serde_json::Value::String(after_field.kind.clone())),
                                (
                                    "before_type_name",
                                    maybe_json_string(before_field.type_name.clone()),
                                ),
                                (
                                    "after_type_name",
                                    maybe_json_string(after_field.type_name.clone()),
                                ),
                                ("field_number", serde_json::Value::from(*field_number)),
                            ]),
                        ));
                    }
                    if before_field.label != after_field.label {
                        deltas.push(delta(
                            path,
                            "field_label_changed",
                            &before_field.full_name,
                            btreemap([
                                ("before_label", serde_json::Value::String(before_field.label.clone())),
                                ("after_label", serde_json::Value::String(after_field.label.clone())),
                                ("field_number", serde_json::Value::from(*field_number)),
                            ]),
                        ));
                    }
                    if before_field.oneof_name != after_field.oneof_name {
                        deltas.push(delta(
                            path,
                            "field_oneof_changed",
                            &before_field.full_name,
                            btreemap([
                                (
                                    "before_oneof",
                                    maybe_json_string(before_field.oneof_name.clone()),
                                ),
                                ("after_oneof", maybe_json_string(after_field.oneof_name.clone())),
                                ("field_number", serde_json::Value::from(*field_number)),
                            ]),
                        ));
                    }
                    push_option_delta(
                        &mut deltas,
                        path,
                        "field_options_changed",
                        &before_field.full_name,
                        &before_field.options,
                        &after_field.options,
                    );
                }
                None => {
                    if let Some(after_field) = after_message.fields_by_name.get(&before_field.name) {
                        deltas.push(delta(
                            path,
                            "field_number_changed",
                            &before_field.full_name,
                            btreemap([
                                ("before_number", serde_json::Value::from(before_field.number)),
                                ("after_number", serde_json::Value::from(after_field.number)),
                            ]),
                        ));
                    } else {
                        deltas.push(delta(
                            path,
                            "field_removed",
                            &before_field.full_name,
                            btreemap([
                                ("field_number", serde_json::Value::from(before_field.number)),
                                ("field_label", serde_json::Value::String(before_field.label.clone())),
                                ("field_kind", serde_json::Value::String(before_field.kind.clone())),
                                (
                                    "field_type_name",
                                    maybe_json_string(before_field.type_name.clone()),
                                ),
                            ]),
                        ));
                    }
                }
            }
        }
    }

    for (enum_name, before_enum) in &before.enums {
        let Some(after_enum) = after.enums.get(enum_name) else {
            deltas.push(delta(path, "enum_removed", &before_enum.full_name, BTreeMap::new()));
            continue;
        };
        if before_enum.reserved_ranges != after_enum.reserved_ranges {
            for range in before_enum.reserved_ranges.difference(&after_enum.reserved_ranges) {
                deltas.push(delta(
                    path,
                    "enum_reserved_range_removed",
                    &before_enum.full_name,
                    btreemap([
                        ("range_start", serde_json::Value::from(range.start)),
                        ("range_end", serde_json::Value::from(range.end)),
                    ]),
                ));
            }
        }
        if before_enum.reserved_names != after_enum.reserved_names {
            for name in before_enum.reserved_names.difference(&after_enum.reserved_names) {
                deltas.push(delta(
                    path,
                    "enum_reserved_name_removed",
                    &before_enum.full_name,
                    btreemap([("name", serde_json::Value::String(name.clone()))]),
                ));
            }
        }
        push_option_delta(
            &mut deltas,
            path,
            "enum_options_changed",
            &before_enum.full_name,
            &before_enum.options,
            &after_enum.options,
        );

        for (number, before_value) in &before_enum.values_by_number {
            if after_enum.values_by_number.contains_key(number) {
                if let Some(after_value) = after_enum.values_by_number.get(number) {
                    push_option_delta(
                        &mut deltas,
                        path,
                        "enum_value_options_changed",
                        &before_value.full_name,
                        &before_value.options,
                        &after_value.options,
                    );
                }
                continue;
            }
            if let Some(after_value) = after_enum.values_by_name.get(&before_value.name) {
                deltas.push(delta(
                    path,
                    "enum_value_number_changed",
                    &before_value.full_name,
                    btreemap([
                        ("before_number", serde_json::Value::from(before_value.number)),
                        ("after_number", serde_json::Value::from(after_value.number)),
                    ]),
                ));
            } else {
                deltas.push(delta(
                    path,
                    "enum_value_removed",
                    &format!("{}.{}", before_enum.full_name, before_value.name),
                    btreemap([("number", serde_json::Value::from(*number))]),
                ));
            }
        }
    }

    for (service_name, before_service) in &before.services {
        let Some(after_service) = after.services.get(service_name) else {
            deltas.push(delta(
                path,
                "service_removed",
                &before_service.full_name,
                BTreeMap::new(),
            ));
            continue;
        };
        push_option_delta(
            &mut deltas,
            path,
            "service_options_changed",
            &before_service.full_name,
            &before_service.options,
            &after_service.options,
        );
        let before_methods = before_service
            .methods
            .iter()
            .map(|method| (method.name.clone(), method))
            .collect::<BTreeMap<_, _>>();
        let after_methods = after_service
            .methods
            .iter()
            .map(|method| (method.name.clone(), method))
            .collect::<BTreeMap<_, _>>();
        for (method_name, before_method) in before_methods {
            let Some(after_method) = after_methods.get(&method_name) else {
                deltas.push(delta(
                    path,
                    "method_removed",
                    &before_method.full_name,
                    BTreeMap::new(),
                ));
                continue;
            };
            if before_method.input_type != after_method.input_type
                || before_method.output_type != after_method.output_type
                || before_method.client_streaming != after_method.client_streaming
                || before_method.server_streaming != after_method.server_streaming
            {
                deltas.push(delta(
                    path,
                    "method_signature_changed",
                    &before_method.full_name,
                    btreemap([
                        (
                            "before_input_type",
                            serde_json::Value::String(before_method.input_type.clone()),
                        ),
                        (
                            "after_input_type",
                            serde_json::Value::String(after_method.input_type.clone()),
                        ),
                        (
                            "before_output_type",
                            serde_json::Value::String(before_method.output_type.clone()),
                        ),
                        (
                            "after_output_type",
                            serde_json::Value::String(after_method.output_type.clone()),
                        ),
                        (
                            "before_client_streaming",
                            serde_json::Value::Bool(before_method.client_streaming),
                        ),
                        (
                            "after_client_streaming",
                            serde_json::Value::Bool(after_method.client_streaming),
                        ),
                        (
                            "before_server_streaming",
                            serde_json::Value::Bool(before_method.server_streaming),
                        ),
                        (
                            "after_server_streaming",
                            serde_json::Value::Bool(after_method.server_streaming),
                        ),
                    ]),
                ));
            }
            push_option_delta(
                &mut deltas,
                path,
                "method_options_changed",
                &before_method.full_name,
                &before_method.options,
                &after_method.options,
            );
        }
    }

    deltas
}

fn field_type_key(field: &FieldSchema) -> (String, Option<String>) {
    (field.kind.clone(), field.type_name.clone())
}

fn diff_extensions(
    deltas: &mut Vec<SchemaDelta>,
    path: &str,
    owner_symbol: &str,
    before_by_number: &BTreeMap<i32, FieldSchema>,
    _before_by_name: &BTreeMap<String, FieldSchema>,
    after_by_number: &BTreeMap<i32, FieldSchema>,
    after_by_name: &BTreeMap<String, FieldSchema>,
) {
    for (field_number, before_field) in before_by_number {
        match after_by_number.get(field_number) {
            Some(after_field) => {
                if field_type_key(before_field) != field_type_key(after_field) {
                    deltas.push(delta(
                        path,
                        "extension_type_changed",
                        &before_field.full_name,
                        btreemap([
                            ("before_kind", serde_json::Value::String(before_field.kind.clone())),
                            ("after_kind", serde_json::Value::String(after_field.kind.clone())),
                            (
                                "before_type_name",
                                maybe_json_string(before_field.type_name.clone()),
                            ),
                            ("after_type_name", maybe_json_string(after_field.type_name.clone())),
                            ("field_number", serde_json::Value::from(*field_number)),
                        ]),
                    ));
                }
                if before_field.label != after_field.label {
                    deltas.push(delta(
                        path,
                        "extension_label_changed",
                        &before_field.full_name,
                        btreemap([
                            ("before_label", serde_json::Value::String(before_field.label.clone())),
                            ("after_label", serde_json::Value::String(after_field.label.clone())),
                            ("field_number", serde_json::Value::from(*field_number)),
                        ]),
                    ));
                }
                push_option_delta(
                    deltas,
                    path,
                    "extension_options_changed",
                    &before_field.full_name,
                    &before_field.options,
                    &after_field.options,
                );
            }
            None => {
                if let Some(after_field) = after_by_name.get(&before_field.name) {
                    deltas.push(delta(
                        path,
                        "extension_number_changed",
                        &before_field.full_name,
                        btreemap([
                            ("before_number", serde_json::Value::from(before_field.number)),
                            ("after_number", serde_json::Value::from(after_field.number)),
                        ]),
                    ));
                } else {
                    deltas.push(delta(
                        path,
                        "extension_removed",
                        &format!("{owner_symbol}.{}", before_field.name),
                        btreemap([("field_number", serde_json::Value::from(before_field.number))]),
                    ));
                }
            }
        }
    }
}

fn push_option_delta(
    deltas: &mut Vec<SchemaDelta>,
    path: &str,
    kind: &str,
    symbol: &str,
    before: &DescriptorOptionsSchema,
    after: &DescriptorOptionsSchema,
) {
    if before.fingerprint != after.fingerprint {
        deltas.push(delta(
            path,
            kind,
            symbol,
            btreemap([
                (
                    "before_option_fingerprint",
                    maybe_json_string(Some(before.fingerprint.clone())),
                ),
                (
                    "after_option_fingerprint",
                    maybe_json_string(Some(after.fingerprint.clone())),
                ),
            ]),
        ));
    }
    push_registered_option_deltas(deltas, path, symbol, before, after);
}

fn push_registered_option_deltas(
    deltas: &mut Vec<SchemaDelta>,
    path: &str,
    symbol: &str,
    before: &DescriptorOptionsSchema,
    after: &DescriptorOptionsSchema,
) {
    let before_map = before
        .extensions
        .iter()
        .map(|extension| (extension.full_name.clone(), extension))
        .collect::<BTreeMap<_, _>>();
    let after_map = after
        .extensions
        .iter()
        .map(|extension| (extension.full_name.clone(), extension))
        .collect::<BTreeMap<_, _>>();

    for (full_name, before_extension) in &before_map {
        let delta_symbol = format!("{symbol}::{full_name}");
        match after_map.get(full_name) {
            Some(after_extension) => {
                let before_values = before_extension
                    .values
                    .iter()
                    .map(|value| value.raw_repr.clone())
                    .collect::<Vec<_>>();
                let after_values = after_extension
                    .values
                    .iter()
                    .map(|value| value.raw_repr.clone())
                    .collect::<Vec<_>>();
                if before_values != after_values || before_extension.decoded != after_extension.decoded {
                    deltas.push(delta(
                        path,
                        "registered_option_value_changed",
                        &delta_symbol,
                        btreemap([
                            ("name", serde_json::Value::String(full_name.clone())),
                            (
                                "registry_name",
                                serde_json::Value::String(before_extension.registry_name.clone()),
                            ),
                            (
                                "before_raw_value",
                                serde_json::Value::String(before_values.join(", ")),
                            ),
                            (
                                "after_raw_value",
                                serde_json::Value::String(after_values.join(", ")),
                            ),
                        ]),
                    ));
                }
            }
            None => {
                deltas.push(delta(
                    path,
                    "registered_option_removed",
                    &delta_symbol,
                    btreemap([
                        ("name", serde_json::Value::String(full_name.clone())),
                        (
                            "registry_name",
                            serde_json::Value::String(before_extension.registry_name.clone()),
                        ),
                        (
                            "before_raw_value",
                            serde_json::Value::String(
                                before_extension
                                    .values
                                    .iter()
                                    .map(|value| value.raw_repr.clone())
                                    .collect::<Vec<_>>()
                                    .join(", "),
                            ),
                        ),
                    ]),
                ));
            }
        }
    }
}

fn maybe_json_string(value: Option<String>) -> serde_json::Value {
    match value {
        Some(value) => serde_json::Value::String(value),
        None => serde_json::Value::Null,
    }
}

fn btreemap<const N: usize>(entries: [(&str, serde_json::Value); N]) -> BTreeMap<String, serde_json::Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

fn delta(
    path: &str,
    kind: &str,
    symbol: &str,
    details: BTreeMap<String, serde_json::Value>,
) -> SchemaDelta {
    SchemaDelta {
        kind: kind.to_owned(),
        path: path.to_owned(),
        symbol: symbol.to_owned(),
        details,
    }
}

fn run_starlark_policy(
    tree: &dyn SourceTree,
    script_path: &Path,
    context: &StarlarkProtoContext,
) -> Result<Vec<Finding>> {
    let source = String::from_utf8(
        tree.read_file(script_path).with_context(|| {
            format!(
                "read protobuf evolution Starlark `{}`",
                script_path.display()
            )
        })?,
    )
    .with_context(|| {
        format!(
            "protobuf evolution Starlark `{}` is not valid utf-8",
            script_path.display()
        )
    })?;

    run_starlark_source(&source, &script_path.to_string_lossy(), context)
}

fn run_starlark_source(
    source: &str,
    source_name: &str,
    context: &StarlarkProtoContext,
) -> Result<Vec<Finding>> {
    let dialect = Dialect {
        enable_types: DialectTypes::Enable,
        ..Dialect::Standard
    };
    let ast = AstModule::parse(source_name, source.to_owned(), &dialect)
        .map_err(|error| anyhow!("failed to parse protobuf evolution Starlark: {error}"))?;

    let globals = protobuf_starlark_globals();
    let module = Module::new();
    let mut evaluator = Evaluator::new(&module);
    evaluator
        .eval_module(ast, &globals)
        .map_err(|error| anyhow!("protobuf evolution Starlark failed: {error}"))?;
    let check = module.get("check").ok_or_else(|| {
        anyhow!("protobuf evolution Starlark `{source_name}` must define a public `check` function")
    })?;
    let ctx = module.heap().alloc(context.clone().into_proto_context_value());
    let value = evaluator
        .eval_function(check, &[ctx], &[])
        .map_err(|error| anyhow!("protobuf evolution Starlark failed calling `check`: {error}"))?;
    unpack_starlark_findings(value).with_context(|| {
        format!(
            "protobuf evolution Starlark `{source_name}` must return `list[Finding]`"
        )
    })
}

fn unpack_starlark_findings(value: starlark::values::Value<'_>) -> Result<Vec<Finding>> {
    let list = ListRef::from_value(value)
        .ok_or_else(|| anyhow!("expected a list of `Finding` values"))?;
    list.iter()
        .map(|item| {
            let finding = FindingValue::from_value(item)
                .ok_or_else(|| anyhow!("expected each returned element to be a `Finding`"))?;
            finding.to_checkleft_finding()
        })
        .collect()
}

impl FindingValue {
    fn to_checkleft_finding(&self) -> Result<Finding> {
        let severity = match severity_value_name(&self.severity) {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        "info" => Severity::Info,
        other => bail!("invalid protobuf evolution Starlark finding severity `{other}`"),
        };

        let location = if let Some(path) = self.path.0.as_ref() {
            Some(Location {
                path: PathBuf::from(path),
                line: self.line.0.map(|line| line as u32),
                column: self.column.0.map(|column| column as u32),
            })
        } else {
            None
        };

        Ok(Finding {
            severity,
            message: self.message.clone(),
            location,
            remediation: self.remediation.0.clone(),
            suggested_fix: None,
        })
    }
}

impl StarlarkProtoContext {
    fn into_proto_context_value(self) -> ProtoContextValue {
        ProtoContextValue {
            config: PolicyConfigValue {
                default_severity: severity_attr(&self.config.default_severity),
                default_remediation: self.config.default_remediation,
            },
            parser: ParserInfoValue {
                before_backend: parser_backend_attr(&self.parser.before_backend),
                after_backend: parser_backend_attr(&self.parser.after_backend),
            },
            registries: self
                .registries
                .into_iter()
                .map(ExtensionRegistryInfo::into_value)
                .collect(),
            files: self
                .files
                .into_iter()
                .map(DescriptorPair::into_value)
                .collect(),
            deltas: self
                .deltas
                .into_iter()
                .map(SchemaDelta::into_value)
                .collect(),
        }
    }
}

impl DescriptorPair {
    fn into_value(self) -> DescriptorPairValue {
        DescriptorPairValue {
            path: self.path,
            before: OptionalAttr::from(self.before.map(FileSchema::into_value)),
            after: OptionalAttr::from(self.after.map(FileSchema::into_value)),
        }
    }
}

impl ExtensionRegistryInfo {
    fn into_value(self) -> ExtensionRegistryInfoValue {
        ExtensionRegistryInfoValue {
            name: self.name,
            extension_count: self.extension_count,
            files: self.files,
            extendees: self.extendees,
        }
    }
}

impl FileSchema {
    fn into_value(self) -> FileDescriptorValue {
        FileDescriptorValue {
            path: self.path,
            package: self.package,
            syntax: self.syntax,
            options: self.options.into_value(),
            messages: self
                .messages
                .into_iter()
                .map(MessageSchema::into_value)
                .collect(),
            enums: self.enums.into_iter().map(EnumSchema::into_value).collect(),
            services: self
                .services
                .into_iter()
                .map(ServiceSchema::into_value)
                .collect(),
            extensions: self
                .extensions
                .into_iter()
                .map(FieldSchema::into_value)
                .collect(),
        }
    }
}

impl MessageSchema {
    fn into_value(self) -> MessageDescriptorValue {
        MessageDescriptorValue {
            full_name: self.full_name,
            name: self.name,
            options: self.options.into_value(),
            is_map_entry: self.is_map_entry,
            fields: self
                .fields
                .into_iter()
                .map(FieldSchema::into_value)
                .collect(),
            oneofs: self
                .oneofs
                .into_iter()
                .map(OneofSchema::into_value)
                .collect(),
            extensions: self
                .extensions
                .into_iter()
                .map(FieldSchema::into_value)
                .collect(),
            reserved_ranges: self
                .reserved_ranges
                .into_iter()
                .map(ReservedRangeSchema::into_value)
                .collect(),
            reserved_names: self.reserved_names,
            nested_messages: self
                .nested_messages
                .into_iter()
                .map(MessageSchema::into_value)
                .collect(),
            nested_enums: self
                .nested_enums
                .into_iter()
                .map(EnumSchema::into_value)
                .collect(),
        }
    }
}

impl FieldSchema {
    fn into_value(self) -> FieldDescriptorValue {
        FieldDescriptorValue {
            full_name: self.full_name,
            name: self.name,
            number: self.number,
            label: field_label_attr(&self.label),
            kind: field_kind_attr(&self.kind),
            type_name: OptionalAttr::from(self.type_name),
            json_name: OptionalAttr::from(self.json_name),
            oneof_index: OptionalAttr::from(self.oneof_index),
            oneof_name: OptionalAttr::from(self.oneof_name),
            proto3_optional: self.proto3_optional,
            extendee: OptionalAttr::from(self.extendee),
            options: self.options.into_value(),
        }
    }
}

impl EnumSchema {
    fn into_value(self) -> EnumDescriptorValue {
        EnumDescriptorValue {
            full_name: self.full_name,
            name: self.name,
            options: self.options.into_value(),
            reserved_ranges: self
                .reserved_ranges
                .into_iter()
                .map(ReservedRangeSchema::into_value)
                .collect(),
            reserved_names: self.reserved_names,
            values: self
                .values
                .into_iter()
                .map(EnumValueSchema::into_value)
                .collect(),
        }
    }
}

impl EnumValueSchema {
    fn into_value(self) -> EnumValueDescriptorValue {
        EnumValueDescriptorValue {
            full_name: self.full_name,
            name: self.name,
            number: self.number,
            options: self.options.into_value(),
        }
    }
}

impl SchemaDelta {
    fn into_value(self) -> SchemaDeltaValue {
        SchemaDeltaValue {
            kind: delta_kind_attr(&self.kind),
            path: self.path,
            symbol: self.symbol,
            before_kind: OptionalAttr::from(
                details_string(&self.details, "before_kind").map(|kind| field_kind_attr(&kind)),
            ),
            after_kind: OptionalAttr::from(
                details_string(&self.details, "after_kind").map(|kind| field_kind_attr(&kind)),
            ),
            before_type_name: OptionalAttr::from(details_string(&self.details, "before_type_name")),
            after_type_name: OptionalAttr::from(details_string(&self.details, "after_type_name")),
            before_label: OptionalAttr::from(
                details_string(&self.details, "before_label").map(|label| field_label_attr(&label)),
            ),
            after_label: OptionalAttr::from(
                details_string(&self.details, "after_label").map(|label| field_label_attr(&label)),
            ),
            before_number: OptionalAttr::from(details_i32(&self.details, "before_number")),
            after_number: OptionalAttr::from(details_i32(&self.details, "after_number")),
            field_number: OptionalAttr::from(details_i32(&self.details, "field_number")),
            number: OptionalAttr::from(details_i32(&self.details, "number")),
            before_package: OptionalAttr::from(details_string(&self.details, "before_package")),
            after_package: OptionalAttr::from(details_string(&self.details, "after_package")),
            before_syntax: OptionalAttr::from(details_string(&self.details, "before_syntax")),
            after_syntax: OptionalAttr::from(details_string(&self.details, "after_syntax")),
            before_input_type: OptionalAttr::from(details_string(&self.details, "before_input_type")),
            after_input_type: OptionalAttr::from(details_string(&self.details, "after_input_type")),
            before_output_type: OptionalAttr::from(details_string(&self.details, "before_output_type")),
            after_output_type: OptionalAttr::from(details_string(&self.details, "after_output_type")),
            before_oneof: OptionalAttr::from(details_string(&self.details, "before_oneof")),
            after_oneof: OptionalAttr::from(details_string(&self.details, "after_oneof")),
            before_option_fingerprint: OptionalAttr::from(details_string(
                &self.details,
                "before_option_fingerprint",
            )),
            after_option_fingerprint: OptionalAttr::from(details_string(
                &self.details,
                "after_option_fingerprint",
            )),
            before_client_streaming: OptionalAttr::from(details_bool(
                &self.details,
                "before_client_streaming",
            )),
            after_client_streaming: OptionalAttr::from(details_bool(
                &self.details,
                "after_client_streaming",
            )),
            before_server_streaming: OptionalAttr::from(details_bool(
                &self.details,
                "before_server_streaming",
            )),
            after_server_streaming: OptionalAttr::from(details_bool(
                &self.details,
                "after_server_streaming",
            )),
            before_map_entry: OptionalAttr::from(details_bool(&self.details, "before_map_entry")),
            after_map_entry: OptionalAttr::from(details_bool(&self.details, "after_map_entry")),
            range_start: OptionalAttr::from(details_i32(&self.details, "range_start")),
            range_end: OptionalAttr::from(details_i32(&self.details, "range_end")),
            name: OptionalAttr::from(details_string(&self.details, "name")),
            registry_name: OptionalAttr::from(details_string(&self.details, "registry_name")),
            before_raw_value: OptionalAttr::from(details_string(&self.details, "before_raw_value")),
            after_raw_value: OptionalAttr::from(details_string(&self.details, "after_raw_value")),
        }
    }
}

impl OneofSchema {
    fn into_value(self) -> OneofDescriptorValue {
        OneofDescriptorValue {
            full_name: self.full_name,
            name: self.name,
            options: self.options.into_value(),
        }
    }
}

impl ServiceSchema {
    fn into_value(self) -> ServiceDescriptorValue {
        ServiceDescriptorValue {
            full_name: self.full_name,
            name: self.name,
            options: self.options.into_value(),
            methods: self
                .methods
                .into_iter()
                .map(MethodSchema::into_value)
                .collect(),
        }
    }
}

impl MethodSchema {
    fn into_value(self) -> MethodDescriptorValue {
        MethodDescriptorValue {
            full_name: self.full_name,
            name: self.name,
            input_type: self.input_type,
            output_type: self.output_type,
            client_streaming: self.client_streaming,
            server_streaming: self.server_streaming,
            options: self.options.into_value(),
        }
    }
}

impl ReservedRangeSchema {
    fn into_value(self) -> ReservedRangeValue {
        ReservedRangeValue {
            start: self.start,
            end: self.end,
        }
    }
}

impl DescriptorOptionsSchema {
    fn into_value(self) -> DescriptorOptionsValue {
        DescriptorOptionsValue {
            fingerprint: self.fingerprint,
            has_unknown_fields: self.has_unknown_fields,
            uninterpreted: self
                .uninterpreted
                .into_iter()
                .map(UninterpretedOptionSchema::into_value)
                .collect(),
            extensions: self
                .extensions
                .into_iter()
                .map(OptionExtensionSchema::into_value)
                .collect(),
        }
    }
}

impl UninterpretedOptionSchema {
    fn into_value(self) -> UninterpretedOptionValue {
        UninterpretedOptionValue {
            name: self.name,
            value: self.value,
        }
    }
}

impl OptionExtensionSchema {
    fn into_value(self) -> OptionExtensionValue {
        OptionExtensionValue {
            registry_name: self.registry_name,
            full_name: self.full_name,
            extendee: self.extendee,
            field_number: self.field_number,
            kind: field_kind_attr(&self.kind),
            type_name: OptionalAttr::from(self.type_name),
            is_repeated: self.is_repeated,
            values: self
                .values
                .into_iter()
                .map(OptionValueSchema::into_value)
                .collect(),
            decoded: self.decoded,
        }
    }
}

impl OptionFieldSchema {
    fn into_value(self) -> OptionFieldValue {
        OptionFieldValue {
            name: self.name,
            full_name: self.full_name,
            number: self.number,
            kind: field_kind_attr(&self.kind),
            type_name: OptionalAttr::from(self.type_name),
            is_repeated: self.is_repeated,
            values: self
                .values
                .into_iter()
                .map(OptionValueSchema::into_value)
                .collect(),
            decoded: self.decoded,
        }
    }
}

impl OptionValueSchema {
    fn into_value(self) -> OptionValueValue {
        OptionValueValue {
            kind: option_value_kind_attr(&self.kind),
            bool_value: OptionalAttr::from(self.bool_value),
            int_value: OptionalAttr::from(self.int_value),
            float_value: OptionalAttr::from(self.float_value),
            enum_name: OptionalAttr::from(self.enum_name),
            string_value: OptionalAttr::from(self.string_value),
            bytes_hex: OptionalAttr::from(self.bytes_hex),
            message_hex: OptionalAttr::from(self.message_hex),
            message_fields: self
                .message_fields
                .into_iter()
                .map(OptionFieldSchema::into_value)
                .collect(),
            raw_repr: self.raw_repr,
            decoded: self.decoded,
        }
    }
}

fn severity_error() -> starlark::values::FrozenValue {
    use starlark::environment::GlobalsStatic;
    static GLOBAL: GlobalsStatic = GlobalsStatic::new();
    GLOBAL.function(|globals| globals.set("v", SeverityEnumValue { value: "error" }))
}

fn severity_warning() -> starlark::values::FrozenValue {
    use starlark::environment::GlobalsStatic;
    static GLOBAL: GlobalsStatic = GlobalsStatic::new();
    GLOBAL.function(|globals| globals.set("v", SeverityEnumValue { value: "warning" }))
}

fn severity_info() -> starlark::values::FrozenValue {
    use starlark::environment::GlobalsStatic;
    static GLOBAL: GlobalsStatic = GlobalsStatic::new();
    GLOBAL.function(|globals| globals.set("v", SeverityEnumValue { value: "info" }))
}

fn parser_backend_auto() -> starlark::values::FrozenValue {
    use starlark::environment::GlobalsStatic;
    static GLOBAL: GlobalsStatic = GlobalsStatic::new();
    GLOBAL.function(|globals| globals.set("v", ParserBackendEnumValue { value: "auto" }))
}

fn parser_backend_protoc() -> starlark::values::FrozenValue {
    use starlark::environment::GlobalsStatic;
    static GLOBAL: GlobalsStatic = GlobalsStatic::new();
    GLOBAL.function(|globals| globals.set("v", ParserBackendEnumValue { value: "protoc" }))
}

fn parser_backend_pure() -> starlark::values::FrozenValue {
    use starlark::environment::GlobalsStatic;
    static GLOBAL: GlobalsStatic = GlobalsStatic::new();
    GLOBAL.function(|globals| globals.set("v", ParserBackendEnumValue { value: "pure" }))
}

fn field_label_optional() -> starlark::values::FrozenValue {
    use starlark::environment::GlobalsStatic;
    static GLOBAL: GlobalsStatic = GlobalsStatic::new();
    GLOBAL.function(|globals| globals.set("v", FieldLabelEnumValue { value: "optional" }))
}

fn field_label_required() -> starlark::values::FrozenValue {
    use starlark::environment::GlobalsStatic;
    static GLOBAL: GlobalsStatic = GlobalsStatic::new();
    GLOBAL.function(|globals| globals.set("v", FieldLabelEnumValue { value: "required" }))
}

fn field_label_repeated() -> starlark::values::FrozenValue {
    use starlark::environment::GlobalsStatic;
    static GLOBAL: GlobalsStatic = GlobalsStatic::new();
    GLOBAL.function(|globals| globals.set("v", FieldLabelEnumValue { value: "repeated" }))
}

fn field_kind_constants() -> [(&'static str, starlark::values::FrozenValue); 18] {
    [
        ("double", field_kind_double()),
        ("float", field_kind_float()),
        ("int64", field_kind_int64()),
        ("uint64", field_kind_uint64()),
        ("int32", field_kind_int32()),
        ("fixed64", field_kind_fixed64()),
        ("fixed32", field_kind_fixed32()),
        ("bool", field_kind_bool()),
        ("string", field_kind_string()),
        ("group", field_kind_group()),
        ("message", field_kind_message()),
        ("bytes", field_kind_bytes()),
        ("uint32", field_kind_uint32()),
        ("enum", field_kind_enum()),
        ("sfixed32", field_kind_sfixed32()),
        ("sfixed64", field_kind_sfixed64()),
        ("sint32", field_kind_sint32()),
        ("sint64", field_kind_sint64()),
    ]
}

macro_rules! field_kind_singleton {
    ($fn_name:ident, $name:literal) => {
        fn $fn_name() -> starlark::values::FrozenValue {
            use starlark::environment::GlobalsStatic;
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|globals| globals.set("v", FieldKindEnumValue { value: $name }))
        }
    };
}

field_kind_singleton!(field_kind_double, "double");
field_kind_singleton!(field_kind_float, "float");
field_kind_singleton!(field_kind_int64, "int64");
field_kind_singleton!(field_kind_uint64, "uint64");
field_kind_singleton!(field_kind_int32, "int32");
field_kind_singleton!(field_kind_fixed64, "fixed64");
field_kind_singleton!(field_kind_fixed32, "fixed32");
field_kind_singleton!(field_kind_bool, "bool");
field_kind_singleton!(field_kind_string, "string");
field_kind_singleton!(field_kind_group, "group");
field_kind_singleton!(field_kind_message, "message");
field_kind_singleton!(field_kind_bytes, "bytes");
field_kind_singleton!(field_kind_uint32, "uint32");
field_kind_singleton!(field_kind_enum, "enum");
field_kind_singleton!(field_kind_sfixed32, "sfixed32");
field_kind_singleton!(field_kind_sfixed64, "sfixed64");
field_kind_singleton!(field_kind_sint32, "sint32");
field_kind_singleton!(field_kind_sint64, "sint64");

fn option_value_kind_bool() -> starlark::values::FrozenValue {
    use starlark::environment::GlobalsStatic;
    static GLOBAL: GlobalsStatic = GlobalsStatic::new();
    GLOBAL.function(|globals| globals.set("v", OptionValueKindEnumValue { value: "bool" }))
}

fn option_value_kind_int() -> starlark::values::FrozenValue {
    use starlark::environment::GlobalsStatic;
    static GLOBAL: GlobalsStatic = GlobalsStatic::new();
    GLOBAL.function(|globals| globals.set("v", OptionValueKindEnumValue { value: "int" }))
}

fn option_value_kind_enum() -> starlark::values::FrozenValue {
    use starlark::environment::GlobalsStatic;
    static GLOBAL: GlobalsStatic = GlobalsStatic::new();
    GLOBAL.function(|globals| globals.set("v", OptionValueKindEnumValue { value: "enum" }))
}

fn option_value_kind_float() -> starlark::values::FrozenValue {
    use starlark::environment::GlobalsStatic;
    static GLOBAL: GlobalsStatic = GlobalsStatic::new();
    GLOBAL.function(|globals| globals.set("v", OptionValueKindEnumValue { value: "float" }))
}

fn option_value_kind_string() -> starlark::values::FrozenValue {
    use starlark::environment::GlobalsStatic;
    static GLOBAL: GlobalsStatic = GlobalsStatic::new();
    GLOBAL.function(|globals| globals.set("v", OptionValueKindEnumValue { value: "string" }))
}

fn option_value_kind_bytes() -> starlark::values::FrozenValue {
    use starlark::environment::GlobalsStatic;
    static GLOBAL: GlobalsStatic = GlobalsStatic::new();
    GLOBAL.function(|globals| globals.set("v", OptionValueKindEnumValue { value: "bytes" }))
}

fn option_value_kind_message() -> starlark::values::FrozenValue {
    use starlark::environment::GlobalsStatic;
    static GLOBAL: GlobalsStatic = GlobalsStatic::new();
    GLOBAL.function(|globals| globals.set("v", OptionValueKindEnumValue { value: "message" }))
}

fn option_value_kind_unknown() -> starlark::values::FrozenValue {
    use starlark::environment::GlobalsStatic;
    static GLOBAL: GlobalsStatic = GlobalsStatic::new();
    GLOBAL.function(|globals| globals.set("v", OptionValueKindEnumValue { value: "unknown" }))
}

fn delta_kind_names() -> &'static [&'static str] {
    &[
        "message_removed",
        "enum_removed",
        "field_removed",
        "field_number_changed",
        "field_type_changed",
        "field_label_changed",
        "field_oneof_changed",
        "enum_value_removed",
        "enum_value_number_changed",
        "message_reserved_range_removed",
        "message_reserved_name_removed",
        "enum_reserved_range_removed",
        "enum_reserved_name_removed",
        "oneof_removed",
        "service_removed",
        "method_removed",
        "method_signature_changed",
        "package_changed",
        "syntax_changed",
        "map_entry_changed",
        "extension_removed",
        "extension_number_changed",
        "extension_type_changed",
        "extension_label_changed",
        "file_options_changed",
        "message_options_changed",
        "field_options_changed",
        "oneof_options_changed",
        "enum_options_changed",
        "enum_value_options_changed",
        "service_options_changed",
        "method_options_changed",
        "extension_options_changed",
        "registered_option_removed",
        "registered_option_value_changed",
    ]
}

fn delta_kind_value(name: &str) -> starlark::values::FrozenValue {
    use starlark::environment::GlobalsStatic;
    match name {
        "message_removed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "message_removed" }))
        }
        "enum_removed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "enum_removed" }))
        }
        "field_removed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "field_removed" }))
        }
        "field_number_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "field_number_changed" }))
        }
        "field_type_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "field_type_changed" }))
        }
        "field_label_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "field_label_changed" }))
        }
        "field_oneof_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "field_oneof_changed" }))
        }
        "enum_value_removed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "enum_value_removed" }))
        }
        "enum_value_number_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "enum_value_number_changed" }))
        }
        "message_reserved_range_removed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "message_reserved_range_removed" }))
        }
        "message_reserved_name_removed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "message_reserved_name_removed" }))
        }
        "enum_reserved_range_removed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "enum_reserved_range_removed" }))
        }
        "enum_reserved_name_removed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "enum_reserved_name_removed" }))
        }
        "oneof_removed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "oneof_removed" }))
        }
        "service_removed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "service_removed" }))
        }
        "method_removed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "method_removed" }))
        }
        "method_signature_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "method_signature_changed" }))
        }
        "package_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "package_changed" }))
        }
        "syntax_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "syntax_changed" }))
        }
        "map_entry_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "map_entry_changed" }))
        }
        "extension_removed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "extension_removed" }))
        }
        "extension_number_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "extension_number_changed" }))
        }
        "extension_type_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "extension_type_changed" }))
        }
        "extension_label_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "extension_label_changed" }))
        }
        "file_options_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "file_options_changed" }))
        }
        "message_options_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "message_options_changed" }))
        }
        "field_options_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "field_options_changed" }))
        }
        "oneof_options_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "oneof_options_changed" }))
        }
        "enum_options_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "enum_options_changed" }))
        }
        "enum_value_options_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "enum_value_options_changed" }))
        }
        "service_options_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "service_options_changed" }))
        }
        "method_options_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "method_options_changed" }))
        }
        "extension_options_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "extension_options_changed" }))
        }
        "registered_option_removed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "registered_option_removed" }))
        }
        "registered_option_value_changed" => {
            static GLOBAL: GlobalsStatic = GlobalsStatic::new();
            GLOBAL.function(|g| g.set("v", DeltaKindEnumValue { value: "registered_option_value_changed" }))
        }
        other => panic!("unsupported delta kind enum `{other}`"),
    }
}

fn severity_value_name(value: &FrozenAttr<SeverityEnumValue>) -> &'static str {
    SeverityEnumValue::from_value(value.value.to_value())
        .expect("severity singleton must decode")
        .value
}

fn delta_kind_value_name(value: &FrozenAttr<DeltaKindEnumValue>) -> &'static str {
    DeltaKindEnumValue::from_value(value.value.to_value())
        .expect("delta kind singleton must decode")
        .value
}

fn parse_delta_kind_param<'v>(
    value: Option<starlark::values::Value<'v>>,
) -> anyhow::Result<Option<&'static str>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if let Some(kind) = DeltaKindEnumValue::from_value(value) {
        return Ok(Some(kind.value));
    }
    if let Some(kind) = value.unpack_str() {
        return Ok(Some(delta_kind_value_name(&delta_kind_attr(kind))));
    }
    Err(anyhow!("expected `DeltaKind` or string delta kind"))
}

fn parse_severity_param<'v>(value: starlark::values::Value<'v>) -> anyhow::Result<&'static str> {
    if let Some(severity) = SeverityEnumValue::from_value(value) {
        return Ok(severity.value);
    }
    if let Some(severity) = value.unpack_str() {
        return Ok(severity_value_name(&severity_attr(severity)));
    }
    Err(anyhow!("expected `Severity` or string severity"))
}

fn build_finding_value(
    severity: &str,
    message: String,
    path: Option<String>,
    line: Option<i32>,
    column: Option<i32>,
    remediation: Option<String>,
) -> FindingValue {
    FindingValue {
        severity: severity_attr(severity),
        message,
        path: OptionalAttr::from(path),
        line: OptionalAttr::from(line),
        column: OptionalAttr::from(column),
        remediation: OptionalAttr::from(remediation),
    }
}

fn protobuf_starlark_globals() -> Globals {
    GlobalsBuilder::extended_by(&[LibraryExtension::Typing])
        .with(protobuf_enum_globals)
        .with(protobuf_type_globals)
        .with(protobuf_helper_globals)
        .build()
}

#[starlark_module]
fn protobuf_type_globals(globals: &mut GlobalsBuilder) {
    const Severity: StarlarkValueAsType<SeverityEnumValue> = StarlarkValueAsType::new();
    const ParserBackend: StarlarkValueAsType<ParserBackendEnumValue> = StarlarkValueAsType::new();
    const FieldLabel: StarlarkValueAsType<FieldLabelEnumValue> = StarlarkValueAsType::new();
    const FieldKind: StarlarkValueAsType<FieldKindEnumValue> = StarlarkValueAsType::new();
    const DeltaKind: StarlarkValueAsType<DeltaKindEnumValue> = StarlarkValueAsType::new();
    const OptionValueKind: StarlarkValueAsType<OptionValueKindEnumValue> =
        StarlarkValueAsType::new();
    const ProtoContext: StarlarkValueAsType<ProtoContextValue> = StarlarkValueAsType::new();
    const PolicyConfig: StarlarkValueAsType<PolicyConfigValue> = StarlarkValueAsType::new();
    const ParserInfo: StarlarkValueAsType<ParserInfoValue> = StarlarkValueAsType::new();
    const ExtensionRegistryInfo: StarlarkValueAsType<ExtensionRegistryInfoValue> =
        StarlarkValueAsType::new();
    const DescriptorPair: StarlarkValueAsType<DescriptorPairValue> = StarlarkValueAsType::new();
    const FileDescriptor: StarlarkValueAsType<FileDescriptorValue> = StarlarkValueAsType::new();
    const MessageDescriptor: StarlarkValueAsType<MessageDescriptorValue> =
        StarlarkValueAsType::new();
    const FieldDescriptor: StarlarkValueAsType<FieldDescriptorValue> = StarlarkValueAsType::new();
    const OneofDescriptor: StarlarkValueAsType<OneofDescriptorValue> = StarlarkValueAsType::new();
    const EnumDescriptor: StarlarkValueAsType<EnumDescriptorValue> = StarlarkValueAsType::new();
    const EnumValueDescriptor: StarlarkValueAsType<EnumValueDescriptorValue> =
        StarlarkValueAsType::new();
    const ServiceDescriptor: StarlarkValueAsType<ServiceDescriptorValue> =
        StarlarkValueAsType::new();
    const MethodDescriptor: StarlarkValueAsType<MethodDescriptorValue> =
        StarlarkValueAsType::new();
    const ReservedRange: StarlarkValueAsType<ReservedRangeValue> = StarlarkValueAsType::new();
    const DescriptorOptions: StarlarkValueAsType<DescriptorOptionsValue> =
        StarlarkValueAsType::new();
    const OptionExtension: StarlarkValueAsType<OptionExtensionValue> = StarlarkValueAsType::new();
    const OptionField: StarlarkValueAsType<OptionFieldValue> = StarlarkValueAsType::new();
    const OptionValue: StarlarkValueAsType<OptionValueValue> = StarlarkValueAsType::new();
    const UninterpretedOption: StarlarkValueAsType<UninterpretedOptionValue> =
        StarlarkValueAsType::new();
    const SchemaDelta: StarlarkValueAsType<SchemaDeltaValue> = StarlarkValueAsType::new();
    const FieldDelta: StarlarkValueAsType<SchemaDeltaValue> = StarlarkValueAsType::new();
    const Finding: StarlarkValueAsType<FindingValue> = StarlarkValueAsType::new();
}

fn protobuf_enum_globals(globals: &mut GlobalsBuilder) {
    globals.namespace("Severities", |ns| {
        ns.set("error", severity_error());
        ns.set("warning", severity_warning());
        ns.set("info", severity_info());
    });
    globals.namespace("ParserBackends", |ns| {
        ns.set("auto", parser_backend_auto());
        ns.set("protoc", parser_backend_protoc());
        ns.set("pure", parser_backend_pure());
    });
    globals.namespace("FieldLabels", |ns| {
        ns.set("optional", field_label_optional());
        ns.set("required", field_label_required());
        ns.set("repeated", field_label_repeated());
    });
    globals.namespace("FieldKinds", |ns| {
        for (name, value) in field_kind_constants() {
            ns.set(name, value);
        }
    });
    globals.namespace("OptionValueKinds", |ns| {
        ns.set("bool", option_value_kind_bool());
        ns.set("int", option_value_kind_int());
        ns.set("enum", option_value_kind_enum());
        ns.set("float", option_value_kind_float());
        ns.set("string", option_value_kind_string());
        ns.set("bytes", option_value_kind_bytes());
        ns.set("message", option_value_kind_message());
        ns.set("unknown", option_value_kind_unknown());
    });
    globals.namespace("DeltaKinds", |ns| {
        for name in delta_kind_names() {
            ns.set(name, delta_kind_value(name));
        }
    });
}

#[starlark_module]
fn protobuf_helper_globals(globals: &mut GlobalsBuilder) {
    fn finding<'v>(
        severity: starlark::values::Value<'v>,
        message: String,
        path: Option<String>,
        line: Option<i32>,
        column: Option<i32>,
        remediation: Option<String>,
    ) -> anyhow::Result<FindingValue> {
        Ok(build_finding_value(
            parse_severity_param(severity)?,
            message,
            path,
            line,
            column,
            remediation,
        ))
    }

    fn error(
        message: String,
        path: Option<String>,
        remediation: Option<String>,
        line: Option<i32>,
        column: Option<i32>,
    ) -> anyhow::Result<FindingValue> {
        Ok(build_finding_value(
            "error",
            message,
            path,
            line,
            column,
            remediation,
        ))
    }

    fn warning(
        message: String,
        path: Option<String>,
        remediation: Option<String>,
        line: Option<i32>,
        column: Option<i32>,
    ) -> anyhow::Result<FindingValue> {
        Ok(build_finding_value(
            "warning",
            message,
            path,
            line,
            column,
            remediation,
        ))
    }

    fn info(
        message: String,
        path: Option<String>,
        remediation: Option<String>,
        line: Option<i32>,
        column: Option<i32>,
    ) -> anyhow::Result<FindingValue> {
        Ok(build_finding_value(
            "info",
            message,
            path,
            line,
            column,
            remediation,
        ))
    }

    fn filter_deltas<'v>(
        ctx: starlark::values::Value<'v>,
        kind: Option<starlark::values::Value<'v>>,
        symbol_prefix: Option<String>,
        path: Option<String>,
    ) -> anyhow::Result<Vec<SchemaDeltaValue>> {
        let ctx = ProtoContextValue::from_value(ctx)
            .ok_or_else(|| anyhow!("expected `ProtoContext` for `ctx`"))?;
        let kind = parse_delta_kind_param(kind)?;
        Ok(ctx
            .deltas
            .iter()
            .filter(|delta| {
                kind.is_none_or(|expected| delta_kind_value_name(&delta.kind) == expected)
            })
            .filter(|delta| {
                symbol_prefix
                    .as_ref()
                    .is_none_or(|prefix| delta.symbol.starts_with(prefix))
            })
            .filter(|delta| path.as_ref().is_none_or(|expected| &delta.path == expected))
            .cloned()
            .collect())
    }

    fn removed_fields<'v>(ctx: starlark::values::Value<'v>) -> anyhow::Result<Vec<SchemaDeltaValue>> {
        filter_delta_kind(ctx, "field_removed")
    }

    fn changed_field_numbers<'v>(
        ctx: starlark::values::Value<'v>,
    ) -> anyhow::Result<Vec<SchemaDeltaValue>> {
        filter_delta_kind(ctx, "field_number_changed")
    }

    fn removed_messages<'v>(
        ctx: starlark::values::Value<'v>,
    ) -> anyhow::Result<Vec<SchemaDeltaValue>> {
        filter_delta_kind(ctx, "message_removed")
    }

    fn removed_enums<'v>(ctx: starlark::values::Value<'v>) -> anyhow::Result<Vec<SchemaDeltaValue>> {
        filter_delta_kind(ctx, "enum_removed")
    }

    fn option_changed_deltas<'v>(
        ctx: starlark::values::Value<'v>,
    ) -> anyhow::Result<Vec<SchemaDeltaValue>> {
        let ctx = ProtoContextValue::from_value(ctx)
            .ok_or_else(|| anyhow!("expected `ProtoContext` for `ctx`"))?;
        Ok(ctx
            .deltas
            .iter()
            .filter(|delta| delta_kind_value_name(&delta.kind).ends_with("_options_changed"))
            .cloned()
            .collect())
    }

    fn registered_option_deltas<'v>(
        ctx: starlark::values::Value<'v>,
    ) -> anyhow::Result<Vec<SchemaDeltaValue>> {
        let ctx = ProtoContextValue::from_value(ctx)
            .ok_or_else(|| anyhow!("expected `ProtoContext` for `ctx`"))?;
        Ok(ctx
            .deltas
            .iter()
            .filter(|delta| {
                matches!(
                    delta_kind_value_name(&delta.kind),
                    "registered_option_removed" | "registered_option_value_changed"
                )
            })
            .cloned()
            .collect())
    }

    fn option_extensions<'v>(
        options: starlark::values::Value<'v>,
        full_name: Option<String>,
    ) -> anyhow::Result<Vec<OptionExtensionValue>> {
        option_extensions_impl(options, full_name)
    }

    fn has_option<'v>(
        options: starlark::values::Value<'v>,
        full_name: String,
    ) -> anyhow::Result<bool> {
        Ok(!option_extensions_impl(options, Some(full_name))?.is_empty())
    }

    fn bool_option<'v>(
        options: starlark::values::Value<'v>,
        full_name: String,
    ) -> anyhow::Result<OptionalAttr<bool>> {
        let mut matches = option_extensions_impl(options, Some(full_name))?;
        let Some(extension) = matches.pop() else {
            return Ok(OptionalAttr::from(None));
        };
        Ok(OptionalAttr::from(
            extension
            .values
            .iter()
            .find_map(|value| value.bool_value.0),
        ))
    }

    fn option_field_values<'v>(
        options: starlark::values::Value<'v>,
        full_name: String,
        field_path: String,
    ) -> anyhow::Result<Vec<OptionValueValue>> {
        option_field_values_impl(options, full_name, field_path)
    }

    fn bool_option_field<'v>(
        options: starlark::values::Value<'v>,
        full_name: String,
        field_path: String,
    ) -> anyhow::Result<OptionalAttr<bool>> {
        let values = option_field_values_impl(options, full_name, field_path)?;
        Ok(OptionalAttr::from(
            values.into_iter().find_map(|value| value.bool_value.0),
        ))
    }

    fn option_descendants<'v>(
        value: starlark::values::Value<'v>,
    ) -> anyhow::Result<Vec<OptionFieldValue>> {
        if let Some(extension) = OptionExtensionValue::from_value(value) {
            return Ok(flatten_option_fields(
                extension
                    .values
                    .iter()
                    .flat_map(|option_value| option_value.message_fields.iter().cloned())
                    .collect(),
            ));
        }
        if let Some(option_value) = OptionValueValue::from_value(value) {
            return Ok(flatten_option_fields(option_value.message_fields.clone()));
        }
        Err(anyhow!("expected `OptionExtension` or `OptionValue`"))
    }

    fn finding_for_delta<'v>(
        ctx: starlark::values::Value<'v>,
        delta: starlark::values::Value<'v>,
        message: String,
        severity: Option<starlark::values::Value<'v>>,
        remediation: Option<String>,
    ) -> anyhow::Result<FindingValue> {
        let ctx = ProtoContextValue::from_value(ctx)
            .ok_or_else(|| anyhow!("expected `ProtoContext` for `ctx`"))?;
        let delta = SchemaDeltaValue::from_value(delta)
            .ok_or_else(|| anyhow!("expected `SchemaDelta` for `delta`"))?;
        Ok(build_finding_value(
            severity
                .map(parse_severity_param)
                .transpose()?
                .unwrap_or_else(|| severity_value_name(&ctx.config.default_severity)),
            message,
            Some(delta.path.clone()),
            None,
            None,
            Some(remediation.unwrap_or_else(|| ctx.config.default_remediation.clone())),
        ))
    }
}

fn option_extensions_impl<'v>(
    options: starlark::values::Value<'v>,
    full_name: Option<String>,
) -> anyhow::Result<Vec<OptionExtensionValue>> {
    let options = DescriptorOptionsValue::from_value(options)
        .ok_or_else(|| anyhow!("expected `DescriptorOptions`"))?;
    Ok(options
        .extensions
        .iter()
        .filter(|extension| {
            full_name
                .as_ref()
                .is_none_or(|expected| &extension.full_name == expected)
        })
        .cloned()
        .collect())
}

fn option_field_values_impl<'v>(
    options: starlark::values::Value<'v>,
    full_name: String,
    field_path: String,
) -> anyhow::Result<Vec<OptionValueValue>> {
    let extensions = option_extensions_impl(options, Some(full_name))?;
    let segments = field_path
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let mut values = extensions
        .into_iter()
        .flat_map(|extension| extension.values.into_iter())
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return Ok(values);
    }
    for segment in segments {
        let mut next = Vec::new();
        for value in values {
            for field in &value.message_fields {
                if field.name == segment {
                    next.extend(field.values.iter().cloned());
                }
            }
        }
        values = next;
    }
    Ok(values)
}

fn flatten_option_fields(mut roots: Vec<OptionFieldValue>) -> Vec<OptionFieldValue> {
    let mut flattened = Vec::new();
    while let Some(field) = roots.pop() {
        for value in field.values.iter().rev() {
            for child in value.message_fields.iter().rev() {
                roots.push(child.clone());
            }
        }
        flattened.push(field);
    }
    flattened.reverse();
    flattened
}

fn filter_delta_kind<'v>(
    ctx: starlark::values::Value<'v>,
    kind: &str,
) -> anyhow::Result<Vec<SchemaDeltaValue>> {
    let ctx = ProtoContextValue::from_value(ctx)
        .ok_or_else(|| anyhow!("expected `ProtoContext` for `ctx`"))?;
    Ok(ctx
        .deltas
        .iter()
        .filter(|delta| delta_kind_value_name(&delta.kind) == kind)
        .cloned()
        .collect())
}

fn details_string(
    details: &BTreeMap<String, serde_json::Value>,
    key: &str,
) -> Option<String> {
    details.get(key).and_then(|value| value.as_str().map(ToOwned::to_owned))
}

fn details_i32(details: &BTreeMap<String, serde_json::Value>, key: &str) -> Option<i32> {
    details
        .get(key)
        .and_then(serde_json::Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
}

fn details_bool(details: &BTreeMap<String, serde_json::Value>, key: &str) -> Option<bool> {
    details.get(key).and_then(serde_json::Value::as_bool)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::hint::black_box;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    use anyhow::anyhow;
    use starlark::environment::Module;
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect, DialectTypes};
    use starlark::values::OwnedFrozenValue;
    use tempfile::tempdir;

    use crate::check::Check;
    use crate::input::{ChangeKind, ChangeSet, ChangedFile};
    use crate::output::{Finding, Location, Severity};
    use crate::source_tree::LocalSourceTree;
    use crate::vcs::BaseRevision;

    use super::ProtobufEvolutionCheck;

    #[tokio::test]
    async fn flags_removed_field_between_base_and_current() {
        let temp = tempdir().expect("create temp dir");
        init_git_repo(temp.path());
        write_proto(
            temp.path(),
            "proto/example.proto",
            r#"
syntax = "proto3";
package example;

message User {
  string id = 1;
  string name = 2;
}
"#,
        );
        git_commit_all(temp.path(), "base");

        write_proto(
            temp.path(),
            "proto/example.proto",
            r#"
syntax = "proto3";
package example;

message User {
  string id = 1;
}
"#,
        );

        let tree = LocalSourceTree::with_base_revision(
            temp.path(),
            Some(BaseRevision::Git("HEAD".to_owned())),
        )
        .expect("create tree");
        let check = ProtobufEvolutionCheck;
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("proto/example.proto").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(Default::default()),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 1);
        assert!(result.findings[0].message.contains("field `example.User.name` was removed"));
    }

    #[tokio::test]
    async fn exposes_before_after_and_deltas_to_starlark() {
        let temp = tempdir().expect("create temp dir");
        init_git_repo(temp.path());
        write_proto(
            temp.path(),
            "proto/example.proto",
            r#"
syntax = "proto3";
package example;

message User {
  string id = 1;
  string name = 2;
}
"#,
        );
        git_commit_all(temp.path(), "base");

        write_proto(
            temp.path(),
            "proto/example.proto",
            r#"
syntax = "proto3";
package example;

message User {
  string id = 1;
}
"#,
        );
        fs::write(
            temp.path().join("proto_rules.star"),
            r#"
def check(ctx: ProtoContext) -> list[Finding]:
    findings = []
    if ctx.files[0].before.messages[0].fields[1].name == "name":
        findings.append(warning(
            message = "starlark saw the removed field",
            path = ctx.files[0].path,
        ))
    if ctx.deltas[0].kind == DeltaKinds.field_removed:
        findings.append(info(
            message = "starlark saw the computed delta",
            path = ctx.deltas[0].path,
        ))
    return findings
"#,
        )
        .expect("write starlark policy");

        let tree = LocalSourceTree::with_base_revision(
            temp.path(),
            Some(BaseRevision::Git("HEAD".to_owned())),
        )
        .expect("create tree");
        let check = ProtobufEvolutionCheck;
        let result = check
            .run(
                &ChangeSet::new(vec![ChangedFile {
                    path: Path::new("proto/example.proto").to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                }]),
                &tree,
                &toml::Value::Table(toml::toml! {
                    starlark_path = "proto_rules.star"
                }),
            )
            .await
            .expect("run check");

        assert_eq!(result.findings.len(), 3);
        assert!(result.findings.iter().any(|finding| finding.message == "starlark saw the removed field"));
        assert!(result.findings.iter().any(|finding| finding.message == "starlark saw the computed delta"));
    }

    #[test]
    fn helper_globals_and_typed_return_contract_work() {
        let findings = super::run_starlark_source(
            r#"
def check(ctx: ProtoContext) -> list[Finding]:
    removed = removed_fields(ctx)
    changed = changed_field_numbers(ctx)
    filtered = filter_deltas(ctx, kind = "field_removed", symbol_prefix = "example.")
    option_deltas = registered_option_deltas(ctx)
    flag = bool_option(ctx.files[0].before.options, "acme.flag")
    owner = option_field_values(ctx.files[0].before.options, "acme.policy", "nested.owner")
    descendants = option_descendants(option_extensions(ctx.files[0].before.options, "acme.policy")[0])
    enabled = bool_option_field(ctx.files[0].before.options, "acme.policy", "enabled")
    findings = []
    if len(removed) == 1 and len(changed) == 1 and len(filtered) == 1 and len(option_deltas) == 1 and flag and enabled and owner[0].string_value == "ops" and len(descendants) >= 3:
        findings.append(finding_for_delta(ctx, removed[0], "helper saw removed field"))
    return findings
"#,
            "<test>",
            &sample_context(),
        )
        .expect("run starlark");

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].message, "helper saw removed field");
    }

    #[test]
    fn rejects_unknown_annotation_type() {
        let error = super::run_starlark_source(
            r#"
def check(ctx: MissingContext) -> list[Finding]:
    return []
"#,
            "<test>",
            &sample_context(),
        )
        .expect_err("missing annotation type should fail");

        assert!(error.to_string().contains("MissingContext"));
    }

    #[test]
    fn rejects_non_finding_returns() {
        let error = super::run_starlark_source(
            r#"
def check(ctx: ProtoContext) -> list[Finding]:
    return ["nope"]
"#,
            "<test>",
            &sample_context(),
        )
        .expect_err("wrong return type should fail");

        assert!(error.to_string().contains("list[Finding]"));
    }

    #[test]
    fn captures_broader_proto_deltas() {
        let context = sample_context();
        let delta_kinds = context
            .deltas
            .iter()
            .map(|delta| delta.kind.as_str())
            .collect::<BTreeSet<_>>();

        assert!(delta_kinds.contains("field_removed"));
        assert!(delta_kinds.contains("field_number_changed"));
        assert!(delta_kinds.contains("message_reserved_name_removed"));
        assert!(delta_kinds.contains("oneof_removed"));
        assert!(delta_kinds.contains("service_removed"));
        assert!(delta_kinds.contains("method_signature_changed"));
        assert!(delta_kinds.contains("package_changed"));
        assert!(delta_kinds.contains("syntax_changed"));
        assert!(delta_kinds.contains("message_options_changed"));
        assert!(delta_kinds.contains("registered_option_value_changed"));
    }

    #[test]
    fn decodes_registered_option_extensions() {
        let mut options = protobuf::descriptor::FieldOptions::new();
        options
            .special_fields
            .mut_unknown_fields()
            .add_varint(51001, 1);

        let registry = super::ExtensionRegistrySet {
            infos: vec![super::ExtensionRegistryInfo {
                name: "acme".to_owned(),
                extension_count: 1,
                files: vec!["proto/extensions/options.proto".to_owned()],
                extendees: vec![".google.protobuf.FieldOptions".to_owned()],
            }],
            by_extendee: BTreeMap::from([(
                ".google.protobuf.FieldOptions".to_owned(),
                BTreeMap::from([(
                    51001,
                    super::RegisteredExtension {
                        registry_name: "acme".to_owned(),
                        source_path: "proto/extensions/options.proto".to_owned(),
                        full_name: "acme.sensitive".to_owned(),
                        extendee: ".google.protobuf.FieldOptions".to_owned(),
                        field_number: 51001,
                        kind: "bool".to_owned(),
                        type_name: None,
                        is_repeated: false,
                    },
                )]),
            )]),
            message_types: BTreeMap::new(),
            enum_types: BTreeMap::new(),
        };

        let decoded = super::descriptor_options_schema(
            Some(&options),
            ".google.protobuf.FieldOptions",
            &registry,
        );

        assert_eq!(decoded.extensions.len(), 1);
        assert_eq!(decoded.extensions[0].registry_name, "acme");
        assert_eq!(decoded.extensions[0].full_name, "acme.sensitive");
        assert_eq!(decoded.extensions[0].values.len(), 1);
        assert_eq!(decoded.extensions[0].values[0].kind, "bool");
        assert_eq!(decoded.extensions[0].values[0].bool_value, Some(true));
        assert_eq!(decoded.extensions[0].values[0].raw_repr, "true");
        assert!(decoded.extensions[0].decoded);
    }

    #[test]
    fn decodes_message_valued_option_extensions() {
        let mut options = protobuf::descriptor::FieldOptions::new();
        options.special_fields.mut_unknown_fields().add_length_delimited(
            51002,
            vec![
                0x08, 0x01, // enabled = true
                0x12, 0x05, 0x0A, 0x03, b'o', b'p', b's', // nested.owner = "ops"
                0x1A, 0x02, 0x07, 0x09, // ids = [7, 9] packed
                0x20, 0x02, // mode = ACTIVE
            ],
        );

        let registry = super::ExtensionRegistrySet {
            infos: vec![super::ExtensionRegistryInfo {
                name: "acme".to_owned(),
                extension_count: 1,
                files: vec!["proto/extensions/options.proto".to_owned()],
                extendees: vec![".google.protobuf.FieldOptions".to_owned()],
            }],
            by_extendee: BTreeMap::from([(
                ".google.protobuf.FieldOptions".to_owned(),
                BTreeMap::from([(
                    51002,
                    super::RegisteredExtension {
                        registry_name: "acme".to_owned(),
                        source_path: "proto/extensions/options.proto".to_owned(),
                        full_name: "acme.policy".to_owned(),
                        extendee: ".google.protobuf.FieldOptions".to_owned(),
                        field_number: 51002,
                        kind: "message".to_owned(),
                        type_name: Some(".acme.Policy".to_owned()),
                        is_repeated: false,
                    },
                )]),
            )]),
            message_types: BTreeMap::from([
                (
                    "acme.Policy".to_owned(),
                    super::RegisteredMessageType {
                        fields_by_number: BTreeMap::from([
                            (
                                1,
                                super::RegisteredMessageField {
                                    name: "enabled".to_owned(),
                                    full_name: "acme.Policy.enabled".to_owned(),
                                    number: 1,
                                    kind: "bool".to_owned(),
                                    type_name: None,
                                    is_repeated: false,
                                },
                            ),
                            (
                                2,
                                super::RegisteredMessageField {
                                    name: "nested".to_owned(),
                                    full_name: "acme.Policy.nested".to_owned(),
                                    number: 2,
                                    kind: "message".to_owned(),
                                    type_name: Some(".acme.Nested".to_owned()),
                                    is_repeated: false,
                                },
                            ),
                            (
                                3,
                                super::RegisteredMessageField {
                                    name: "ids".to_owned(),
                                    full_name: "acme.Policy.ids".to_owned(),
                                    number: 3,
                                    kind: "int32".to_owned(),
                                    type_name: None,
                                    is_repeated: true,
                                },
                            ),
                            (
                                4,
                                super::RegisteredMessageField {
                                    name: "mode".to_owned(),
                                    full_name: "acme.Policy.mode".to_owned(),
                                    number: 4,
                                    kind: "enum".to_owned(),
                                    type_name: Some(".acme.Mode".to_owned()),
                                    is_repeated: false,
                                },
                            ),
                        ]),
                    },
                ),
                (
                    "acme.Nested".to_owned(),
                    super::RegisteredMessageType {
                        fields_by_number: BTreeMap::from([(
                            1,
                            super::RegisteredMessageField {
                                name: "owner".to_owned(),
                                full_name: "acme.Nested.owner".to_owned(),
                                number: 1,
                                kind: "string".to_owned(),
                                type_name: None,
                                is_repeated: false,
                            },
                        )]),
                    },
                ),
            ]),
            enum_types: BTreeMap::from([(
                "acme.Mode".to_owned(),
                super::RegisteredEnumType {
                    values_by_number: BTreeMap::from([(1, "UNKNOWN".to_owned()), (2, "ACTIVE".to_owned())]),
                },
            )]),
        };

        let decoded = super::descriptor_options_schema(
            Some(&options),
            ".google.protobuf.FieldOptions",
            &registry,
        );

        let value = &decoded.extensions[0].values[0];
        assert_eq!(value.kind, "message");
        assert!(value.decoded);
        assert_eq!(value.message_fields.len(), 4);
        assert_eq!(value.message_fields[0].name, "enabled");
        assert_eq!(value.message_fields[0].values[0].bool_value, Some(true));
        assert_eq!(value.message_fields[1].name, "nested");
        assert_eq!(
            value.message_fields[1].values[0].message_fields[0].values[0].string_value,
            Some("ops".to_owned())
        );
        assert_eq!(value.message_fields[2].values.len(), 2);
        assert_eq!(value.message_fields[2].values[0].int_value, Some(7));
        assert_eq!(value.message_fields[2].values[1].int_value, Some(9));
        assert_eq!(value.message_fields[3].values[0].kind, "enum");
        assert_eq!(value.message_fields[3].values[0].enum_name, Some("ACTIVE".to_owned()));
    }

    #[tokio::test]
    #[ignore = "stress benchmark for manual protobuf evolution profiling"]
    async fn protobuf_evolution_perf_stress_e2e() {
        let file_count = env_usize("CHECKLEFT_PROTO_PERF_FILES", 24);
        let messages_per_file = env_usize("CHECKLEFT_PROTO_PERF_MESSAGES", 10);
        let fields_per_message = env_usize("CHECKLEFT_PROTO_PERF_FIELDS", 24);
        let policy_samples = env_usize("CHECKLEFT_PROTO_PERF_POLICY_SAMPLES", 5);
        let e2e_samples = env_usize("CHECKLEFT_PROTO_PERF_E2E_SAMPLES", 3);

        let policy_context =
            large_perf_context(file_count, messages_per_file, fields_per_message);
        let starlark_findings = super::run_starlark_source(
            super::PERF_POLICY_SOURCE,
            "<perf>",
            &policy_context,
        )
        .expect("run starlark perf policy");
        let rust_findings = run_rust_perf_policy(&policy_context);
        assert_eq!(starlark_findings.len(), rust_findings.len());

        let starlark_policy = measure_sync(policy_samples, || {
            super::run_starlark_source(super::PERF_POLICY_SOURCE, "<perf>", &policy_context)
                .expect("run starlark perf policy")
                .len()
        });
        let starlark_parse_only = measure_sync(policy_samples, || {
            parse_perf_policy(super::PERF_POLICY_SOURCE);
            1
        });
        let frozen_check = compile_perf_policy_check(super::PERF_POLICY_SOURCE);
        let starlark_call_only = measure_sync(policy_samples, || {
            call_frozen_perf_policy(&frozen_check, &policy_context)
                .expect("run frozen starlark perf policy")
                .len()
        });
        let rust_policy = measure_sync(policy_samples, || {
            run_rust_perf_policy(&policy_context).len()
        });

        let temp = tempdir().expect("create temp dir");
        init_git_repo(temp.path());
        fs::write(temp.path().join("proto_rules.star"), super::PERF_POLICY_SOURCE)
            .expect("write perf starlark");
        for file_index in 0..file_count {
            write_proto(
                temp.path(),
                &format!("proto/perf_{file_index:03}.proto"),
                &stress_proto_file(
                    file_index,
                    messages_per_file,
                    fields_per_message,
                    true,
                ),
            );
        }
        git_commit_all(temp.path(), "base");
        for file_index in 0..file_count {
            write_proto(
                temp.path(),
                &format!("proto/perf_{file_index:03}.proto"),
                &stress_proto_file(
                    file_index,
                    messages_per_file,
                    fields_per_message,
                    false,
                ),
            );
        }

        let tree = LocalSourceTree::with_base_revision(
            temp.path(),
            Some(BaseRevision::Git("HEAD".to_owned())),
        )
        .expect("create tree");
        let check = ProtobufEvolutionCheck;
        let configured = check
            .configure(&toml::Value::Table(toml::toml! {
                parser_backend = "pure"
                starlark_path = "proto_rules.star"
            }))
            .expect("configure check");
        let changeset = ChangeSet::new(
            (0..file_count)
                .map(|file_index| ChangedFile {
                    path: Path::new(&format!("proto/perf_{file_index:03}.proto")).to_path_buf(),
                    kind: ChangeKind::Modified,
                    old_path: None,
                })
                .collect(),
        );

        let e2e_result = configured
            .run(&changeset, &tree)
            .await
            .expect("run e2e perf check");
        assert!(!e2e_result.findings.is_empty());

        let mut e2e_durations = Vec::with_capacity(e2e_samples);
        let mut e2e_sink = 0usize;
        for _ in 0..e2e_samples {
            let started_at = Instant::now();
            let result = configured
                .run(&changeset, &tree)
                .await
                .expect("run e2e perf check");
            e2e_sink ^= black_box(result.findings.len());
            e2e_durations.push(started_at.elapsed());
        }
        e2e_durations.sort_unstable();
        black_box(e2e_sink);
        let e2e_starlark = e2e_durations[e2e_durations.len() / 2];

        println!(
            "protobuf perf workload: files={file_count} messages_per_file={messages_per_file} fields_per_message={fields_per_message}"
        );
        println!(
            "policy findings: starlark={} rust={} e2e={}",
            starlark_findings.len(),
            rust_findings.len(),
            e2e_result.findings.len()
        );
        println!(
            "{:<24} {:>12}",
            "starlark-parse",
            format_duration(starlark_parse_only)
        );
        println!(
            "{:<24} {:>12}",
            "starlark-policy",
            format_duration(starlark_policy)
        );
        println!(
            "{:<24} {:>12}",
            "starlark-call-only",
            format_duration(starlark_call_only)
        );
        println!(
            "{:<24} {:>12}",
            "rust-policy",
            format_duration(rust_policy)
        );
        println!(
            "{:<24} {:>12}",
            "e2e-starlark",
            format_duration(e2e_starlark)
        );
        println!(
            "{:<24} {:>11.2}x",
            "policy-overhead",
            starlark_policy.as_secs_f64() / rust_policy.as_secs_f64()
        );
        println!(
            "{:<24} {:>11.2}x",
            "call-overhead",
            starlark_call_only.as_secs_f64() / rust_policy.as_secs_f64()
        );
    }

    fn init_git_repo(root: &Path) {
        run(root, &["git", "init"]);
        run(root, &["git", "config", "user.email", "checkleft@example.com"]);
        run(root, &["git", "config", "user.name", "Checkleft Tests"]);
    }

    fn git_commit_all(root: &Path, message: &str) {
        run(root, &["git", "add", "."]);
        run(root, &["git", "commit", "-m", message]);
    }

    fn write_proto(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("create proto dir");
        fs::write(path, contents.trim_start()).expect("write proto");
    }

    fn run(root: &Path, command: &[&str]) {
        let output = Command::new(command[0])
            .args(&command[1..])
            .current_dir(root)
            .output()
            .expect("run command");
        assert!(
            output.status.success(),
            "{} failed: {}",
            command.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn sample_context() -> super::StarlarkProtoContext {
        let before = super::FlatFileSchema {
            file: super::FileSchema {
                path: "proto/example.proto".to_owned(),
                package: "example.v1".to_owned(),
                syntax: "proto2".to_owned(),
                options: super::DescriptorOptionsSchema {
                    fingerprint: "aa".to_owned(),
                    has_unknown_fields: false,
                    uninterpreted: Vec::new(),
                    extensions: vec![
                        bool_option_extension("acme.flag", "true"),
                        policy_option_extension("true"),
                    ],
                },
                messages: vec![super::MessageSchema {
                    full_name: "example.v1.User".to_owned(),
                    name: "User".to_owned(),
                    options: super::DescriptorOptionsSchema {
                        fingerprint: "bb".to_owned(),
                        has_unknown_fields: false,
                        uninterpreted: Vec::new(),
                        extensions: Vec::new(),
                    },
                    is_map_entry: false,
                    fields: vec![
                        field("example.v1.User.id", "id", 1, None, "cc"),
                        field("example.v1.User.name", "name", 2, Some("choice"), "dd"),
                    ],
                    oneofs: vec![oneof("example.v1.User.choice", "choice", "ee")],
                    extensions: Vec::new(),
                    reserved_ranges: vec![super::ReservedRangeSchema { start: 10, end: 20 }],
                    reserved_names: vec!["legacy_name".to_owned()],
                    nested_messages: Vec::new(),
                    nested_enums: Vec::new(),
                }],
                enums: vec![super::EnumSchema {
                    full_name: "example.v1.State".to_owned(),
                    name: "State".to_owned(),
                    options: super::DescriptorOptionsSchema::default(),
                    reserved_ranges: vec![super::ReservedRangeSchema { start: 4, end: 8 }],
                    reserved_names: vec!["OLD".to_owned()],
                    values: vec![super::EnumValueSchema {
                        full_name: "example.v1.State.ACTIVE".to_owned(),
                        name: "ACTIVE".to_owned(),
                        number: 1,
                        options: super::DescriptorOptionsSchema::default(),
                    }],
                }],
                services: vec![super::ServiceSchema {
                    full_name: "example.v1.Users".to_owned(),
                    name: "Users".to_owned(),
                    options: super::DescriptorOptionsSchema::default(),
                    methods: vec![super::MethodSchema {
                        full_name: "example.v1.Users.Get".to_owned(),
                        name: "Get".to_owned(),
                        input_type: ".example.v1.GetUserRequest".to_owned(),
                        output_type: ".example.v1.User".to_owned(),
                        client_streaming: false,
                        server_streaming: false,
                        options: super::DescriptorOptionsSchema::default(),
                    }],
                },
                super::ServiceSchema {
                    full_name: "example.v1.LegacyUsers".to_owned(),
                    name: "LegacyUsers".to_owned(),
                    options: super::DescriptorOptionsSchema::default(),
                    methods: vec![super::MethodSchema {
                        full_name: "example.v1.LegacyUsers.Delete".to_owned(),
                        name: "Delete".to_owned(),
                        input_type: ".example.v1.DeleteUserRequest".to_owned(),
                        output_type: ".example.v1.DeleteUserResponse".to_owned(),
                        client_streaming: false,
                        server_streaming: false,
                        options: super::DescriptorOptionsSchema::default(),
                    }],
                }],
                extensions: Vec::new(),
            },
            messages: BTreeMap::from([(
                "example.v1.User".to_owned(),
                super::FlatMessageSchema {
                    full_name: "example.v1.User".to_owned(),
                    fields_by_number: BTreeMap::from([
                        (1, field("example.v1.User.id", "id", 1, None, "cc")),
                        (2, field("example.v1.User.name", "name", 2, Some("choice"), "dd")),
                    ]),
                    fields_by_name: BTreeMap::from([
                        ("id".to_owned(), field("example.v1.User.id", "id", 1, None, "cc")),
                        (
                            "name".to_owned(),
                            field("example.v1.User.name", "name", 2, Some("choice"), "dd"),
                        ),
                    ]),
                    oneofs_by_name: BTreeMap::from([(
                        "choice".to_owned(),
                        oneof("example.v1.User.choice", "choice", "ee"),
                    )]),
                    extensions_by_number: BTreeMap::new(),
                    extensions_by_name: BTreeMap::new(),
                    reserved_ranges: BTreeSet::from([super::ReservedRangeSchema {
                        start: 10,
                        end: 20,
                    }]),
                    reserved_names: BTreeSet::from(["legacy_name".to_owned()]),
                    options: super::DescriptorOptionsSchema {
                        fingerprint: "bb".to_owned(),
                        has_unknown_fields: false,
                        uninterpreted: Vec::new(),
                        extensions: Vec::new(),
                    },
                    is_map_entry: false,
                },
            )]),
            enums: BTreeMap::from([(
                "example.v1.State".to_owned(),
                super::FlatEnumSchema {
                    full_name: "example.v1.State".to_owned(),
                    values_by_number: BTreeMap::from([(
                        1,
                        super::EnumValueSchema {
                            full_name: "example.v1.State.ACTIVE".to_owned(),
                            name: "ACTIVE".to_owned(),
                            number: 1,
                            options: super::DescriptorOptionsSchema::default(),
                        },
                    )]),
                    values_by_name: BTreeMap::from([(
                        "ACTIVE".to_owned(),
                        super::EnumValueSchema {
                            full_name: "example.v1.State.ACTIVE".to_owned(),
                            name: "ACTIVE".to_owned(),
                            number: 1,
                            options: super::DescriptorOptionsSchema::default(),
                        },
                    )]),
                    reserved_ranges: BTreeSet::from([super::ReservedRangeSchema {
                        start: 4,
                        end: 8,
                    }]),
                    reserved_names: BTreeSet::from(["OLD".to_owned()]),
                    options: super::DescriptorOptionsSchema::default(),
                },
            )]),
            services: BTreeMap::from([(
                "example.v1.Users".to_owned(),
                super::ServiceSchema {
                    full_name: "example.v1.Users".to_owned(),
                    name: "Users".to_owned(),
                    options: super::DescriptorOptionsSchema::default(),
                    methods: vec![super::MethodSchema {
                        full_name: "example.v1.Users.Get".to_owned(),
                        name: "Get".to_owned(),
                        input_type: ".example.v1.GetUserRequest".to_owned(),
                        output_type: ".example.v1.User".to_owned(),
                        client_streaming: false,
                        server_streaming: false,
                        options: super::DescriptorOptionsSchema::default(),
                    }],
                },
            ),
            (
                "example.v1.LegacyUsers".to_owned(),
                super::ServiceSchema {
                    full_name: "example.v1.LegacyUsers".to_owned(),
                    name: "LegacyUsers".to_owned(),
                    options: super::DescriptorOptionsSchema::default(),
                    methods: vec![super::MethodSchema {
                        full_name: "example.v1.LegacyUsers.Delete".to_owned(),
                        name: "Delete".to_owned(),
                        input_type: ".example.v1.DeleteUserRequest".to_owned(),
                        output_type: ".example.v1.DeleteUserResponse".to_owned(),
                        client_streaming: false,
                        server_streaming: false,
                        options: super::DescriptorOptionsSchema::default(),
                    }],
                },
            )]),
            extensions_by_number: BTreeMap::new(),
            extensions_by_name: BTreeMap::new(),
        };
        let after = super::FlatFileSchema {
            file: super::FileSchema {
                path: "proto/example.proto".to_owned(),
                package: "example.v2".to_owned(),
                syntax: "proto3".to_owned(),
                options: super::DescriptorOptionsSchema {
                    fingerprint: "ab".to_owned(),
                    has_unknown_fields: true,
                    uninterpreted: Vec::new(),
                    extensions: vec![
                        bool_option_extension("acme.flag", "false"),
                        policy_option_extension("false"),
                    ],
                },
                messages: vec![super::MessageSchema {
                    full_name: "example.v1.User".to_owned(),
                    name: "User".to_owned(),
                    options: super::DescriptorOptionsSchema {
                        fingerprint: "bc".to_owned(),
                        has_unknown_fields: false,
                        uninterpreted: Vec::new(),
                        extensions: Vec::new(),
                    },
                    is_map_entry: true,
                    fields: vec![
                        field("example.v1.User.id", "id", 9, None, "cc"),
                        field("example.v1.User.extra", "extra", 3, None, "ff"),
                    ],
                    oneofs: Vec::new(),
                    extensions: Vec::new(),
                    reserved_ranges: Vec::new(),
                    reserved_names: Vec::new(),
                    nested_messages: Vec::new(),
                    nested_enums: Vec::new(),
                }],
                enums: vec![super::EnumSchema {
                    full_name: "example.v1.State".to_owned(),
                    name: "State".to_owned(),
                    options: super::DescriptorOptionsSchema::default(),
                    reserved_ranges: Vec::new(),
                    reserved_names: Vec::new(),
                    values: vec![super::EnumValueSchema {
                        full_name: "example.v1.State.ACTIVE".to_owned(),
                        name: "ACTIVE".to_owned(),
                        number: 1,
                        options: super::DescriptorOptionsSchema::default(),
                    }],
                }],
                services: vec![super::ServiceSchema {
                    full_name: "example.v1.Users".to_owned(),
                    name: "Users".to_owned(),
                    options: super::DescriptorOptionsSchema::default(),
                    methods: vec![super::MethodSchema {
                        full_name: "example.v1.Users.Get".to_owned(),
                        name: "Get".to_owned(),
                        input_type: ".example.v1.GetUserRequest".to_owned(),
                        output_type: ".example.v2.User".to_owned(),
                        client_streaming: false,
                        server_streaming: true,
                        options: super::DescriptorOptionsSchema::default(),
                    }],
                }],
                extensions: Vec::new(),
            },
            messages: BTreeMap::from([(
                "example.v1.User".to_owned(),
                super::FlatMessageSchema {
                    full_name: "example.v1.User".to_owned(),
                    fields_by_number: BTreeMap::from([
                        (9, field("example.v1.User.id", "id", 9, None, "cc")),
                        (3, field("example.v1.User.extra", "extra", 3, None, "ff")),
                    ]),
                    fields_by_name: BTreeMap::from([
                        ("id".to_owned(), field("example.v1.User.id", "id", 9, None, "cc")),
                        (
                            "extra".to_owned(),
                            field("example.v1.User.extra", "extra", 3, None, "ff"),
                        ),
                    ]),
                    oneofs_by_name: BTreeMap::new(),
                    extensions_by_number: BTreeMap::new(),
                    extensions_by_name: BTreeMap::new(),
                    reserved_ranges: BTreeSet::new(),
                    reserved_names: BTreeSet::new(),
                    options: super::DescriptorOptionsSchema {
                        fingerprint: "bc".to_owned(),
                        has_unknown_fields: false,
                        uninterpreted: Vec::new(),
                        extensions: Vec::new(),
                    },
                    is_map_entry: true,
                },
            )]),
            enums: BTreeMap::from([(
                "example.v1.State".to_owned(),
                super::FlatEnumSchema {
                    full_name: "example.v1.State".to_owned(),
                    values_by_number: BTreeMap::from([(
                        1,
                        super::EnumValueSchema {
                            full_name: "example.v1.State.ACTIVE".to_owned(),
                            name: "ACTIVE".to_owned(),
                            number: 1,
                            options: super::DescriptorOptionsSchema::default(),
                        },
                    )]),
                    values_by_name: BTreeMap::from([(
                        "ACTIVE".to_owned(),
                        super::EnumValueSchema {
                            full_name: "example.v1.State.ACTIVE".to_owned(),
                            name: "ACTIVE".to_owned(),
                            number: 1,
                            options: super::DescriptorOptionsSchema::default(),
                        },
                    )]),
                    reserved_ranges: BTreeSet::new(),
                    reserved_names: BTreeSet::new(),
                    options: super::DescriptorOptionsSchema::default(),
                },
            )]),
            services: BTreeMap::from([(
                "example.v1.Users".to_owned(),
                super::ServiceSchema {
                    full_name: "example.v1.Users".to_owned(),
                    name: "Users".to_owned(),
                    options: super::DescriptorOptionsSchema::default(),
                    methods: vec![super::MethodSchema {
                        full_name: "example.v1.Users.Get".to_owned(),
                        name: "Get".to_owned(),
                        input_type: ".example.v1.GetUserRequest".to_owned(),
                        output_type: ".example.v2.User".to_owned(),
                        client_streaming: false,
                        server_streaming: true,
                        options: super::DescriptorOptionsSchema::default(),
                    }],
                },
            )]),
            extensions_by_number: BTreeMap::new(),
            extensions_by_name: BTreeMap::new(),
        };

        super::build_context(
            &[super::ChangedProtoFile {
                current_path: Some(Path::new("proto/example.proto").to_path_buf()),
                base_path: Some(Path::new("proto/example.proto").to_path_buf()),
                kind: ChangeKind::Modified,
            }],
            &BTreeMap::from([("proto/example.proto".to_owned(), before)]),
            &BTreeMap::from([("proto/example.proto".to_owned(), after)]),
            &[],
            super::ParserBackend::Pure,
            super::ParserBackend::Protoc,
            super::Severity::Error,
        )
    }

    fn field(
        full_name: &str,
        name: &str,
        number: i32,
        oneof_name: Option<&str>,
        fingerprint: &str,
    ) -> super::FieldSchema {
        super::FieldSchema {
            full_name: full_name.to_owned(),
            name: name.to_owned(),
            number,
            label: "optional".to_owned(),
            kind: "string".to_owned(),
            type_name: None,
            json_name: None,
            oneof_index: oneof_name.map(|_| 0),
            oneof_name: oneof_name.map(ToOwned::to_owned),
            proto3_optional: false,
            extendee: None,
            options: super::DescriptorOptionsSchema {
                fingerprint: fingerprint.to_owned(),
                has_unknown_fields: false,
                uninterpreted: Vec::new(),
                extensions: Vec::new(),
            },
        }
    }

    fn oneof(full_name: &str, name: &str, fingerprint: &str) -> super::OneofSchema {
        super::OneofSchema {
            full_name: full_name.to_owned(),
            name: name.to_owned(),
            options: super::DescriptorOptionsSchema {
                fingerprint: fingerprint.to_owned(),
                has_unknown_fields: false,
                uninterpreted: Vec::new(),
                extensions: Vec::new(),
            },
        }
    }

    fn bool_option_extension(full_name: &str, value: &str) -> super::OptionExtensionSchema {
        super::OptionExtensionSchema {
            registry_name: "acme".to_owned(),
            full_name: full_name.to_owned(),
            extendee: ".google.protobuf.FileOptions".to_owned(),
            field_number: 51001,
            kind: "bool".to_owned(),
            type_name: None,
            is_repeated: false,
            values: vec![super::OptionValueSchema {
                kind: "bool".to_owned(),
                bool_value: Some(value == "true"),
                int_value: None,
                float_value: None,
                enum_name: None,
                string_value: None,
                bytes_hex: None,
                message_hex: None,
                message_fields: Vec::new(),
                raw_repr: value.to_owned(),
                decoded: true,
            }],
            decoded: true,
        }
    }

    fn policy_option_extension(enabled: &str) -> super::OptionExtensionSchema {
        super::OptionExtensionSchema {
            registry_name: "acme".to_owned(),
            full_name: "acme.policy".to_owned(),
            extendee: ".google.protobuf.FileOptions".to_owned(),
            field_number: 51002,
            kind: "message".to_owned(),
            type_name: Some(".acme.Policy".to_owned()),
            is_repeated: false,
            values: vec![super::OptionValueSchema {
                kind: "message".to_owned(),
                bool_value: None,
                int_value: None,
                float_value: None,
                enum_name: None,
                string_value: None,
                bytes_hex: None,
                message_hex: Some("deadbeef".to_owned()),
                message_fields: vec![
                    super::OptionFieldSchema {
                        name: "enabled".to_owned(),
                        full_name: "acme.Policy.enabled".to_owned(),
                        number: 1,
                        kind: "bool".to_owned(),
                        type_name: None,
                        is_repeated: false,
                        values: vec![super::OptionValueSchema {
                            kind: "bool".to_owned(),
                            bool_value: Some(enabled == "true"),
                            int_value: None,
                            float_value: None,
                            enum_name: None,
                            string_value: None,
                            bytes_hex: None,
                            message_hex: None,
                            message_fields: Vec::new(),
                            raw_repr: enabled.to_owned(),
                            decoded: true,
                        }],
                        decoded: true,
                    },
                    super::OptionFieldSchema {
                        name: "nested".to_owned(),
                        full_name: "acme.Policy.nested".to_owned(),
                        number: 2,
                        kind: "message".to_owned(),
                        type_name: Some(".acme.Nested".to_owned()),
                        is_repeated: false,
                        values: vec![super::OptionValueSchema {
                            kind: "message".to_owned(),
                            bool_value: None,
                            int_value: None,
                            float_value: None,
                            enum_name: None,
                            string_value: None,
                            bytes_hex: None,
                            message_hex: Some("bead".to_owned()),
                            message_fields: vec![super::OptionFieldSchema {
                                name: "owner".to_owned(),
                                full_name: "acme.Nested.owner".to_owned(),
                                number: 1,
                                kind: "string".to_owned(),
                                type_name: None,
                                is_repeated: false,
                                values: vec![super::OptionValueSchema {
                                    kind: "string".to_owned(),
                                    bool_value: None,
                                    int_value: None,
                                    float_value: None,
                                    enum_name: None,
                                    string_value: Some("ops".to_owned()),
                                    bytes_hex: None,
                                    message_hex: None,
                                    message_fields: Vec::new(),
                                    raw_repr: "ops".to_owned(),
                                    decoded: true,
                                }],
                                decoded: true,
                            }],
                            raw_repr: "0xbead".to_owned(),
                            decoded: true,
                        }],
                        decoded: true,
                    },
                    super::OptionFieldSchema {
                        name: "ids".to_owned(),
                        full_name: "acme.Policy.ids".to_owned(),
                        number: 3,
                        kind: "int32".to_owned(),
                        type_name: None,
                        is_repeated: true,
                        values: vec![
                            super::OptionValueSchema {
                                kind: "int".to_owned(),
                                bool_value: None,
                                int_value: Some(7),
                                float_value: None,
                                enum_name: None,
                                string_value: None,
                                bytes_hex: None,
                                message_hex: None,
                                message_fields: Vec::new(),
                                raw_repr: "7".to_owned(),
                                decoded: true,
                            },
                            super::OptionValueSchema {
                                kind: "int".to_owned(),
                                bool_value: None,
                                int_value: Some(9),
                                float_value: None,
                                enum_name: None,
                                string_value: None,
                                bytes_hex: None,
                                message_hex: None,
                                message_fields: Vec::new(),
                                raw_repr: "9".to_owned(),
                                decoded: true,
                            },
                        ],
                        decoded: true,
                    },
                ],
                raw_repr: "0xdeadbeef".to_owned(),
                decoded: true,
            }],
            decoded: true,
        }
    }

    fn large_perf_context(
        file_count: usize,
        messages_per_file: usize,
        fields_per_message: usize,
    ) -> super::StarlarkProtoContext {
        let mut changed_files = Vec::with_capacity(file_count);
        let mut before = BTreeMap::new();
        let mut after = BTreeMap::new();

        for file_index in 0..file_count {
            let path = format!("proto/perf_{file_index:03}.proto");
            let package = format!("perf.f{file_index:03}");
            let before_file =
                build_perf_flat_file(&path, &package, messages_per_file, fields_per_message, true);
            let after_file =
                build_perf_flat_file(&path, &package, messages_per_file, fields_per_message, false);
            changed_files.push(super::ChangedProtoFile {
                current_path: Some(PathBuf::from(&path)),
                base_path: Some(PathBuf::from(&path)),
                kind: ChangeKind::Modified,
            });
            before.insert(path.clone(), before_file);
            after.insert(path, after_file);
        }

        super::build_context(
            &changed_files,
            &before,
            &after,
            &[],
            super::ParserBackend::Pure,
            super::ParserBackend::Pure,
            super::Severity::Warning,
        )
    }

    fn build_perf_flat_file(
        path: &str,
        package: &str,
        messages_per_file: usize,
        fields_per_message: usize,
        before_version: bool,
    ) -> super::FlatFileSchema {
        let mut messages = Vec::with_capacity(messages_per_file);
        let mut flat_messages = BTreeMap::new();
        for message_index in 0..messages_per_file {
            let (message, flat) = build_perf_message(
                package,
                &format!("Message{message_index:03}"),
                fields_per_message,
                before_version,
            );
            flat_messages.insert(message.full_name.clone(), flat);
            messages.push(message);
        }

        let options = super::DescriptorOptionsSchema {
            fingerprint: if before_version { "perf-before" } else { "perf-after" }.to_owned(),
            has_unknown_fields: false,
            uninterpreted: Vec::new(),
            extensions: vec![
                bool_option_extension("acme.flag", if before_version { "true" } else { "false" }),
                policy_option_extension(if before_version { "true" } else { "false" }),
            ],
        };

        super::FlatFileSchema {
            file: super::FileSchema {
                path: path.to_owned(),
                package: package.to_owned(),
                syntax: "proto2".to_owned(),
                options: options.clone(),
                messages,
                enums: Vec::new(),
                services: Vec::new(),
                extensions: Vec::new(),
            },
            messages: flat_messages,
            enums: BTreeMap::new(),
            services: BTreeMap::new(),
            extensions_by_number: BTreeMap::new(),
            extensions_by_name: BTreeMap::new(),
        }
    }

    fn build_perf_message(
        package: &str,
        name: &str,
        fields_per_message: usize,
        before_version: bool,
    ) -> (super::MessageSchema, super::FlatMessageSchema) {
        let full_name = format!("{package}.{name}");
        let mut fields = Vec::new();
        for field_index in 1..=fields_per_message {
            if !before_version && field_index % 9 == 0 {
                continue;
            }
            let (number, kind) = if !before_version && field_index % 7 == 0 {
                (field_index as i32 + 1000, "string")
            } else if !before_version && field_index % 5 == 0 {
                (field_index as i32, "int64")
            } else {
                (field_index as i32, "string")
            };
            fields.push(perf_field(
                &format!("{full_name}.field_{field_index:03}"),
                &format!("field_{field_index:03}"),
                number,
                kind,
                before_version,
            ));
        }

        let options = super::DescriptorOptionsSchema {
            fingerprint: if before_version { "msg-before" } else { "msg-after" }.to_owned(),
            has_unknown_fields: false,
            uninterpreted: Vec::new(),
            extensions: Vec::new(),
        };

        let fields_by_number = fields
            .iter()
            .cloned()
            .map(|field| (field.number, field))
            .collect();
        let fields_by_name = fields
            .iter()
            .cloned()
            .map(|field| (field.name.clone(), field))
            .collect();

        (
            super::MessageSchema {
                full_name: full_name.clone(),
                name: name.to_owned(),
                options: options.clone(),
                is_map_entry: false,
                fields: fields.clone(),
                oneofs: Vec::new(),
                extensions: Vec::new(),
                reserved_ranges: Vec::new(),
                reserved_names: Vec::new(),
                nested_messages: Vec::new(),
                nested_enums: Vec::new(),
            },
            super::FlatMessageSchema {
                full_name,
                fields_by_number,
                fields_by_name,
                oneofs_by_name: BTreeMap::new(),
                extensions_by_number: BTreeMap::new(),
                extensions_by_name: BTreeMap::new(),
                reserved_ranges: BTreeSet::new(),
                reserved_names: BTreeSet::new(),
                options,
                is_map_entry: false,
            },
        )
    }

    fn perf_field(
        full_name: &str,
        name: &str,
        number: i32,
        kind: &str,
        before_version: bool,
    ) -> super::FieldSchema {
        super::FieldSchema {
            full_name: full_name.to_owned(),
            name: name.to_owned(),
            number,
            label: "optional".to_owned(),
            kind: kind.to_owned(),
            type_name: None,
            json_name: None,
            oneof_index: None,
            oneof_name: None,
            proto3_optional: false,
            extendee: None,
            options: super::DescriptorOptionsSchema {
                fingerprint: if before_version { "field-before" } else { "field-after" }.to_owned(),
                has_unknown_fields: false,
                uninterpreted: Vec::new(),
                extensions: Vec::new(),
            },
        }
    }

    fn stress_proto_file(
        file_index: usize,
        messages_per_file: usize,
        fields_per_message: usize,
        before_version: bool,
    ) -> String {
        let mut output = format!("syntax = \"proto2\";\npackage perf.f{file_index:03};\n\n");
        for message_index in 0..messages_per_file {
            output.push_str(&format!("message Message{message_index:03} {{\n"));
            for field_index in 1..=fields_per_message {
                if !before_version && field_index % 9 == 0 {
                    continue;
                }
                let (number, kind) = if !before_version && field_index % 7 == 0 {
                    (field_index + 1000, "string")
                } else if !before_version && field_index % 5 == 0 {
                    (field_index, "int64")
                } else {
                    (field_index, "string")
                };
                output.push_str(&format!(
                    "  optional {kind} field_{field_index:03} = {number};\n"
                ));
            }
            output.push_str("}\n\n");
        }
        output
    }

    fn run_rust_perf_policy(context: &super::StarlarkProtoContext) -> Vec<Finding> {
        let mut findings = Vec::new();
        for delta in &context.deltas {
            if matches!(
                delta.kind.as_str(),
                "field_removed"
                    | "field_number_changed"
                    | "field_type_changed"
                    | "registered_option_value_changed"
            ) {
                findings.push(Finding {
                    severity: Severity::Warning,
                    message: format!("perf delta {}", delta.symbol),
                    location: Some(Location {
                        path: PathBuf::from(&delta.path),
                        line: None,
                        column: None,
                    }),
                    remediation: Some(super::DEFAULT_PROTOBUF_EVOLUTION_REMEDIATION.to_owned()),
                    suggested_fix: None,
                });
            }
        }

        for file_pair in &context.files {
            let target = file_pair.after.as_ref().or(file_pair.before.as_ref());
            let Some(target) = target else {
                continue;
            };
            if let Some(policy) = find_option_extension(&target.options, "acme.policy") {
                if bool_option_field_schema(&target.options, "acme.policy", "enabled") == Some(true)
                    && option_field_string_schema(&target.options, "acme.policy", "nested.owner")
                        == Some("ops")
                    && option_descendant_count_schema(policy) >= 3
                {
                    findings.push(Finding {
                        severity: Severity::Info,
                        message: format!("option policy {}", target.path),
                        location: Some(Location {
                            path: PathBuf::from(&file_pair.path),
                            line: None,
                            column: None,
                        }),
                        remediation: None,
                        suggested_fix: None,
                    });
                }
            }

            for message in &target.messages {
                for field in &message.fields {
                    if field.kind == "string" && field.number % 11 == 0 {
                        findings.push(Finding {
                            severity: Severity::Info,
                            message: format!("field sample {}", field.full_name),
                            location: Some(Location {
                                path: PathBuf::from(&file_pair.path),
                                line: None,
                                column: None,
                            }),
                            remediation: None,
                            suggested_fix: None,
                        });
                    }
                }
            }
        }

        findings
    }

    fn find_option_extension<'a>(
        options: &'a super::DescriptorOptionsSchema,
        full_name: &str,
    ) -> Option<&'a super::OptionExtensionSchema> {
        options
            .extensions
            .iter()
            .find(|extension| extension.full_name == full_name)
    }

    fn bool_option_field_schema(
        options: &super::DescriptorOptionsSchema,
        full_name: &str,
        field_path: &str,
    ) -> Option<bool> {
        option_field_values_schema(options, full_name, field_path)
            .into_iter()
            .find_map(|value| value.bool_value)
    }

    fn option_field_string_schema<'a>(
        options: &'a super::DescriptorOptionsSchema,
        full_name: &str,
        field_path: &str,
    ) -> Option<&'a str> {
        option_field_values_schema(options, full_name, field_path)
            .into_iter()
            .find_map(|value| value.string_value.as_deref())
    }

    fn option_field_values_schema<'a>(
        options: &'a super::DescriptorOptionsSchema,
        full_name: &str,
        field_path: &str,
    ) -> Vec<&'a super::OptionValueSchema> {
        let mut values = options
            .extensions
            .iter()
            .filter(|extension| extension.full_name == full_name)
            .flat_map(|extension| extension.values.iter())
            .collect::<Vec<_>>();
        for segment in field_path.split('.').filter(|segment| !segment.is_empty()) {
            let mut next = Vec::new();
            for value in values {
                for field in &value.message_fields {
                    if field.name == segment {
                        next.extend(field.values.iter());
                    }
                }
            }
            values = next;
        }
        values
    }

    fn option_descendant_count_schema(extension: &super::OptionExtensionSchema) -> usize {
        extension
            .values
            .iter()
            .map(option_value_descendant_count_schema)
            .sum()
    }

    fn option_value_descendant_count_schema(value: &super::OptionValueSchema) -> usize {
        value.message_fields.len()
            + value
                .message_fields
                .iter()
                .map(option_field_descendant_count_schema)
                .sum::<usize>()
    }

    fn option_field_descendant_count_schema(field: &super::OptionFieldSchema) -> usize {
        field.values
            .iter()
            .map(option_value_descendant_count_schema)
            .sum()
    }

    fn env_usize(name: &str, default: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(default)
    }

    fn measure_sync<F>(samples: usize, mut run: F) -> Duration
    where
        F: FnMut() -> usize,
    {
        let mut durations = Vec::with_capacity(samples);
        let mut sink = 0usize;
        for _ in 0..samples {
            let started_at = Instant::now();
            sink ^= black_box(run());
            durations.push(started_at.elapsed());
        }
        durations.sort_unstable();
        black_box(sink);
        durations[durations.len() / 2]
    }

    fn format_duration(duration: Duration) -> String {
        if duration.as_secs() >= 1 {
            format!("{:.2}s", duration.as_secs_f64())
        } else if duration.as_millis() >= 1 {
            format!("{:.1}ms", duration.as_secs_f64() * 1_000.0)
        } else {
            format!("{:.1}us", duration.as_secs_f64() * 1_000_000.0)
        }
    }

    fn parse_perf_policy(source: &str) {
        let dialect = Dialect {
            enable_types: DialectTypes::Enable,
            ..Dialect::Standard
        };
        AstModule::parse("<perf>", source.to_owned(), &dialect).expect("parse perf policy");
    }

    fn compile_perf_policy_check(source: &str) -> OwnedFrozenValue {
        let dialect = Dialect {
            enable_types: DialectTypes::Enable,
            ..Dialect::Standard
        };
        let ast = AstModule::parse("<perf>", source.to_owned(), &dialect)
            .expect("parse perf policy");
        let globals = super::protobuf_starlark_globals();
        let module = Module::new();
        let mut evaluator = Evaluator::new(&module);
        evaluator
            .eval_module(ast, &globals)
            .expect("eval perf policy module");
        drop(evaluator);
        let frozen = module.freeze().expect("freeze perf policy module");
        frozen.get("check").expect("get perf check function")
    }

    fn call_frozen_perf_policy(
        check: &OwnedFrozenValue,
        context: &super::StarlarkProtoContext,
    ) -> anyhow::Result<Vec<Finding>> {
        let module = Module::new();
        let mut evaluator = Evaluator::new(&module);
        let check = check.owned_value(module.frozen_heap());
        let ctx = module.heap().alloc(context.clone().into_proto_context_value());
        let value = evaluator
            .eval_function(check, &[ctx], &[])
            .map_err(|error| anyhow!(error.to_string()))?;
        super::unpack_starlark_findings(value)
    }
}

#[cfg(test)]
const PERF_POLICY_SOURCE: &str = r#"
def check(ctx: ProtoContext) -> list[Finding]:
    findings = []
    for delta in ctx.deltas:
        if (
            delta.kind == DeltaKinds.field_removed
            or delta.kind == DeltaKinds.field_number_changed
            or delta.kind == DeltaKinds.field_type_changed
            or delta.kind == DeltaKinds.registered_option_value_changed
        ):
            findings.append(finding_for_delta(
                ctx,
                delta,
                "perf delta {}".format(delta.symbol),
                severity = Severities.warning,
            ))

    for file_pair in ctx.files:
        target = file_pair.after if file_pair.after != None else file_pair.before
        if target == None:
            continue
        if has_option(target.options, "acme.policy"):
            descendants = option_descendants(option_extensions(target.options, "acme.policy")[0])
            owners = option_field_values(target.options, "acme.policy", "nested.owner")
            enabled = bool_option_field(target.options, "acme.policy", "enabled")
            if enabled and owners and owners[0].string_value == "ops" and len(descendants) >= 3:
                findings.append(info(
                    message = "option policy {}".format(target.path),
                    path = file_pair.path,
                ))
        for message in target.messages:
            for field in message.fields:
                if field.kind == FieldKinds.string and field.number % 11 == 0:
                    findings.append(info(
                        message = "field sample {}".format(field.full_name),
                        path = file_pair.path,
                    ))
    return findings
"#;
