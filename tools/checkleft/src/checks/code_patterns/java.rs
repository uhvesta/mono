use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use tree_sitter::{Node, Parser};

use crate::output::{Finding, Location};

use super::config::CompiledNoCallRule;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceLocation {
    line: u32,
    column: u32,
}

pub(super) fn analyze_java_file(
    path: &Path,
    contents: &str,
    rules: &[CompiledNoCallRule],
) -> Vec<Finding> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }

    let Some(tree) = parser.parse(contents, None) else {
        return Vec::new();
    };
    if tree.root_node().has_error() {
        return Vec::new();
    }

    let source = contents.as_bytes();
    let model = JavaFileModel::collect(tree.root_node(), source);
    let matches = collect_java_matches(tree.root_node(), source, &model, rules);

    matches
        .into_iter()
        .map(|matched| Finding {
            severity: matched.rule.severity,
            message: matched.rule.message.clone().unwrap_or_else(|| {
                format!("Disallowed call to {}.", matched.rule.pattern.render())
            }),
            location: Some(Location {
                path: path.to_path_buf(),
                line: Some(matched.location.line),
                column: Some(matched.location.column),
            }),
            remediations: matched.rule.remediation.iter().cloned().collect(),
            suggested_fix: None,
        })
        .collect()
}

#[derive(Debug)]
struct JavaFileModel {
    package_name: Option<String>,
    imports_by_simple: HashMap<String, String>,
    declared_types: BTreeMap<String, JavaTypeDecl>,
    declared_types_by_simple: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct JavaTypeDecl {
    fields: HashMap<String, String>,
    methods: Vec<JavaMethodDecl>,
    super_types: Vec<String>,
}

#[derive(Debug, Clone)]
struct JavaMethodDecl {
    name: String,
    arity: usize,
    return_type: Option<String>,
}

impl JavaFileModel {
    fn collect(root: Node<'_>, source: &[u8]) -> Self {
        let package_name = find_package_name(root, source);
        let mut model = Self {
            package_name,
            imports_by_simple: find_imports(root, source),
            declared_types: BTreeMap::new(),
            declared_types_by_simple: HashMap::new(),
        };

        collect_type_declarations(root, source, &mut model, None);
        model
    }

    fn resolve_type_text(&self, raw: &str) -> Option<String> {
        let normalized = normalize_type_text(raw)?;
        if normalized.contains('.') {
            return Some(normalized);
        }
        if let Some(imported) = self.imports_by_simple.get(&normalized) {
            return Some(imported.clone());
        }
        if let Some(declared) = self.declared_types_by_simple.get(&normalized) {
            return Some(declared.clone());
        }
        if let Some(known) = known_lang_type(&normalized) {
            return Some(known.to_owned());
        }
        self.package_name
            .as_ref()
            .map(|package| format!("{package}.{normalized}"))
            .or(Some(normalized))
    }
}

fn find_package_name(root: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "package_declaration" {
            continue;
        }
        return node_named_children_text(child, source)
            .last()
            .map(|value| (*value).to_owned());
    }
    None
}

fn find_imports(root: Node<'_>, source: &[u8]) -> HashMap<String, String> {
    let mut imports = HashMap::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "import_declaration" {
            continue;
        }
        let text = child
            .utf8_text(source)
            .ok()
            .map(str::trim)
            .unwrap_or_default();
        if text.contains(" static ") {
            continue;
        }
        let segments = node_named_children_text(child, source);
        if segments.iter().any(|segment| *segment == "*") {
            continue;
        }
        let Some(full_path) = segments.last() else {
            continue;
        };
        let Some(simple) = full_path.rsplit('.').next() else {
            continue;
        };
        imports.insert(simple.to_owned(), (*full_path).to_owned());
    }
    imports
}

fn collect_type_declarations(
    node: Node<'_>,
    source: &[u8],
    model: &mut JavaFileModel,
    enclosing_type: Option<&str>,
) {
    if matches!(node.kind(), "class_declaration" | "interface_declaration") {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let Ok(simple_name) = name_node.utf8_text(source) else {
            return;
        };
        let fqcn = match enclosing_type {
            Some(enclosing) => format!("{enclosing}.{simple_name}"),
            None => model
                .package_name
                .as_ref()
                .map(|package| format!("{package}.{simple_name}"))
                .unwrap_or_else(|| simple_name.to_owned()),
        };

        let super_types = collect_declared_supertypes(node, source, model);
        let fields = collect_declared_fields(node, source, model);
        let methods = collect_declared_methods(node, source, model);
        model
            .declared_types_by_simple
            .entry(simple_name.to_owned())
            .or_insert_with(|| fqcn.clone());
        model.declared_types.insert(
            fqcn.clone(),
            JavaTypeDecl {
                fields,
                methods,
                super_types,
            },
        );

        if let Some(body) = node.child_by_field_name("body") {
            let mut cursor = body.walk();
            for child in body.named_children(&mut cursor) {
                collect_type_declarations(child, source, model, Some(&fqcn));
            }
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_type_declarations(child, source, model, enclosing_type);
    }
}

fn collect_declared_supertypes(
    type_node: Node<'_>,
    source: &[u8],
    model: &JavaFileModel,
) -> Vec<String> {
    let mut output = Vec::new();
    if let Some(superclass) = type_node.child_by_field_name("superclass") {
        let mut cursor = superclass.walk();
        for child in superclass.named_children(&mut cursor) {
            if let Some(resolved) = resolve_type_node_text(child, source, model) {
                output.push(resolved);
            }
        }
    }
    if let Some(interfaces) = type_node.child_by_field_name("interfaces") {
        let mut cursor = interfaces.walk();
        for child in interfaces.named_children(&mut cursor) {
            if child.kind() == "type_list" {
                let mut type_cursor = child.walk();
                for listed in child.named_children(&mut type_cursor) {
                    if let Some(resolved) = resolve_type_node_text(listed, source, model) {
                        output.push(resolved);
                    }
                }
            } else if let Some(resolved) = resolve_type_node_text(child, source, model) {
                output.push(resolved);
            }
        }
    }
    if type_node.kind() == "interface_declaration" {
        let mut cursor = type_node.walk();
        for child in type_node.named_children(&mut cursor) {
            if child.kind() != "extends_interfaces" {
                continue;
            }
            let mut extends_cursor = child.walk();
            for listed in child.named_children(&mut extends_cursor) {
                if listed.kind() == "type_list" {
                    let mut type_cursor = listed.walk();
                    for item in listed.named_children(&mut type_cursor) {
                        if let Some(resolved) = resolve_type_node_text(item, source, model) {
                            output.push(resolved);
                        }
                    }
                }
            }
        }
    }
    output
}

fn collect_declared_fields(
    type_node: Node<'_>,
    source: &[u8],
    model: &JavaFileModel,
) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    let Some(body) = type_node.child_by_field_name("body") else {
        return fields;
    };

    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "field_declaration" {
            continue;
        }
        let Some(type_node) = child.child_by_field_name("type") else {
            continue;
        };
        let Some(field_type) = resolve_type_node_text(type_node, source, model) else {
            continue;
        };

        let mut field_cursor = child.walk();
        for declarator in child.named_children(&mut field_cursor) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            let Some(name_node) = declarator.child_by_field_name("name") else {
                continue;
            };
            if let Ok(name) = name_node.utf8_text(source) {
                fields.insert(name.to_owned(), field_type.clone());
            }
        }
    }
    fields
}

fn collect_declared_methods(
    type_node: Node<'_>,
    source: &[u8],
    model: &JavaFileModel,
) -> Vec<JavaMethodDecl> {
    let mut methods = Vec::new();
    let Some(body) = type_node.child_by_field_name("body") else {
        return methods;
    };

    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "method_declaration" {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(source) else {
            continue;
        };
        let Some(parameters) = child.child_by_field_name("parameters") else {
            continue;
        };
        let return_type = child
            .child_by_field_name("type")
            .and_then(|node| resolve_type_node_text(node, source, model));

        methods.push(JavaMethodDecl {
            name: name.to_owned(),
            arity: parameter_count(parameters),
            return_type,
        });
    }
    methods
}

fn resolve_type_node_text(node: Node<'_>, source: &[u8], model: &JavaFileModel) -> Option<String> {
    let raw = node.utf8_text(source).ok()?;
    model.resolve_type_text(raw)
}

fn node_named_children_text<'a>(node: Node<'a>, source: &'a [u8]) -> Vec<&'a str> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter_map(|child| child.utf8_text(source).ok())
        .collect()
}

fn normalize_type_text(raw: &str) -> Option<String> {
    let mut result = String::new();
    let mut generic_depth = 0usize;
    for ch in raw.chars() {
        match ch {
            '<' => generic_depth = generic_depth.saturating_add(1),
            '>' => generic_depth = generic_depth.saturating_sub(1),
            _ if generic_depth > 0 => {}
            _ if ch.is_whitespace() => {}
            _ => result.push(ch),
        }
    }
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

fn known_lang_type(simple: &str) -> Option<&'static str> {
    match simple {
        "Appendable" => Some("java.lang.Appendable"),
        "ArithmeticException" => Some("java.lang.ArithmeticException"),
        "AutoCloseable" => Some("java.lang.AutoCloseable"),
        "Boolean" => Some("java.lang.Boolean"),
        "Byte" => Some("java.lang.Byte"),
        "CharSequence" => Some("java.lang.CharSequence"),
        "Character" => Some("java.lang.Character"),
        "Class" => Some("java.lang.Class"),
        "ClassCastException" => Some("java.lang.ClassCastException"),
        "ClassLoader" => Some("java.lang.ClassLoader"),
        "CloneNotSupportedException" => Some("java.lang.CloneNotSupportedException"),
        "Cloneable" => Some("java.lang.Cloneable"),
        "Comparable" => Some("java.lang.Comparable"),
        "Double" => Some("java.lang.Double"),
        "Enum" => Some("java.lang.Enum"),
        "Error" => Some("java.lang.Error"),
        "Exception" => Some("java.lang.Exception"),
        "Float" => Some("java.lang.Float"),
        "IllegalArgumentException" => Some("java.lang.IllegalArgumentException"),
        "IllegalStateException" => Some("java.lang.IllegalStateException"),
        "IndexOutOfBoundsException" => Some("java.lang.IndexOutOfBoundsException"),
        "Integer" => Some("java.lang.Integer"),
        "InterruptedException" => Some("java.lang.InterruptedException"),
        "Iterable" => Some("java.lang.Iterable"),
        "Long" => Some("java.lang.Long"),
        "Math" => Some("java.lang.Math"),
        "NullPointerException" => Some("java.lang.NullPointerException"),
        "Number" => Some("java.lang.Number"),
        "NumberFormatException" => Some("java.lang.NumberFormatException"),
        "Object" => Some("java.lang.Object"),
        "Override" => Some("java.lang.Override"),
        "Readable" => Some("java.lang.Readable"),
        "Record" => Some("java.lang.Record"),
        "Runnable" => Some("java.lang.Runnable"),
        "RuntimeException" => Some("java.lang.RuntimeException"),
        "Short" => Some("java.lang.Short"),
        "StackTraceElement" => Some("java.lang.StackTraceElement"),
        "String" => Some("java.lang.String"),
        "StringBuffer" => Some("java.lang.StringBuffer"),
        "StringBuilder" => Some("java.lang.StringBuilder"),
        "SuppressWarnings" => Some("java.lang.SuppressWarnings"),
        "System" => Some("java.lang.System"),
        "Thread" => Some("java.lang.Thread"),
        "Throwable" => Some("java.lang.Throwable"),
        "UnsupportedOperationException" => Some("java.lang.UnsupportedOperationException"),
        "Void" => Some("java.lang.Void"),
        _ => None,
    }
}

#[derive(Debug)]
struct MatchedRule<'a> {
    location: SourceLocation,
    rule: &'a CompiledNoCallRule,
}

fn collect_java_matches<'a>(
    root: Node<'_>,
    source: &'a [u8],
    model: &'a JavaFileModel,
    rules: &'a [CompiledNoCallRule],
) -> Vec<MatchedRule<'a>> {
    let mut ctx = JavaTraversalContext {
        source,
        model,
        current_type: None,
        scopes: Vec::new(),
        findings: Vec::new(),
    };
    walk_java(root, &mut ctx, rules);
    ctx.findings
}

struct JavaTraversalContext<'a> {
    source: &'a [u8],
    model: &'a JavaFileModel,
    current_type: Option<String>,
    scopes: Vec<HashMap<String, String>>,
    findings: Vec<MatchedRule<'a>>,
}

impl<'a> JavaTraversalContext<'a> {
    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn declare_variable(&mut self, name: &str, ty: String) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_owned(), ty);
        }
    }

    fn assign_variable(&mut self, name: &str, ty: String) {
        for scope in self.scopes.iter_mut().rev() {
            if scope.contains_key(name) {
                scope.insert(name.to_owned(), ty);
                return;
            }
        }
    }

    fn lookup_variable_type(&self, name: &str) -> Option<String> {
        for scope in self.scopes.iter().rev() {
            if let Some(ty) = scope.get(name) {
                return Some(ty.clone());
            }
        }
        self.lookup_field_type(self.current_type.as_deref()?, name)
    }

    fn lookup_field_type(&self, owner_type: &str, field_name: &str) -> Option<String> {
        let mut queue = vec![owner_type.to_owned()];
        let mut seen = HashSet::new();
        while let Some(current) = queue.pop() {
            if !seen.insert(current.clone()) {
                continue;
            }

            if let Some(decl) = self.model.declared_types.get(&current) {
                if let Some(field_type) = decl.fields.get(field_name) {
                    return Some(field_type.clone());
                }
                queue.extend(decl.super_types.iter().cloned());
            }
            queue.extend(
                known_direct_supertypes(&current)
                    .iter()
                    .map(|value| (*value).to_owned()),
            );
        }
        None
    }

    fn lookup_method_return_type(
        &self,
        owner_type: &str,
        method_name: &str,
        arity: usize,
    ) -> Option<String> {
        let mut queue = vec![owner_type.to_owned()];
        let mut seen = HashSet::new();
        while let Some(current) = queue.pop() {
            if !seen.insert(current.clone()) {
                continue;
            }

            if let Some(decl) = self.model.declared_types.get(&current) {
                for method in &decl.methods {
                    if method.name == method_name && method.arity == arity {
                        return method.return_type.clone();
                    }
                }
                queue.extend(decl.super_types.iter().cloned());
            }
            queue.extend(
                known_direct_supertypes(&current)
                    .iter()
                    .map(|value| (*value).to_owned()),
            );
        }
        None
    }
}

fn walk_java<'a>(
    node: Node<'_>,
    ctx: &mut JavaTraversalContext<'a>,
    rules: &'a [CompiledNoCallRule],
) {
    match node.kind() {
        "class_declaration" | "interface_declaration" => {
            let previous_type = ctx.current_type.clone();
            if let Some(name_node) = node.child_by_field_name("name")
                && let Ok(simple_name) = name_node.utf8_text(ctx.source) {
                    let next_type = previous_type
                        .as_ref()
                        .map(|parent| format!("{parent}.{simple_name}"))
                        .or_else(|| {
                            ctx.model
                                .package_name
                                .as_ref()
                                .map(|package| format!("{package}.{simple_name}"))
                        })
                        .unwrap_or_else(|| simple_name.to_owned());
                    ctx.current_type = Some(next_type);
                }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk_java(child, ctx, rules);
            }
            ctx.current_type = previous_type;
        }
        "method_declaration" => {
            ctx.push_scope();
            if let Some(parameters) = node.child_by_field_name("parameters") {
                bind_parameters(parameters, ctx);
            }
            if let Some(body) = node.child_by_field_name("body") {
                walk_java(body, ctx, rules);
            }
            ctx.pop_scope();
        }
        "constructor_declaration" => {
            ctx.push_scope();
            if let Some(parameters) = node.child_by_field_name("parameters") {
                bind_parameters(parameters, ctx);
            }
            if let Some(body) = node.child_by_field_name("body") {
                walk_java(body, ctx, rules);
            }
            ctx.pop_scope();
        }
        "block" => {
            ctx.push_scope();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk_java(child, ctx, rules);
            }
            ctx.pop_scope();
        }
        "local_variable_declaration" => {
            bind_local_variables(node, ctx);
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk_java(child, ctx, rules);
            }
        }
        "assignment_expression" => {
            bind_assignment(node, ctx);
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk_java(child, ctx, rules);
            }
        }
        "method_invocation" => {
            inspect_method_invocation(node, ctx, rules);
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk_java(child, ctx, rules);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk_java(child, ctx, rules);
            }
        }
    }
}

fn bind_parameters(node: Node<'_>, ctx: &mut JavaTraversalContext<'_>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "formal_parameter" => {
                let Some(name_node) = child.child_by_field_name("name") else {
                    continue;
                };
                let Some(type_node) = child.child_by_field_name("type") else {
                    continue;
                };
                let (Ok(name), Some(param_type)) = (
                    name_node.utf8_text(ctx.source),
                    resolve_type_node_text(type_node, ctx.source, ctx.model),
                ) else {
                    continue;
                };
                ctx.declare_variable(name, param_type);
            }
            "spread_parameter" => {
                let mut spread_cursor = child.walk();
                let mut pieces = child.named_children(&mut spread_cursor);
                let Some(type_node) = pieces.next() else {
                    continue;
                };
                let Some(declarator) = pieces.find(|node| node.kind() == "variable_declarator")
                else {
                    continue;
                };
                let Some(name_node) = declarator.child_by_field_name("name") else {
                    continue;
                };
                let (Ok(name), Some(param_type)) = (
                    name_node.utf8_text(ctx.source),
                    resolve_type_node_text(type_node, ctx.source, ctx.model),
                ) else {
                    continue;
                };
                ctx.declare_variable(name, param_type);
            }
            _ => {}
        }
    }
}

fn bind_local_variables(node: Node<'_>, ctx: &mut JavaTraversalContext<'_>) {
    let declared_type_node = node.child_by_field_name("type");
    let declared_type_raw = declared_type_node
        .and_then(|type_node| type_node.utf8_text(ctx.source).ok())
        .and_then(normalize_type_text);
    let declared_type = declared_type_node
        .and_then(|type_node| resolve_type_node_text(type_node, ctx.source, ctx.model));

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(ctx.source) else {
            continue;
        };

        let inferred_type = child
            .child_by_field_name("value")
            .and_then(|value| resolve_expression_type(value, ctx));

        let variable_type = match declared_type_raw.as_deref() {
            Some("var") => inferred_type,
            Some(_) => declared_type.clone(),
            None => inferred_type,
        };
        if let Some(variable_type) = variable_type {
            ctx.declare_variable(name, variable_type);
        }
    }
}

fn bind_assignment(node: Node<'_>, ctx: &mut JavaTraversalContext<'_>) {
    let Some(left) = node.child_by_field_name("left") else {
        return;
    };
    let Some(right) = first_named_child(node, "right").or_else(|| last_named_child(node)) else {
        return;
    };
    let Some(rhs_type) = resolve_expression_type(right, ctx) else {
        return;
    };

    match left.kind() {
        "identifier" => {
            if let Ok(name) = left.utf8_text(ctx.source) {
                ctx.assign_variable(name, rhs_type);
            }
        }
        "field_access" => {}
        _ => {}
    }
}

fn inspect_method_invocation<'a>(
    node: Node<'_>,
    ctx: &mut JavaTraversalContext<'a>,
    rules: &'a [CompiledNoCallRule],
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let Ok(method_name) = name_node.utf8_text(ctx.source) else {
        return;
    };
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return;
    };
    let arity = argument_count(arguments);
    let Some(receiver_type) = resolve_method_owner_type(node, ctx) else {
        return;
    };

    for rule in rules {
        if rule.pattern.method_name != method_name || rule.pattern.arity != arity {
            continue;
        }
        if !is_assignable_to(&receiver_type, &rule.pattern.receiver_type, ctx.model) {
            continue;
        }
        ctx.findings.push(MatchedRule {
            location: source_location(name_node),
            rule,
        });
    }
}

fn resolve_method_owner_type(node: Node<'_>, ctx: &JavaTraversalContext<'_>) -> Option<String> {
    let object = node.child_by_field_name("object");
    match object {
        Some(object) => resolve_expression_type(object, ctx),
        None => ctx.current_type.clone(),
    }
}

fn resolve_expression_type(node: Node<'_>, ctx: &JavaTraversalContext<'_>) -> Option<String> {
    match node.kind() {
        "identifier" => {
            let name = node.utf8_text(ctx.source).ok()?;
            ctx.lookup_variable_type(name)
        }
        "this" => ctx.current_type.clone(),
        "super" => ctx
            .current_type
            .as_deref()
            .and_then(|current| direct_super_types(current, ctx.model).into_iter().next()),
        "object_creation_expression" => node
            .child_by_field_name("type")
            .and_then(|type_node| resolve_type_node_text(type_node, ctx.source, ctx.model)),
        "parenthesized_expression" => first_named_child(node, "expression")
            .or_else(|| last_named_child(node))
            .and_then(|child| resolve_expression_type(child, ctx)),
        "cast_expression" => node
            .child_by_field_name("type")
            .and_then(|type_node| resolve_type_node_text(type_node, ctx.source, ctx.model)),
        "field_access" => {
            let object = node.child_by_field_name("object")?;
            let field = node.child_by_field_name("field")?;
            let owner_type = resolve_expression_type(object, ctx)?;
            let field_name = field.utf8_text(ctx.source).ok()?;
            ctx.lookup_field_type(&owner_type, field_name)
        }
        "method_invocation" => {
            let method_name = node
                .child_by_field_name("name")?
                .utf8_text(ctx.source)
                .ok()?;
            let arguments = node.child_by_field_name("arguments")?;
            let arity = argument_count(arguments);
            let owner_type = resolve_method_owner_type(node, ctx)?;
            ctx.lookup_method_return_type(&owner_type, method_name, arity)
        }
        _ => None,
    }
}

fn first_named_child<'a>(node: Node<'a>, field_name: &str) -> Option<Node<'a>> {
    node.child_by_field_name(field_name)
}

fn last_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).last()
}

fn parameter_count(node: Node<'_>) -> usize {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).count()
}

fn argument_count(node: Node<'_>) -> usize {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).count()
}

fn source_location(node: Node<'_>) -> SourceLocation {
    let position = node.start_position();
    SourceLocation {
        line: (position.row + 1) as u32,
        column: (position.column + 1) as u32,
    }
}

fn is_assignable_to(actual: &str, expected: &str, model: &JavaFileModel) -> bool {
    if actual == expected {
        return true;
    }

    let mut queue = vec![actual.to_owned()];
    let mut seen = HashSet::new();
    while let Some(current) = queue.pop() {
        if !seen.insert(current.clone()) {
            continue;
        }
        if current == expected {
            return true;
        }
        if let Some(decl) = model.declared_types.get(&current) {
            queue.extend(decl.super_types.iter().cloned());
        }
        queue.extend(
            known_direct_supertypes(&current)
                .iter()
                .map(|value| (*value).to_owned()),
        );
    }

    false
}

fn direct_super_types(actual: &str, model: &JavaFileModel) -> Vec<String> {
    let mut direct = Vec::new();
    if let Some(decl) = model.declared_types.get(actual) {
        direct.extend(decl.super_types.iter().cloned());
    }
    direct.extend(
        known_direct_supertypes(actual)
            .iter()
            .map(|value| (*value).to_owned()),
    );
    direct
}

fn known_direct_supertypes(actual: &str) -> &'static [&'static str] {
    match actual {
        "java.util.concurrent.CompletableFuture" => &[
            "java.util.concurrent.Future",
            "java.util.concurrent.CompletionStage",
            "java.lang.Object",
        ],
        "java.util.concurrent.FutureTask" => &[
            "java.util.concurrent.RunnableFuture",
            "java.util.concurrent.Future",
            "java.lang.Object",
        ],
        "java.util.concurrent.RunnableFuture" => &[
            "java.util.concurrent.Future",
            "java.lang.Runnable",
            "java.lang.Object",
        ],
        "java.util.concurrent.ScheduledFuture" => {
            &["java.util.concurrent.Future", "java.lang.Object"]
        }
        _ => &[],
    }
}
