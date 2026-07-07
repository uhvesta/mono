use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use globset::{Glob, GlobSetBuilder};
use starlark::values::structs::AllocStruct;
use starlark::values::{Heap, Value};
use tree_sitter::{Node, Parser};

use crate::input::{ChangeKind, ChangedFile, SourceTree, TreeVersion};
use crate::starlark::adapter::{AdapterInput, AdapterPreparedOutput, FormatAdapter};

#[derive(Debug)]
pub(crate) struct JavaAdapterOutput {
    files: Vec<JavaFilePair>,
    deltas: Vec<JavaDelta>,
}

impl JavaAdapterOutput {
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

pub(crate) struct JavaAdapter;

impl FormatAdapter for JavaAdapter {
    fn kind(&self) -> &'static str {
        "java"
    }

    fn prepare(&self, input: AdapterInput<'_>) -> Result<AdapterPreparedOutput> {
        Ok(AdapterPreparedOutput::Java(JavaAdapterOutput::prepare(
            input.changeset,
            input.tree,
            input.applies_to,
            input.package_scope,
        )?))
    }
}

#[derive(Debug)]
struct JavaFilePair {
    path: PathBuf,
    before: Option<JavaFile>,
    after: Option<JavaFile>,
    change_kind: ChangeKind,
}

#[derive(Debug, Clone)]
struct JavaFile {
    package: String,
    imports: Vec<String>,
    classes: Vec<JavaClass>,
}

#[derive(Debug, Clone)]
struct JavaClass {
    name: String,
    full_name: String,
    visibility: String,
    modifiers: Vec<String>,
    superclass: Option<String>,
    interfaces: Vec<String>,
    annotations: Vec<JavaAnnotation>,
    methods: Vec<JavaMethod>,
    fields: Vec<JavaField>,
    inner_classes: Vec<JavaClass>,
}

#[derive(Debug, Clone)]
struct JavaAnnotation {
    name: String,
}

#[derive(Debug, Clone)]
struct JavaMethod {
    name: String,
    visibility: String,
    return_type: String,
    parameters: Vec<JavaParameter>,
    annotations: Vec<JavaAnnotation>,
    modifiers: Vec<String>,
}

#[derive(Debug, Clone)]
struct JavaParameter {
    name: String,
    type_name: String,
}

#[derive(Debug, Clone)]
struct JavaField {
    name: String,
    visibility: String,
    type_name: String,
    annotations: Vec<JavaAnnotation>,
    modifiers: Vec<String>,
}

#[derive(Debug)]
struct JavaDelta {
    kind: String,
    path: PathBuf,
    symbol: String,
}

impl JavaAdapterOutput {
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
            if !is_java_path(&changed.path) && !changed.old_path.as_deref().is_some_and(is_java_path) {
                continue;
            }
            if !matches_changed_file(&glob_set, changed, package_scope) {
                continue;
            }

            let before = read_java_file(tree, before_path(changed), TreeVersion::Base).transpose()?;
            let after = read_java_file(tree, &changed.path, TreeVersion::Current).transpose()?;
            deltas.extend(java_deltas(&changed.path, before.as_ref(), after.as_ref()));
            files.push(JavaFilePair {
                path: changed.path.clone(),
                before,
                after,
                change_kind: changed.kind,
            });
        }
        Ok(Self { files, deltas })
    }
}

fn read_java_file(tree: &dyn SourceTree, path: &Path, version: TreeVersion) -> Option<Result<JavaFile>> {
    let bytes = match tree.read_file_versioned(path, version) {
        Ok(bytes) => bytes,
        Err(_) => return None,
    };
    Some(parse_java_file(path, &bytes))
}

fn parse_java_file(path: &Path, bytes: &[u8]) -> Result<JavaFile> {
    let source = std::str::from_utf8(bytes).with_context(|| format!("{} is not valid UTF-8", path.display()))?;
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .map_err(|err| anyhow!("failed to initialize Java parser: {err}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("failed to parse {}", path.display()))?;
    if tree.root_node().has_error() {
        return Err(anyhow!("{} contains Java syntax errors", path.display()));
    }
    let root = tree.root_node();
    let package = find_package(root, bytes).unwrap_or_default();
    let imports = find_imports(root, bytes);
    let classes = collect_classes(root, bytes, &package, "");
    Ok(JavaFile {
        package,
        imports,
        classes,
    })
}

fn find_package(root: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .find(|child| child.kind() == "package_declaration")
        .and_then(|node| named_child_texts(node, source).last().cloned())
}

fn find_imports(root: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut imports = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "import_declaration" {
            continue;
        }
        if let Some(path) = named_child_texts(child, source).last() {
            imports.push(path.clone());
        }
    }
    imports
}

fn collect_classes(node: Node<'_>, source: &[u8], package: &str, parent_full_name: &str) -> Vec<JavaClass> {
    let mut classes = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if is_class_node(child) {
            classes.push(class_schema(child, source, package, parent_full_name));
            continue;
        }
        if parent_full_name.is_empty() {
            classes.extend(collect_classes(child, source, package, parent_full_name));
        }
    }
    classes
}

fn class_schema(node: Node<'_>, source: &[u8], package: &str, parent_full_name: &str) -> JavaClass {
    let name = node_text_field(node, source, "name").unwrap_or_default();
    let full_name = if !parent_full_name.is_empty() {
        format!("{parent_full_name}.{name}")
    } else if !package.is_empty() {
        format!("{package}.{name}")
    } else {
        name.clone()
    };
    let modifier_text = modifier_text(node, source);
    let body = node.child_by_field_name("body");
    JavaClass {
        name,
        full_name: full_name.clone(),
        visibility: visibility(&modifier_text),
        modifiers: modifiers(&modifier_text),
        superclass: superclass(node, source),
        interfaces: interfaces(node, source),
        annotations: annotations(&modifier_text),
        methods: body.map_or_else(Vec::new, |body| collect_methods(body, source)),
        fields: body.map_or_else(Vec::new, |body| collect_fields(body, source)),
        inner_classes: body.map_or_else(Vec::new, |body| collect_classes(body, source, package, &full_name)),
    }
}

fn collect_methods(body: Node<'_>, source: &[u8]) -> Vec<JavaMethod> {
    let mut methods = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if !matches!(child.kind(), "method_declaration" | "constructor_declaration") {
            continue;
        }
        let modifier_text = modifier_text(child, source);
        let name = node_text_field(child, source, "name").unwrap_or_default();
        let return_type = child
            .child_by_field_name("type")
            .and_then(|node| node_text(node, source))
            .unwrap_or_else(|| {
                if child.kind() == "constructor_declaration" {
                    "<constructor>".to_owned()
                } else {
                    "void".to_owned()
                }
            });
        methods.push(JavaMethod {
            name,
            visibility: visibility(&modifier_text),
            return_type,
            parameters: child
                .child_by_field_name("parameters")
                .map_or_else(Vec::new, |parameters| collect_parameters(parameters, source)),
            annotations: annotations(&modifier_text),
            modifiers: modifiers(&modifier_text),
        });
    }
    methods
}

fn collect_parameters(parameters: Node<'_>, source: &[u8]) -> Vec<JavaParameter> {
    let mut output = Vec::new();
    let mut cursor = parameters.walk();
    for child in parameters.named_children(&mut cursor) {
        if !matches!(child.kind(), "formal_parameter" | "spread_parameter") {
            continue;
        }
        output.push(JavaParameter {
            name: node_text_field(child, source, "name").unwrap_or_default(),
            type_name: child
                .child_by_field_name("type")
                .and_then(|node| node_text(node, source))
                .unwrap_or_default(),
        });
    }
    output
}

fn collect_fields(body: Node<'_>, source: &[u8]) -> Vec<JavaField> {
    let mut fields = Vec::new();
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "field_declaration" {
            continue;
        }
        let modifier_text = modifier_text(child, source);
        let type_name = child
            .child_by_field_name("type")
            .and_then(|node| node_text(node, source))
            .unwrap_or_default();
        let mut field_cursor = child.walk();
        for declarator in child.named_children(&mut field_cursor) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            fields.push(JavaField {
                name: node_text_field(declarator, source, "name").unwrap_or_default(),
                visibility: visibility(&modifier_text),
                type_name: type_name.clone(),
                annotations: annotations(&modifier_text),
                modifiers: modifiers(&modifier_text),
            });
        }
    }
    fields
}

fn java_deltas(path: &Path, before: Option<&JavaFile>, after: Option<&JavaFile>) -> Vec<JavaDelta> {
    let mut deltas = Vec::new();
    let before_classes = before.map(flat_classes).unwrap_or_default();
    let after_classes = after.map(flat_classes).unwrap_or_default();
    for (class_name, before_class) in &before_classes {
        let Some(after_class) = after_classes.get(class_name) else {
            deltas.push(delta(path, "class_removed", class_name));
            continue;
        };
        compare_class(path, before_class, after_class, &mut deltas);
    }
    deltas
}

fn compare_class(path: &Path, before: &JavaClass, after: &JavaClass, deltas: &mut Vec<JavaDelta>) {
    if visibility_rank(&after.visibility) < visibility_rank(&before.visibility) {
        deltas.push(delta(path, "visibility_narrowed", &before.full_name));
    }
    if before.superclass != after.superclass {
        deltas.push(delta(path, "superclass_changed", &before.full_name));
    }
    for interface in &before.interfaces {
        if !after.interfaces.contains(interface) {
            deltas.push(delta(
                path,
                "interface_removed",
                &format!("{}:{interface}", before.full_name),
            ));
        }
    }

    let before_methods = method_map(&before.methods, &before.full_name);
    let after_methods = method_map(&after.methods, &after.full_name);
    for (symbol, before_method) in before_methods {
        let Some(after_method) = after_methods.get(&symbol) else {
            deltas.push(delta(path, "method_removed", &symbol));
            continue;
        };
        if visibility_rank(&after_method.visibility) < visibility_rank(&before_method.visibility) {
            deltas.push(delta(path, "visibility_narrowed", &symbol));
        }
    }

    let before_fields = field_map(&before.fields, &before.full_name);
    let after_fields = field_map(&after.fields, &after.full_name);
    for (symbol, before_field) in before_fields {
        let Some(after_field) = after_fields.get(&symbol) else {
            deltas.push(delta(path, "field_removed", &symbol));
            continue;
        };
        if before_field.type_name != after_field.type_name {
            deltas.push(delta(path, "field_type_changed", &symbol));
        }
        if visibility_rank(&after_field.visibility) < visibility_rank(&before_field.visibility) {
            deltas.push(delta(path, "visibility_narrowed", &symbol));
        }
    }
}

fn flat_classes(file: &JavaFile) -> BTreeMap<String, JavaClass> {
    let mut output = BTreeMap::new();
    for class in &file.classes {
        insert_class(class, &mut output);
    }
    output
}

fn insert_class(class: &JavaClass, output: &mut BTreeMap<String, JavaClass>) {
    output.insert(class.full_name.clone(), class.clone());
    for inner in &class.inner_classes {
        insert_class(inner, output);
    }
}

fn method_map(methods: &[JavaMethod], class_name: &str) -> BTreeMap<String, JavaMethod> {
    methods
        .iter()
        .map(|method| (method_symbol(class_name, method), method.clone()))
        .collect()
}

fn field_map(fields: &[JavaField], class_name: &str) -> BTreeMap<String, JavaField> {
    fields
        .iter()
        .map(|field| (format!("{class_name}.{}", field.name), field.clone()))
        .collect()
}

fn method_symbol(class_name: &str, method: &JavaMethod) -> String {
    let parameters = method
        .parameters
        .iter()
        .map(|parameter| parameter.type_name.as_str())
        .collect::<Vec<_>>()
        .join(",");
    format!("{class_name}.{}({parameters})", method.name)
}

fn delta(path: &Path, kind: &str, symbol: &str) -> JavaDelta {
    JavaDelta {
        kind: kind.to_owned(),
        path: path.to_path_buf(),
        symbol: symbol.to_owned(),
    }
}

fn is_class_node(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration"
    )
}

fn modifier_text(node: Node<'_>, source: &[u8]) -> String {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "modifiers" {
            return node_text(child, source).unwrap_or_default();
        }
    }
    String::new()
}

fn visibility(modifiers: &str) -> String {
    if modifier_words(modifiers).contains("public") {
        "public"
    } else if modifier_words(modifiers).contains("protected") {
        "protected"
    } else if modifier_words(modifiers).contains("private") {
        "private"
    } else {
        "package"
    }
    .to_owned()
}

fn modifiers(modifier_text: &str) -> Vec<String> {
    modifier_words(modifier_text)
        .into_iter()
        .filter(|word| {
            matches!(
                *word,
                "abstract" | "final" | "static" | "sealed" | "non-sealed" | "strictfp" | "synchronized" | "native"
            )
        })
        .map(ToOwned::to_owned)
        .collect()
}

fn annotations(modifier_text: &str) -> Vec<JavaAnnotation> {
    modifier_text
        .split('@')
        .skip(1)
        .filter_map(|segment| {
            let name = segment
                .trim_start()
                .split(|ch: char| !(ch == '_' || ch == '.' || ch.is_ascii_alphanumeric()))
                .next()
                .unwrap_or_default();
            (!name.is_empty()).then(|| JavaAnnotation {
                name: name.rsplit('.').next().unwrap_or(name).to_owned(),
            })
        })
        .collect()
}

fn modifier_words(modifier_text: &str) -> BTreeSet<&str> {
    modifier_text
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-')
        .filter(|word| !word.is_empty())
        .collect()
}

fn visibility_rank(value: &str) -> u8 {
    match value {
        "public" => 3,
        "protected" => 2,
        "package" => 1,
        "private" => 0,
        _ => 0,
    }
}

fn superclass(node: Node<'_>, source: &[u8]) -> Option<String> {
    node.child_by_field_name("superclass").and_then(|superclass| {
        let texts = named_child_texts(superclass, source);
        texts.last().cloned()
    })
}

fn interfaces(node: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut output = Vec::new();
    for field in ["interfaces", "super_interfaces"] {
        if let Some(interfaces) = node.child_by_field_name(field) {
            collect_type_names(interfaces, source, &mut output);
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(child.kind(), "extends_interfaces" | "implements_interfaces") {
            collect_type_names(child, source, &mut output);
        }
    }
    output.sort();
    output.dedup();
    output
}

fn collect_type_names(node: Node<'_>, source: &[u8], output: &mut Vec<String>) {
    if matches!(
        node.kind(),
        "type_identifier" | "scoped_type_identifier" | "generic_type" | "identifier"
    ) {
        if let Some(text) = node_text(node, source) {
            output.push(text);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_type_names(child, source, output);
    }
}

fn node_text_field(node: Node<'_>, source: &[u8], field: &str) -> Option<String> {
    node.child_by_field_name(field).and_then(|node| node_text(node, source))
}

fn node_text(node: Node<'_>, source: &[u8]) -> Option<String> {
    node.utf8_text(source).ok().map(|text| text.trim().to_owned())
}

fn named_child_texts(node: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut output = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(text) = node_text(child, source) {
            output.push(text);
        }
    }
    output
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

fn is_java_path(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("java")
}

fn alloc_file_pair<'v>(heap: Heap<'v>, pair: &JavaFilePair) -> Value<'v> {
    let before = pair
        .before
        .as_ref()
        .map_or_else(Value::new_none, |file| alloc_java_file(heap, file));
    let after = pair
        .after
        .as_ref()
        .map_or_else(Value::new_none, |file| alloc_java_file(heap, file));
    heap.alloc(AllocStruct([
        ("path", heap.alloc(pair.path.to_string_lossy().to_string())),
        ("before", before),
        ("after", after),
        ("change_kind", heap.alloc(change_kind_name(pair.change_kind))),
    ]))
}

fn alloc_java_file<'v>(heap: Heap<'v>, file: &JavaFile) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("package", heap.alloc(file.package.clone())),
        ("imports", heap.alloc(file.imports.clone())),
        (
            "classes",
            heap.alloc(
                file.classes
                    .iter()
                    .map(|class| alloc_class(heap, class))
                    .collect::<Vec<_>>(),
            ),
        ),
    ]))
}

fn alloc_class<'v>(heap: Heap<'v>, class: &JavaClass) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("name", heap.alloc(class.name.clone())),
        ("full_name", heap.alloc(class.full_name.clone())),
        ("visibility", heap.alloc(class.visibility.clone())),
        ("modifiers", heap.alloc(class.modifiers.clone())),
        (
            "superclass",
            class
                .superclass
                .as_ref()
                .map_or_else(Value::new_none, |value| heap.alloc(value.clone())),
        ),
        ("interfaces", heap.alloc(class.interfaces.clone())),
        (
            "annotations",
            heap.alloc(
                class
                    .annotations
                    .iter()
                    .map(|annotation| alloc_annotation(heap, annotation))
                    .collect::<Vec<_>>(),
            ),
        ),
        (
            "methods",
            heap.alloc(
                class
                    .methods
                    .iter()
                    .map(|method| alloc_method(heap, method))
                    .collect::<Vec<_>>(),
            ),
        ),
        (
            "fields",
            heap.alloc(
                class
                    .fields
                    .iter()
                    .map(|field| alloc_field(heap, field))
                    .collect::<Vec<_>>(),
            ),
        ),
        (
            "inner_classes",
            heap.alloc(
                class
                    .inner_classes
                    .iter()
                    .map(|inner| alloc_class(heap, inner))
                    .collect::<Vec<_>>(),
            ),
        ),
    ]))
}

fn alloc_annotation<'v>(heap: Heap<'v>, annotation: &JavaAnnotation) -> Value<'v> {
    heap.alloc(AllocStruct([("name", heap.alloc(annotation.name.clone()))]))
}

fn alloc_method<'v>(heap: Heap<'v>, method: &JavaMethod) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("name", heap.alloc(method.name.clone())),
        ("visibility", heap.alloc(method.visibility.clone())),
        ("return_type", heap.alloc(method.return_type.clone())),
        (
            "parameters",
            heap.alloc(
                method
                    .parameters
                    .iter()
                    .map(|parameter| alloc_parameter(heap, parameter))
                    .collect::<Vec<_>>(),
            ),
        ),
        (
            "annotations",
            heap.alloc(
                method
                    .annotations
                    .iter()
                    .map(|annotation| alloc_annotation(heap, annotation))
                    .collect::<Vec<_>>(),
            ),
        ),
        ("modifiers", heap.alloc(method.modifiers.clone())),
    ]))
}

fn alloc_parameter<'v>(heap: Heap<'v>, parameter: &JavaParameter) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("name", heap.alloc(parameter.name.clone())),
        ("type_name", heap.alloc(parameter.type_name.clone())),
    ]))
}

fn alloc_field<'v>(heap: Heap<'v>, field: &JavaField) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("name", heap.alloc(field.name.clone())),
        ("visibility", heap.alloc(field.visibility.clone())),
        ("type_name", heap.alloc(field.type_name.clone())),
        (
            "annotations",
            heap.alloc(
                field
                    .annotations
                    .iter()
                    .map(|annotation| alloc_annotation(heap, annotation))
                    .collect::<Vec<_>>(),
            ),
        ),
        ("modifiers", heap.alloc(field.modifiers.clone())),
    ]))
}

fn alloc_delta<'v>(heap: Heap<'v>, delta: &JavaDelta) -> Value<'v> {
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
