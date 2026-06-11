use std::path::Path;

use tree_sitter::{Node, Parser, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StarlarkFileKind {
    Build,
    Module,
    Bzl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SourceLocation {
    pub(crate) line: u32,
    pub(crate) column: u32,
}

#[derive(Debug)]
pub(crate) struct ParsedStarlarkFile<'a> {
    tree: Tree,
    pub(crate) source: &'a [u8],
}

impl ParsedStarlarkFile<'_> {
    pub(crate) fn root(&self) -> Node<'_> {
        self.tree.root_node()
    }
}

pub(crate) fn starlark_file_kind(path: &Path) -> Option<StarlarkFileKind> {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("BUILD") | Some("BUILD.bazel") => Some(StarlarkFileKind::Build),
        Some("MODULE.bazel") => Some(StarlarkFileKind::Module),
        _ if matches!(path.extension().and_then(|ext| ext.to_str()), Some("bzl")) => Some(StarlarkFileKind::Bzl),
        _ => None,
    }
}

pub(crate) fn parse_starlark_file(contents: &str) -> Option<ParsedStarlarkFile<'_>> {
    let mut parser = Parser::new();
    if parser.set_language(&tree_sitter_starlark::LANGUAGE.into()).is_err() {
        return None;
    }
    let tree = parser.parse(contents, None)?;
    if tree.root_node().has_error() {
        return None;
    }
    Some(ParsedStarlarkFile {
        tree,
        source: contents.as_bytes(),
    })
}

pub(crate) fn normalize_callee(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => Some(node.utf8_text(source).ok()?.to_owned()),
        "attribute" => {
            let object = node.child_by_field_name("object")?;
            let attribute = node.child_by_field_name("attribute")?;
            let object = normalize_callee(object, source)?;
            let attribute = attribute.utf8_text(source).ok()?;
            Some(format!("{object}.{attribute}"))
        }
        _ => None,
    }
}

pub(crate) fn call_function_name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let function = node.child_by_field_name("function")?;
    if function.kind() != "identifier" {
        return None;
    }
    function.utf8_text(source).ok()
}

pub(crate) fn find_matching_string_literal<'a>(
    node: Node<'_>,
    source: &[u8],
    values: &'a [String],
) -> Option<(&'a str, SourceLocation)> {
    if node.kind() == "string" {
        let text = node.utf8_text(source).ok()?;
        let literal = unquote_starlark_string(text)?;
        if let Some(matched) = values.iter().find(|value| value.as_str() == literal) {
            return Some((matched.as_str(), source_location(node)));
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(result) = find_matching_string_literal(child, source, values) {
            return Some(result);
        }
    }
    None
}

pub(crate) fn source_location(node: Node<'_>) -> SourceLocation {
    let position = node.start_position();
    SourceLocation {
        line: (position.row + 1) as u32,
        column: (position.column + 1) as u32,
    }
}

fn unquote_starlark_string(raw: &str) -> Option<&str> {
    for prefix in ["r", "R", "rb", "rB", "Rb", "RB", "br", "bR", "Br", "BR", "b", "B"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            return unquote_starlark_string(rest);
        }
    }

    let bytes = raw.as_bytes();
    if bytes.len() < 2 {
        return None;
    }

    match (bytes.first(), bytes.last()) {
        (Some(b'"'), Some(b'"')) | (Some(b'\''), Some(b'\'')) => Some(&raw[1..raw.len() - 1]),
        _ => None,
    }
}
