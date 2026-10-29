use std::path::Path;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub(crate) struct Symbol { pub name: String, pub start: usize, pub end: usize, pub kind: String }

fn language(path: &Path) -> Option<tree_sitter::Language> {
    Some(match path.extension()?.to_str()? {
        "rs" => tree_sitter_rust::LANGUAGE.into(),
        "py" | "pyi" => tree_sitter_python::LANGUAGE.into(),
        "js" | "jsx" | "mjs" | "cjs" => tree_sitter_javascript::LANGUAGE.into(),
        "ts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "json" => tree_sitter_json::LANGUAGE.into(),
        _ => return None,
    })
}

fn tree(path: &Path, text: &str) -> Result<Option<tree_sitter::Tree>, String> {
    let Some(language) = language(path) else { return Ok(None) };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).map_err(|e| e.to_string())?;
    parser.parse(text, None).map(Some).ok_or_else(|| "parser cancelled".into())
}

pub(crate) fn errors(path: &Path, text: &str) -> Result<Option<Vec<String>>, String> {
    let Some(tree) = tree(path, text)? else { return Ok(None) };
    let mut errors = Vec::new();
    fn walk(node: tree_sitter::Node<'_>, errors: &mut Vec<String>) {
        if errors.len() >= 20 { return; }
        if node.is_error() || node.is_missing() {
            errors.push(format!("{}:{} {}{}", node.start_position().row + 1, node.start_position().column + 1, if node.is_missing() { "missing " } else { "error " }, node.kind()));
            return;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) { if child.has_error() || child.is_missing() { walk(child, errors); } }
    }
    walk(tree.root_node(), &mut errors);
    Ok(Some(errors))
}

pub(crate) fn symbols(path: &Path, text: &str) -> Result<Vec<Symbol>, String> {
    let tree = tree(path, text)?.ok_or_else(|| format!("symbol/outline unsupported for {}; supported: Rust, Python, JS, TS/TSX, JSON", path.display()))?;
    let mut symbols = Vec::new();
    fn visit(node: tree_sitter::Node<'_>, text: &str, scope: &str, out: &mut Vec<Symbol>) {
        let kind = node.kind();
        let is_symbol = matches!(kind, "function_item" | "function_definition" | "function_declaration" | "method_definition" | "method_signature" | "class_definition" | "class_declaration" | "struct_item" | "enum_item" | "trait_item" | "interface_declaration" | "type_alias_declaration" | "type_item" | "mod_item" | "impl_item");
        let name_node = node.child_by_field_name("name").or_else(|| if kind == "impl_item" { node.child_by_field_name("type") } else { None });
        let mut next_scope = scope.to_owned();
        if is_symbol && let Some(name_node) = name_node {
            let name = name_node.utf8_text(text.as_bytes()).unwrap_or("");
            next_scope = if scope.is_empty() { name.to_owned() } else { format!("{scope}.{name}") };
            out.push(Symbol { name: next_scope.clone(), start: node.start_position().row + 1, end: node.end_position().row + usize::from(node.end_position().column > 0), kind: kind.into() });
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) { visit(child, text, &next_scope, out); }
    }
    visit(tree.root_node(), text, "", &mut symbols);
    Ok(symbols)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn qualified_python_and_rust_symbols() {
        let s = symbols(Path::new("x.py"), "class Parser:\n    def parse_header(self):\n        pass\n").unwrap();
        assert!(s.iter().any(|s| s.name == "Parser.parse_header"));
        let s = symbols(Path::new("x.rs"), "struct Parser; impl Parser { fn parse_header() {} }").unwrap();
        assert!(s.iter().any(|s| s.name == "Parser.parse_header"));
        assert!(!errors(Path::new("x.rs"), "fn x( {").unwrap().unwrap().is_empty());
    }
}
