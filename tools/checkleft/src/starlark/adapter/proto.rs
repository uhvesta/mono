use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use globset::{Glob, GlobSetBuilder};
use protobuf::descriptor::field_descriptor_proto::{Label, Type};
use protobuf::descriptor::{
    DescriptorProto, EnumDescriptorProto, EnumValueDescriptorProto, FieldDescriptorProto, FileDescriptorProto,
    FileDescriptorSet, MethodDescriptorProto, ServiceDescriptorProto,
};
use protobuf_parse::Parser;
use starlark::values::structs::AllocStruct;
use starlark::values::{Heap, Value};
use tempfile::TempDir;

use crate::input::{ChangeKind, ChangedFile, SourceTree, TreeVersion};
use crate::path::validate_relative_path;
use crate::starlark::adapter::{AdapterFileSelector, AdapterInput, AdapterPreparedOutput, FormatAdapter};

#[derive(Debug)]
pub(crate) struct ProtoAdapterOutput {
    files: Vec<ProtoFilePair>,
    deltas: Vec<ProtoDelta>,
}

impl ProtoAdapterOutput {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn alloc_context<'v>(&self, heap: Heap<'v>) -> Value<'v> {
        let files = self
            .files
            .iter()
            .map(|file| alloc_proto_file_pair(heap, file))
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

pub(crate) struct ProtoAdapter;

impl FormatAdapter for ProtoAdapter {
    fn kind(&self) -> &'static str {
        "proto"
    }

    fn file_selectors(&self) -> &'static [AdapterFileSelector] {
        &[AdapterFileSelector::Ext("proto")]
    }

    fn prepare(&self, input: AdapterInput<'_>) -> Result<AdapterPreparedOutput> {
        Ok(AdapterPreparedOutput::Proto(collect_proto_output(
            input.changeset,
            input.tree,
            input.applies_to,
            input.package_scope,
        )?))
    }
}

#[derive(Debug)]
struct ProtoFilePair {
    path: PathBuf,
    before: Option<ProtoFile>,
    after: Option<ProtoFile>,
    change_kind: ChangeKind,
}

#[derive(Debug, Clone)]
struct ProtoFile {
    path: String,
    package: String,
    syntax: String,
    messages: Vec<ProtoMessage>,
    enums: Vec<ProtoEnum>,
    services: Vec<ProtoService>,
}

#[derive(Debug, Clone)]
struct ProtoMessage {
    full_name: String,
    name: String,
    fields: Vec<ProtoField>,
    messages: Vec<ProtoMessage>,
    enums: Vec<ProtoEnum>,
}

#[derive(Debug, Clone)]
struct ProtoField {
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
}

#[derive(Debug, Clone)]
struct ProtoEnum {
    full_name: String,
    name: String,
    values: Vec<ProtoEnumValue>,
}

#[derive(Debug, Clone)]
struct ProtoEnumValue {
    full_name: String,
    name: String,
    number: i32,
}

#[derive(Debug, Clone)]
struct ProtoService {
    full_name: String,
    name: String,
    methods: Vec<ProtoMethod>,
}

#[derive(Debug, Clone)]
struct ProtoMethod {
    full_name: String,
    name: String,
    input_type: String,
    output_type: String,
    client_streaming: bool,
    server_streaming: bool,
}

#[derive(Debug)]
struct ProtoDelta {
    kind: String,
    path: PathBuf,
    symbol: String,
}

fn collect_proto_output(
    changeset: &crate::input::ChangeSet,
    tree: &dyn SourceTree,
    applies_to: &[String],
    package_scope: Option<&Path>,
) -> Result<ProtoAdapterOutput> {
    let glob_set = build_glob_set(applies_to)?;
    let targets = changeset
        .changed_files
        .iter()
        .filter(|changed| is_proto_path(&changed.path) || changed.old_path.as_deref().is_some_and(is_proto_path))
        .filter(|changed| matches_changed_proto(&glob_set, changed, package_scope))
        .collect::<Vec<_>>();
    if targets.is_empty() {
        return Ok(ProtoAdapterOutput {
            files: Vec::new(),
            deltas: Vec::new(),
        });
    }

    let snapshot_paths = collect_snapshot_proto_paths(changeset, tree, package_scope)?;
    let current = build_snapshot(tree, &snapshot_paths, &BTreeMap::new(), TreeVersion::Current)?;
    let base_overrides = base_path_overrides(&targets);
    let base = build_snapshot(tree, &snapshot_paths, &base_overrides, TreeVersion::Base)?;

    let current_inputs = targets
        .iter()
        .filter(|changed| !matches!(changed.kind, ChangeKind::Deleted))
        .map(|changed| changed.path.clone())
        .collect::<BTreeSet<_>>();
    let base_inputs = targets
        .iter()
        .filter(|changed| !matches!(changed.kind, ChangeKind::Added))
        .map(|changed| before_path(changed).to_path_buf())
        .collect::<BTreeSet<_>>();

    let current_files = parse_snapshot(current.path(), &current_inputs)?;
    let base_files = parse_snapshot(base.path(), &base_inputs)?;

    let mut files = Vec::new();
    let mut deltas = Vec::new();
    for changed in targets {
        let before_path = before_path(changed);
        let before = base_files.get(&before_path.to_string_lossy().to_string()).cloned();
        let after = current_files.get(&changed.path.to_string_lossy().to_string()).cloned();
        deltas.extend(proto_deltas(&changed.path, before.as_ref(), after.as_ref()));
        files.push(ProtoFilePair {
            path: changed.path.clone(),
            before,
            after,
            change_kind: changed.kind,
        });
    }
    Ok(ProtoAdapterOutput { files, deltas })
}

fn collect_snapshot_proto_paths(
    changeset: &crate::input::ChangeSet,
    tree: &dyn SourceTree,
    package_scope: Option<&Path>,
) -> Result<BTreeSet<PathBuf>> {
    let pattern = package_scope
        .filter(|scope| !scope.as_os_str().is_empty())
        .map(|scope| format!("{}/**/*.proto", scope.display()))
        .unwrap_or_else(|| "**/*.proto".to_owned());
    let mut paths = tree.glob(&pattern)?.into_iter().collect::<BTreeSet<_>>();
    for changed in &changeset.changed_files {
        if is_proto_path(&changed.path) {
            paths.insert(changed.path.clone());
        }
        if let Some(old_path) = &changed.old_path {
            if is_proto_path(old_path) {
                paths.insert(old_path.clone());
            }
        }
    }
    Ok(paths)
}

fn base_path_overrides(targets: &[&ChangedFile]) -> BTreeMap<PathBuf, PathBuf> {
    targets
        .iter()
        .filter(|changed| !matches!(changed.kind, ChangeKind::Added))
        .map(|changed| (before_path(changed).to_path_buf(), before_path(changed).to_path_buf()))
        .collect()
}

fn build_snapshot(
    tree: &dyn SourceTree,
    paths: &BTreeSet<PathBuf>,
    base_overrides: &BTreeMap<PathBuf, PathBuf>,
    default_version: TreeVersion,
) -> Result<TempDir> {
    let temp = TempDir::new().context("failed to create protobuf snapshot")?;
    for path in paths {
        validate_relative_path(path)?;
        let version = if base_overrides.contains_key(path) {
            TreeVersion::Base
        } else {
            default_version
        };
        let bytes = match tree.read_file_versioned(path, version) {
            Ok(bytes) => bytes,
            Err(err) if matches!(version, TreeVersion::Base) => {
                if matches!(default_version, TreeVersion::Base) {
                    continue;
                }
                return Err(err).with_context(|| format!("failed to read current proto {}", path.display()));
            }
            Err(err) => {
                if matches!(default_version, TreeVersion::Current) && !tree.exists(path) {
                    continue;
                }
                return Err(err).with_context(|| format!("failed to read proto {}", path.display()));
            }
        };
        let destination = temp.path().join(path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(&destination, bytes).with_context(|| format!("failed to write {}", destination.display()))?;
    }
    Ok(temp)
}

fn parse_snapshot(snapshot_root: &Path, inputs: &BTreeSet<PathBuf>) -> Result<BTreeMap<String, ProtoFile>> {
    if inputs.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut parser = Parser::new();
    parser.pure();
    parser.include(snapshot_root);
    parser.inputs(inputs.iter().map(|path| snapshot_root.join(path)));
    let descriptor_set = parser.file_descriptor_set().map_err(|error| anyhow!(error))?;
    Ok(extract_proto_files(&descriptor_set, inputs))
}

fn extract_proto_files(descriptor_set: &FileDescriptorSet, targets: &BTreeSet<PathBuf>) -> BTreeMap<String, ProtoFile> {
    let target_names = targets
        .iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect::<BTreeSet<_>>();
    descriptor_set
        .file
        .iter()
        .filter(|file| target_names.contains(file.name()))
        .map(|file| {
            let proto_file = proto_file_schema(file);
            (proto_file.path.clone(), proto_file)
        })
        .collect()
}

fn proto_file_schema(file: &FileDescriptorProto) -> ProtoFile {
    ProtoFile {
        path: file.name().to_owned(),
        package: file.package().to_owned(),
        syntax: file.syntax().to_owned(),
        messages: collect_messages(file.package(), "", &file.message_type),
        enums: collect_enums(file.package(), "", &file.enum_type),
        services: collect_services(file.package(), &file.service),
    }
}

fn collect_messages(package: &str, parent: &str, messages: &[DescriptorProto]) -> Vec<ProtoMessage> {
    messages
        .iter()
        .map(|message| {
            let name = message.name().to_owned();
            let full_name = join_proto_name(package, parent, &name);
            let oneof_names = message
                .oneof_decl
                .iter()
                .map(|oneof| oneof.name().to_owned())
                .collect::<Vec<_>>();
            ProtoMessage {
                full_name: full_name.clone(),
                name,
                fields: message
                    .field
                    .iter()
                    .map(|field| field_schema(field, &full_name, &oneof_names))
                    .collect(),
                messages: collect_messages(package, &full_name, &message.nested_type),
                enums: collect_enums(package, &full_name, &message.enum_type),
            }
        })
        .collect()
}

fn collect_enums(package: &str, parent: &str, enums: &[EnumDescriptorProto]) -> Vec<ProtoEnum> {
    enums
        .iter()
        .map(|proto_enum| {
            let name = proto_enum.name().to_owned();
            let full_name = join_proto_name(package, parent, &name);
            ProtoEnum {
                full_name: full_name.clone(),
                name,
                values: proto_enum
                    .value
                    .iter()
                    .map(|value| enum_value_schema(value, &full_name))
                    .collect(),
            }
        })
        .collect()
}

fn collect_services(package: &str, services: &[ServiceDescriptorProto]) -> Vec<ProtoService> {
    services
        .iter()
        .map(|service| {
            let name = service.name().to_owned();
            let full_name = join_proto_name(package, "", &name);
            ProtoService {
                full_name: full_name.clone(),
                name,
                methods: service
                    .method
                    .iter()
                    .map(|method| method_schema(method, &full_name))
                    .collect(),
            }
        })
        .collect()
}

fn field_schema(field: &FieldDescriptorProto, parent_full_name: &str, oneof_names: &[String]) -> ProtoField {
    let name = field.name().to_owned();
    ProtoField {
        full_name: join_proto_name("", parent_full_name, &name),
        name,
        number: field.number(),
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
    }
}

fn enum_value_schema(value: &EnumValueDescriptorProto, parent_full_name: &str) -> ProtoEnumValue {
    let name = value.name().to_owned();
    ProtoEnumValue {
        full_name: join_proto_name("", parent_full_name, &name),
        name,
        number: value.number(),
    }
}

fn method_schema(method: &MethodDescriptorProto, parent_full_name: &str) -> ProtoMethod {
    let name = method.name().to_owned();
    ProtoMethod {
        full_name: join_proto_name("", parent_full_name, &name),
        name,
        input_type: method.input_type().to_owned(),
        output_type: method.output_type().to_owned(),
        client_streaming: method.client_streaming(),
        server_streaming: method.server_streaming(),
    }
}

fn proto_deltas(path: &Path, before: Option<&ProtoFile>, after: Option<&ProtoFile>) -> Vec<ProtoDelta> {
    let mut deltas = Vec::new();
    let before_messages = before.map(flat_messages).unwrap_or_default();
    let after_messages = after.map(flat_messages).unwrap_or_default();
    for (message_name, before_message) in &before_messages {
        let Some(after_message) = after_messages.get(message_name) else {
            deltas.push(delta(path, "message_removed", message_name));
            continue;
        };
        compare_message(path, before_message, after_message, &mut deltas);
    }

    let before_enums = before.map(flat_enums).unwrap_or_default();
    let after_enums = after.map(flat_enums).unwrap_or_default();
    for (enum_name, before_enum) in &before_enums {
        let Some(after_enum) = after_enums.get(enum_name) else {
            deltas.push(delta(path, "enum_removed", enum_name));
            continue;
        };
        compare_enum(path, before_enum, after_enum, &mut deltas);
    }

    let before_services = before.map(flat_services).unwrap_or_default();
    let after_services = after.map(flat_services).unwrap_or_default();
    for service_name in before_services.keys() {
        if !after_services.contains_key(service_name) {
            deltas.push(delta(path, "service_removed", service_name));
        }
    }
    deltas
}

fn compare_message(path: &Path, before: &ProtoMessage, after: &ProtoMessage, deltas: &mut Vec<ProtoDelta>) {
    let before_fields = field_map(&before.fields);
    let after_fields = field_map(&after.fields);
    for (field_name, before_field) in before_fields {
        let Some(after_field) = after_fields.get(&field_name) else {
            deltas.push(delta(path, "field_removed", &before_field.full_name));
            continue;
        };
        if before_field.number != after_field.number {
            deltas.push(delta(path, "field_number_changed", &before_field.full_name));
        }
        if before_field.kind != after_field.kind || before_field.type_name != after_field.type_name {
            deltas.push(delta(path, "field_type_changed", &before_field.full_name));
        }
        if before_field.label != after_field.label {
            deltas.push(delta(path, "field_label_changed", &before_field.full_name));
        }
        if before_field.oneof_name != after_field.oneof_name {
            deltas.push(delta(path, "field_oneof_changed", &before_field.full_name));
        }
    }
}

fn compare_enum(path: &Path, before: &ProtoEnum, after: &ProtoEnum, deltas: &mut Vec<ProtoDelta>) {
    let before_values = enum_value_map(&before.values);
    let after_values = enum_value_map(&after.values);
    for (value_name, before_value) in before_values {
        let Some(after_value) = after_values.get(&value_name) else {
            deltas.push(delta(path, "enum_value_removed", &before_value.full_name));
            continue;
        };
        if before_value.number != after_value.number {
            deltas.push(delta(path, "enum_value_number_changed", &before_value.full_name));
        }
    }
}

fn flat_messages(file: &ProtoFile) -> BTreeMap<String, ProtoMessage> {
    let mut output = BTreeMap::new();
    for message in &file.messages {
        insert_message(message, &mut output);
    }
    output
}

fn insert_message(message: &ProtoMessage, output: &mut BTreeMap<String, ProtoMessage>) {
    output.insert(message.full_name.clone(), message.clone());
    for nested in &message.messages {
        insert_message(nested, output);
    }
}

fn flat_enums(file: &ProtoFile) -> BTreeMap<String, ProtoEnum> {
    let mut output = BTreeMap::new();
    for proto_enum in &file.enums {
        output.insert(proto_enum.full_name.clone(), proto_enum.clone());
    }
    for message in &file.messages {
        insert_nested_enums(message, &mut output);
    }
    output
}

fn insert_nested_enums(message: &ProtoMessage, output: &mut BTreeMap<String, ProtoEnum>) {
    for proto_enum in &message.enums {
        output.insert(proto_enum.full_name.clone(), proto_enum.clone());
    }
    for nested in &message.messages {
        insert_nested_enums(nested, output);
    }
}

fn flat_services(file: &ProtoFile) -> BTreeMap<String, ProtoService> {
    file.services
        .iter()
        .map(|service| (service.full_name.clone(), service.clone()))
        .collect()
}

fn field_map(fields: &[ProtoField]) -> BTreeMap<String, ProtoField> {
    fields.iter().map(|field| (field.name.clone(), field.clone())).collect()
}

fn enum_value_map(values: &[ProtoEnumValue]) -> BTreeMap<String, ProtoEnumValue> {
    values.iter().map(|value| (value.name.clone(), value.clone())).collect()
}

fn delta(path: &Path, kind: &str, symbol: &str) -> ProtoDelta {
    ProtoDelta {
        kind: kind.to_owned(),
        path: path.to_path_buf(),
        symbol: symbol.to_owned(),
    }
}

fn describe_field_label(field: &FieldDescriptorProto) -> String {
    match field
        .label
        .as_ref()
        .map(|label| label.enum_value_or_default())
        .unwrap_or_default()
    {
        Label::LABEL_OPTIONAL => "optional",
        Label::LABEL_REQUIRED => "required",
        Label::LABEL_REPEATED => "repeated",
    }
    .to_owned()
}

fn describe_field_kind(field: &FieldDescriptorProto) -> String {
    match field
        .type_
        .as_ref()
        .map(|kind| kind.enum_value_or_default())
        .unwrap_or_default()
    {
        Type::TYPE_DOUBLE => "double",
        Type::TYPE_FLOAT => "float",
        Type::TYPE_INT64 => "int64",
        Type::TYPE_UINT64 => "uint64",
        Type::TYPE_INT32 => "int32",
        Type::TYPE_FIXED64 => "fixed64",
        Type::TYPE_FIXED32 => "fixed32",
        Type::TYPE_BOOL => "bool",
        Type::TYPE_STRING => "string",
        Type::TYPE_GROUP => "group",
        Type::TYPE_MESSAGE => "message",
        Type::TYPE_BYTES => "bytes",
        Type::TYPE_UINT32 => "uint32",
        Type::TYPE_ENUM => "enum",
        Type::TYPE_SFIXED32 => "sfixed32",
        Type::TYPE_SFIXED64 => "sfixed64",
        Type::TYPE_SINT32 => "sint32",
        Type::TYPE_SINT64 => "sint64",
    }
    .to_owned()
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn join_proto_name(package: &str, parent: &str, name: &str) -> String {
    match (package.is_empty(), parent.is_empty()) {
        (true, true) => name.to_owned(),
        (true, false) => format!("{parent}.{name}"),
        (false, true) => format!("{package}.{name}"),
        (false, false) => format!("{parent}.{name}"),
    }
}

fn matches_changed_proto(glob_set: &globset::GlobSet, changed: &ChangedFile, package_scope: Option<&Path>) -> bool {
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

fn is_proto_path(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("proto")
}

fn alloc_proto_file_pair<'v>(heap: Heap<'v>, file: &ProtoFilePair) -> Value<'v> {
    let before = file
        .before
        .as_ref()
        .map_or_else(Value::new_none, |file| alloc_proto_file(heap, file));
    let after = file
        .after
        .as_ref()
        .map_or_else(Value::new_none, |file| alloc_proto_file(heap, file));
    heap.alloc(AllocStruct([
        ("path", heap.alloc(file.path.to_string_lossy().to_string())),
        ("before", before),
        ("after", after),
        ("change_kind", heap.alloc(change_kind_name(file.change_kind))),
    ]))
}

fn alloc_proto_file<'v>(heap: Heap<'v>, file: &ProtoFile) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("path", heap.alloc(file.path.clone())),
        ("package", heap.alloc(file.package.clone())),
        ("syntax", heap.alloc(file.syntax.clone())),
        (
            "messages",
            heap.alloc(file.messages.iter().map(|m| alloc_message(heap, m)).collect::<Vec<_>>()),
        ),
        (
            "enums",
            heap.alloc(file.enums.iter().map(|e| alloc_enum(heap, e)).collect::<Vec<_>>()),
        ),
        (
            "services",
            heap.alloc(file.services.iter().map(|s| alloc_service(heap, s)).collect::<Vec<_>>()),
        ),
    ]))
}

fn alloc_message<'v>(heap: Heap<'v>, message: &ProtoMessage) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("full_name", heap.alloc(message.full_name.clone())),
        ("name", heap.alloc(message.name.clone())),
        (
            "fields",
            heap.alloc(message.fields.iter().map(|f| alloc_field(heap, f)).collect::<Vec<_>>()),
        ),
        (
            "messages",
            heap.alloc(
                message
                    .messages
                    .iter()
                    .map(|m| alloc_message(heap, m))
                    .collect::<Vec<_>>(),
            ),
        ),
        (
            "enums",
            heap.alloc(message.enums.iter().map(|e| alloc_enum(heap, e)).collect::<Vec<_>>()),
        ),
    ]))
}

fn alloc_field<'v>(heap: Heap<'v>, field: &ProtoField) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("full_name", heap.alloc(field.full_name.clone())),
        ("name", heap.alloc(field.name.clone())),
        ("number", heap.alloc(field.number)),
        ("label", heap.alloc(field.label.clone())),
        ("kind", heap.alloc(field.kind.clone())),
        (
            "type_name",
            field
                .type_name
                .as_ref()
                .map_or_else(Value::new_none, |value| heap.alloc(value.clone())),
        ),
        (
            "json_name",
            field
                .json_name
                .as_ref()
                .map_or_else(Value::new_none, |value| heap.alloc(value.clone())),
        ),
        (
            "oneof_index",
            field
                .oneof_index
                .map_or_else(Value::new_none, |value| heap.alloc(value)),
        ),
        (
            "oneof_name",
            field
                .oneof_name
                .as_ref()
                .map_or_else(Value::new_none, |value| heap.alloc(value.clone())),
        ),
        ("proto3_optional", heap.alloc(field.proto3_optional)),
    ]))
}

fn alloc_enum<'v>(heap: Heap<'v>, proto_enum: &ProtoEnum) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("full_name", heap.alloc(proto_enum.full_name.clone())),
        ("name", heap.alloc(proto_enum.name.clone())),
        (
            "values",
            heap.alloc(
                proto_enum
                    .values
                    .iter()
                    .map(|value| alloc_enum_value(heap, value))
                    .collect::<Vec<_>>(),
            ),
        ),
    ]))
}

fn alloc_enum_value<'v>(heap: Heap<'v>, value: &ProtoEnumValue) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("full_name", heap.alloc(value.full_name.clone())),
        ("name", heap.alloc(value.name.clone())),
        ("number", heap.alloc(value.number)),
    ]))
}

fn alloc_service<'v>(heap: Heap<'v>, service: &ProtoService) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("full_name", heap.alloc(service.full_name.clone())),
        ("name", heap.alloc(service.name.clone())),
        (
            "methods",
            heap.alloc(
                service
                    .methods
                    .iter()
                    .map(|method| alloc_method(heap, method))
                    .collect::<Vec<_>>(),
            ),
        ),
    ]))
}

fn alloc_method<'v>(heap: Heap<'v>, method: &ProtoMethod) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("full_name", heap.alloc(method.full_name.clone())),
        ("name", heap.alloc(method.name.clone())),
        ("input_type", heap.alloc(method.input_type.clone())),
        ("output_type", heap.alloc(method.output_type.clone())),
        ("client_streaming", heap.alloc(method.client_streaming)),
        ("server_streaming", heap.alloc(method.server_streaming)),
    ]))
}

fn alloc_delta<'v>(heap: Heap<'v>, delta: &ProtoDelta) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("kind", heap.alloc(delta.kind.clone())),
        ("path", heap.alloc(delta.path.to_string_lossy().to_string())),
        ("symbol", heap.alloc(delta.symbol.clone())),
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
