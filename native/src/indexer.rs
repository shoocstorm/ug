use crate::types::{
    Dependency, ExportInfo, FileClassification, FileNode, ImportedItem, ImportInfo, IndexResult, IndexStats,
    Param, Signature, Symbol, SymbolMetrics, TypeRef,
};
use ignore::WalkBuilder;
use napi_derive::napi;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use tree_sitter::{Node, Parser};

const SUPPORTED_EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "py"];
const IGNORED_DIRS: &[&str] = &["node_modules", ".git", "target"];

fn get_language_for_ext(ext: &str) -> Option<(tree_sitter::Language, &'static str)> {
    match ext {
        "ts" | "tsx" | "js" | "jsx" => {
            Some((tree_sitter_typescript::language_typescript(), "typescript"))
        }
        "py" => Some((tree_sitter_python::language(), "python")),
        _ => None,
    }
}

pub fn process_file(path: &Path) -> Option<FileNode> {
    let ext = path.extension()?.to_str()?;
    let (lang, lang_name) = get_language_for_ext(ext)?;

    let content = fs::read_to_string(path).ok()?;
    let hash_data = fs::read(path).ok()?;
    let hash = blake3::hash(&hash_data).to_hex().to_string();

    let mut parser = Parser::new();
    parser.set_language(lang).ok()?;

    let tree = parser.parse(content.as_bytes(), None)?;

    let file_imports = extract_file_imports(&tree.root_node(), content.as_bytes(), lang_name);
    let file_exports = extract_file_exports(&tree.root_node(), content.as_bytes(), lang_name);

    let mut symbols = Vec::new();
    let mut symbol_map: HashMap<String, &mut Symbol> = HashMap::new();

    extract_symbols(
        &mut symbols,
        &mut symbol_map,
        tree.root_node(),
        content.as_bytes(),
        lang_name,
    );

    let path_str = path.to_string_lossy().to_string();
    for sym in symbols.iter_mut() {
        sym.file = path_str.clone();
    }

    resolve_import_refs(&mut symbols, &file_imports);

    let classification = classify_file(&path_str, &symbols);

    Some(FileNode {
        path: path_str,
        hash,
        language: lang_name.to_string(),
        classification,
        symbols,
        imports: file_imports,
        exports: file_exports,
    })
}

fn extract_file_imports(node: &Node, source: &[u8], language: &str) -> Vec<ImportInfo> {
    let mut imports = Vec::new();
    let mut import_lookup: HashMap<String, ImportInfo> = HashMap::new();

    if language == "typescript" {
        let source_str = String::from_utf8(source.to_vec()).unwrap_or_default();
        
        let import_regex = regex::Regex::new(r#"import\s+(?:\{([^}]+)\}|\*\s+as\s+(\w+)|(\w+))\s+from\s+['"]([^'"]+)['"]"#).ok();
        if let Some(re) = import_regex {
            for cap in re.captures_iter(&source_str) {
                let names: Vec<ImportedItem> = {
                    if let Some(matched) = cap.get(1) {
                        matched.as_str()
                            .split(',')
                            .map(|s| {
                                let name = s.trim().split(" as ").next().unwrap_or(s.trim()).to_string();
                                ImportedItem { name, alias: None }
                            })
                            .collect()
                    } else if let Some(alias) = cap.get(2) {
                        vec![ImportedItem { name: alias.as_str().to_string(), alias: None }]
                    } else if let Some(name) = cap.get(3) {
                        vec![ImportedItem { name: name.as_str().to_string(), alias: None }]
                    } else {
                        vec![]
                    }
                };

                let path = cap.get(4).map(|m| m.as_str().to_string()).unwrap_or_default();
                if !path.is_empty() {
                    let is_external = !path.starts_with('.');
                    import_lookup
                        .entry(path.clone())
                        .and_modify(|info| info.imported.extend(names.clone()))
                        .or_insert_with(|| ImportInfo {
                            path,
                            imported: names,
                            is_external,
                        });
                }
            }
        }

        let import_type_regex = regex::Regex::new(r#"import\s+type\s+\{([^}]+)\}\s+from\s+['"]([^'"]+)['"]"#).ok();
        if let Some(re) = import_type_regex {
            for cap in re.captures_iter(&source_str) {
                let names: Vec<ImportedItem> = {
                    if let Some(matched) = cap.get(1) {
                        matched.as_str()
                            .split(',')
                            .map(|s| {
                                let name = s.trim().split(" as ").next().unwrap_or(s.trim()).to_string();
                                ImportedItem { name, alias: None }
                            })
                            .collect()
                    } else {
                        vec![]
                    }
                };

                let path = cap.get(2).map(|m| m.as_str().to_string()).unwrap_or_default();
                if !path.is_empty() {
                    let is_external = !path.starts_with('.');
                    import_lookup
                        .entry(path.clone())
                        .and_modify(|info| info.imported.extend(names.clone()))
                        .or_insert_with(|| ImportInfo {
                            path,
                            imported: names,
                            is_external,
                        });
                }
            }
        }
    } else if language == "python" {
        let source_str = String::from_utf8(source.to_vec()).unwrap_or_default();
        
        let from_import_re = regex::Regex::new(r#"from\s+(\.[^ ]+|[a-zA-Z_][a-zA-Z0-9_.]*)\s+import\s+(?:\(([^)]+)\)|([a-zA-Z_][a-zA-Z0-9_,\s]*))"#).ok();
        if let Some(re) = from_import_re {
            for cap in re.captures_iter(&source_str) {
                let path = cap.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
                let names: Vec<ImportedItem> = {
                    let names_str = cap.get(2).or(cap.get(3)).map(|m| m.as_str()).unwrap_or("*");
                    names_str
                        .split(',')
                        .map(|s| {
                            let name = s.trim().split(" as ").next().unwrap_or(s.trim()).to_string();
                            ImportedItem { name, alias: None }
                        })
                        .filter(|i| !i.name.is_empty())
                        .collect()
                };

                if !path.is_empty() {
                    import_lookup
                        .entry(path.clone())
                        .and_modify(|info| info.imported.extend(names.clone()))
                        .or_insert_with(|| ImportInfo {
                            path,
                            imported: names,
                            is_external: true,
                        });
                }
            }
        }

        let import_re = regex::Regex::new(r#"import\s+([a-zA-Z_][a-zA-Z0-9_.]*)"#).ok();
        if let Some(re) = import_re {
            for cap in re.captures_iter(&source_str) {
                let path = cap.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
                if !path.is_empty() && !path.contains("from") {
                    import_lookup
                        .entry(path.clone())
                        .or_insert_with(|| ImportInfo {
                            path: path.clone(),
                            imported: vec![ImportedItem {
                                name: path.split('.').last().unwrap_or(&path).to_string(),
                                alias: None,
                            }],
                            is_external: true,
                        });
                }
            }
        }
    }

    imports.extend(import_lookup.into_values());
    imports
}

fn extract_file_exports(node: &Node, source: &[u8], language: &str) -> Vec<ExportInfo> {
    let mut exports = Vec::new();

    if language == "typescript" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "export_clause" {
                let mut spec_cursor = child.walk();
                for spec in child.children(&mut spec_cursor) {
                    if spec.kind() == "export_specifier" {
                        let name = get_node_text(spec.child_by_field_name("name"), source)
                            .unwrap_or_default();
                        let alias = spec.child_by_field_name("alias").and_then(|n| {
                            get_node_text(Some(n), source)
                        });
                        exports.push(ExportInfo {
                            name,
                            alias,
                            is_default: false,
                        });
                    }
                }
            } else if child.kind() == "re_export_statement" || child.kind() == "export_statement" {
                if let Some(source_node) = child.child_by_field_name("source") {
                    let re_export_path = get_node_text(Some(source_node), source)
                        .unwrap_or_default();
                    if !re_export_path.is_empty() {
                        let mut spec_cursor = child.walk();
                        for spec in child.children(&mut spec_cursor) {
                            if spec.kind() == "export_specifier" {
                                let name = get_node_text(spec.child_by_field_name("name"), source)
                                    .unwrap_or_default();
                                let alias = spec.child_by_field_name("alias").and_then(|n| {
                                    get_node_text(Some(n), source)
                                });
                                exports.push(ExportInfo {
                                    name,
                                    alias,
                                    is_default: false,
                                });
                            }
                        }
                    }
                }
            }
        }
    } else if language == "python" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "import_statement" {
                continue;
            }
        }
    }

    exports
}

fn extract_symbols(
    symbols: &mut Vec<Symbol>,
    symbol_map: &mut HashMap<String, &mut Symbol>,
    node: Node,
    source: &[u8],
    language: &str,
) {
    let kind = node.kind();
    let start = (node.start_position().row + 1) as u32;
    let end = (node.end_position().row + 1) as u32;
    let start_byte = node.start_byte();
    let end_byte = node.end_byte();

    if language == "typescript" {
        match kind {
            "function_declaration" | "method_definition" => {
                if let Some(name) = get_node_text(node.child_by_field_name("name"), source) {
                    let params = extract_params(&node, source, "typescript");
                    let return_type = extract_return_type(&node, source);
                    let calls = extract_function_calls(&node, source);
                    let extends = extract_extends(&node, source);
                    let implements = extract_implements(&node, source);
                    let typed_as = extract_type_refs(&node, source);
                    let docstring = extract_docstring(&node, source);
                    let metrics = SymbolMetrics {
                        loc: end.saturating_sub(start),
                        params: params.len() as u32,
                        max_nesting: calculate_nesting(&node),
                    };

                    symbols.push(Symbol {
                        id: format!("fn:{}:{}", start, name),
                        name,
                        kind: kind.to_string(),
                        file: String::new(),
                        start_line: start,
                        end_line: end,
                        docstring,
                        signature: Some(Signature {
                            params,
                            return_type,
                        }),
                        imports: vec![],
                        exports: vec![],
                        extends,
                        implements,
                        calls,
                        typed_as,
                        metrics: Some(metrics),
                    });
                }
            }
            "class_declaration" => {
                if let Some(name) = get_node_text(node.child_by_field_name("name"), source) {
                    let extends = extract_extends(&node, source);
                    let implements = extract_implements(&node, source);
                    let typed_as = extract_type_refs(&node, source);
                    let docstring = extract_docstring(&node, source);

                    symbols.push(Symbol {
                        id: format!("class:{}:{}", start, name),
                        name,
                        kind: "class".to_string(),
                        file: String::new(),
                        start_line: start,
                        end_line: end,
                        docstring,
                        signature: None,
                        imports: vec![],
                        exports: vec![],
                        extends,
                        implements,
                        calls: vec![],
                        typed_as,
                        metrics: None,
                    });
                }
            }
            "interface_declaration" => {
                if let Some(name) = get_node_text(node.child_by_field_name("name"), source) {
                    let extends = extract_extends(&node, source);
                    let typed_as = extract_type_refs(&node, source);
                    let docstring = extract_docstring(&node, source);
                    let members = extract_interface_members(&node, source);

                    symbols.push(Symbol {
                        id: format!("interface:{}:{}", start, name),
                        name,
                        kind: "interface".to_string(),
                        file: String::new(),
                        start_line: start,
                        end_line: end,
                        docstring,
                        signature: None,
                        imports: vec![],
                        exports: vec![],
                        extends,
                        implements: vec![],
                        calls: vec![],
                        typed_as: members,
                        metrics: None,
                    });
                }
            }
            "variable_declaration" => {
                if let Some(name) = get_node_text(node.child_by_field_name("name"), source) {
                    let typed_as = extract_type_refs(&node, source);
                    let docstring = extract_docstring(&node, source);

                    symbols.push(Symbol {
                        id: format!("var:{}:{}", start, name),
                        name,
                        kind: "variable".to_string(),
                        file: String::new(),
                        start_line: start,
                        end_line: end,
                        docstring,
                        signature: None,
                        imports: vec![],
                        exports: vec![],
                        extends: vec![],
                        implements: vec![],
                        calls: vec![],
                        typed_as,
                        metrics: None,
                    });
                }
            }
            "type_alias_declaration" => {
                if let Some(name) = get_node_text(node.child_by_field_name("name"), source) {
                    let typed_as = extract_type_refs(&node, source);
                    let docstring = extract_docstring(&node, source);

                    symbols.push(Symbol {
                        id: format!("type:{}:{}", start, name),
                        name,
                        kind: "type".to_string(),
                        file: String::new(),
                        start_line: start,
                        end_line: end,
                        docstring,
                        signature: None,
                        imports: vec![],
                        exports: vec![],
                        extends: vec![],
                        implements: vec![],
                        calls: vec![],
                        typed_as,
                        metrics: None,
                    });
                }
            }
            _ => {}
        }
    } else if language == "python" {
        match kind {
            "function_definition" => {
                if let Some(name) = get_node_text(node.child_by_field_name("name"), source) {
                    let params = extract_params(&node, source, "python");
                    let return_type = extract_return_type(&node, source);
                    let calls = extract_function_calls(&node, source);
                    let docstring = extract_docstring(&node, source);
                    let metrics = SymbolMetrics {
                        loc: end.saturating_sub(start),
                        params: params.len() as u32,
                        max_nesting: calculate_nesting(&node),
                    };

                    symbols.push(Symbol {
                        id: format!("fn:{}:{}", start, name),
                        name,
                        kind: "function".to_string(),
                        file: String::new(),
                        start_line: start,
                        end_line: end,
                        docstring,
                        signature: Some(Signature {
                            params,
                            return_type,
                        }),
                        imports: vec![],
                        exports: vec![],
                        extends: vec![],
                        implements: vec![],
                        calls,
                        typed_as: vec![],
                        metrics: Some(metrics),
                    });
                }
            }
            "class_definition" => {
                if let Some(name) = get_node_text(node.child_by_field_name("name"), source) {
                    let extends = extract_python_extends(&node, source);
                    let typed_as = extract_type_refs(&node, source);
                    let docstring = extract_docstring(&node, source);

                    symbols.push(Symbol {
                        id: format!("class:{}:{}", start, name),
                        name,
                        kind: "class".to_string(),
                        file: String::new(),
                        start_line: start,
                        end_line: end,
                        docstring,
                        signature: None,
                        imports: vec![],
                        exports: vec![],
                        extends,
                        implements: vec![],
                        calls: vec![],
                        typed_as,
                        metrics: None,
                    });
                }
            }
            "assignment" => {
                let typed_as = extract_type_refs(&node, source);
                if let Some(target) = node.child_by_field_name("target") {
                    let name = get_node_text(Some(target), source);
                    if let Some(n) = name {
                        symbols.push(Symbol {
                            id: format!("assign:{}:{}", start, n),
                            name: n,
                            kind: "assignment".to_string(),
                            file: String::new(),
                            start_line: start,
                            end_line: end,
                            docstring: None,
                            signature: None,
                            imports: vec![],
                            exports: vec![],
                            extends: vec![],
                            implements: vec![],
                            calls: vec![],
                            typed_as,
                            metrics: None,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        extract_symbols(symbols, symbol_map, child, source, language);
    }
}

fn extract_params(node: &Node, source: &[u8], language: &str) -> Vec<Param> {
    let mut params = Vec::new();

    if let Some(params_node) = node.child_by_field_name("parameters") {
        let mut cursor = params_node.walk();
        for child in params_node.children(&mut cursor) {
            if language == "typescript" {
                if child.kind() == "required_parameter"
                    || child.kind() == "optional_parameter"
                    || child.kind() == "rest_parameter"
                {
                    let name = get_node_text(child.child_by_field_name("name"), source)
                        .unwrap_or_default();
                    if name.is_empty() {
                        continue;
                    }
                    let param_type = get_node_text(child.child_by_field_name("type"), source);
                    let optional = child.kind() == "optional_parameter";
                    let default = get_node_text(child.child_by_field_name("default"), source);

                    params.push(Param {
                        name,
                        param_type,
                        optional,
                        default,
                    });
                }
            } else if language == "python" {
                if child.kind() == "parameter" {
                    let name = get_node_text(child.child_by_field_name("name"), source)
                        .unwrap_or_default();
                    if name.is_empty() {
                        continue;
                    }
                    let default = get_node_text(child.child_by_field_name("default"), source);
                    let optional = default.is_some();

                    params.push(Param {
                        name,
                        param_type: None,
                        optional,
                        default,
                    });
                }
            }
        }
    }

if params.is_empty() {
        if let Some(node_text) = get_node_text(Some(*node), source) {
            if let Some(start) = node_text.find('(') {
                if let Some(end) = node_text[start..].find(')') {
                    let args = &node_text[start + 1..start + end];
                    let param_re = regex::Regex::new(r"(\w+)\s*(?::\s*([^\s,=]+))?").ok();
                    if let Some(re) = param_re {
                        for cap in re.captures_iter(args.trim()) {
                            if let Some(name_match) = cap.get(1) {
                                let name = name_match.as_str().to_string();
                                if name.is_empty() || name == "function" || name == "class" || name == "interface" {
                                    continue;
                                }
                                let param_type = cap.get(2).map(|m| m.as_str().to_string());
                                params.push(Param {
                                    name,
                                    param_type,
                                    optional: false,
                                    default: None,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    params
}

fn extract_return_type(node: &Node, source: &[u8]) -> Option<String> {
    if let Some(return_type) = node.child_by_field_name("return_type") {
        if let Some(text) = get_node_text(Some(return_type), source) {
            return Some(text.trim_start_matches(':').trim().to_string());
        }
    }

    if let Some(node_text) = get_node_text(Some(*node), source) {
        let return_re = regex::Regex::new(r"\)\s*:\s*([^\s{]+)").ok();
        if let Some(re) = return_re {
            if let Some(cap) = re.captures(&node_text) {
                if let Some(return_match) = cap.get(1) {
                    let return_type = return_match.as_str().to_string();
                    if !return_type.is_empty() {
                        return Some(return_type);
                    }
                }
            }
        }
    }

    None
}

fn extract_extends(node: &Node, source: &[u8]) -> Vec<String> {
    let mut extends = Vec::new();

    if let Some(superclass) = node.child_by_field_name("superclass") {
        if let Some(name) = get_node_text(Some(superclass), source) {
            extends.push(name);
        }
    }

    extends
}

fn extract_python_extends(node: &Node, source: &[u8]) -> Vec<String> {
    let mut extends = Vec::new();

    if let Some(base_clause) = node.child_by_field_name("bases") {
        let mut cursor = base_clause.walk();
        for child in base_clause.children(&mut cursor) {
            if let Some(name) = get_node_text(Some(child), source) {
                extends.push(name);
            }
        }
    }

    extends
}

fn extract_implements(node: &Node, source: &[u8]) -> Vec<String> {
    let mut implements = Vec::new();

    if let Some(protocols) = node.child_by_field_name("protocols") {
        let mut cursor = protocols.walk();
        for child in protocols.children(&mut cursor) {
            if let Some(name) = get_node_text(Some(child), source) {
                implements.push(name);
            }
        }
    }

    implements
}

fn extract_type_refs(node: &Node, source: &[u8]) -> Vec<TypeRef> {
    let mut type_refs = Vec::new();

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "type_annotation" {
            if let Some(type_node) = child.child_by_field_name("type") {
                if let Some(type_str) = get_node_text(Some(type_node), source) {
                    let parts: Vec<&str> = type_str.splitn(2, '<').collect();
                    let name = parts[0].to_string();
                    let generic = parts.get(1).map(|s| s.trim_end_matches('>').to_string());

                    type_refs.push(TypeRef { name, generic });
                }
            }
        } else if child.kind() == "attribute" {
            if let Some(type_node) = child.child_by_field_name("type") {
                if let Some(type_str) = get_node_text(Some(type_node), source) {
                    let parts: Vec<&str> = type_str.splitn(2, '<').collect();
                    let name = parts[0].to_string();
                    let generic = parts.get(1).map(|s| s.trim_end_matches('>').to_string());

                    type_refs.push(TypeRef { name, generic });
                }
            }
        } else if child.kind() == "variable_declarator" {
            if let Some(type_node) = child.child_by_field_name("type") {
                if let Some(type_str) = get_node_text(Some(type_node), source) {
                    type_refs.push(TypeRef {
                        name: type_str,
                        generic: None,
                    });
                }
            }
        }
    }

    type_refs
}

fn extract_interface_members(node: &Node, source: &[u8]) -> Vec<TypeRef> {
    let mut members = Vec::new();

    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "property_signature" {
                let name = get_node_text(child.child_by_field_name("name"), source)
                    .unwrap_or_default();
                let mut type_refs = extract_type_refs(&child, source);

                if !type_refs.is_empty() {
                    if let Some(tr) = type_refs.pop() {
                        members.push(TypeRef {
                            name: format!("{}: {}", name, tr.name),
                            generic: tr.generic,
                        });
                    }
                } else {
                    members.push(TypeRef {
                        name,
                        generic: None,
                    });
                }
            } else if child.kind() == "method_signature" {
                let name = get_node_text(child.child_by_field_name("name"), source)
                    .unwrap_or_default();
                let params = extract_params(&child, source, "typescript");
                let return_type = extract_return_type(&child, source);

                let sig = format!(
                    "{}({}) => {}",
                    name,
                    params
                        .iter()
                        .map(|p| p.name.clone())
                        .collect::<Vec<_>>()
                        .join(", "),
                    return_type.unwrap_or_default()
                );
                members.push(TypeRef {
                    name: sig,
                    generic: None,
                });
            }
        }
    }

    members
}

fn extract_function_calls(node: &Node, source: &[u8]) -> Vec<String> {
    let mut calls = Vec::new();

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "call_expression" {
            if let Some(func) = child.child_by_field_name("function") {
                if let Some(name) = get_node_text(Some(func), source) {
                    if !calls.contains(&name) {
                        calls.push(name);
                    }
                }
            }
        }
    }

    calls
}

fn extract_docstring(node: &Node, source: &[u8]) -> Option<String> {
    let start_byte = node.start_byte();
    if start_byte < 6 {
        return None;
    }

    let search_range = 200.min(start_byte);
    let slice = &source[start_byte - search_range..start_byte];

    if let Some(start) = slice.windows(3).rposition(|w| w == b"/**") {
        let doc_start = start_byte - search_range + start;
        let doc = &source[doc_start..start_byte];

        let has_close = doc.windows(2).any(|w| w == b"*/");
        if !has_close {
            return None;
        }

        let text = String::from_utf8(doc.to_vec()).ok()?;
        let clean = text
            .lines()
            .filter_map(|l| {
                let line = l.trim().trim_start_matches('*').trim();
                if line.is_empty() || line.starts_with("/**") || line.starts_with("*/") {
                    None
                } else if line.starts_with("@param") {
                    let parts: Vec<&str> = line.splitn(2, '-').collect();
                    Some(format!("param: {}", parts.get(0).unwrap_or(&line).trim().replace("@param", "")))
                } else if line.starts_with("@return") || line.starts_with("@returns") {
                    Some(format!("returns: {}", line.replace("@return", "").replace("@returns", "").trim()))
                } else {
                    Some(line.to_string())
                }
            })
            .collect::<Vec<_>>()
            .join(" ");

        if clean.is_empty() {
            None
        } else {
            Some(clean)
        }
    } else {
        None
    }
}

fn calculate_nesting(node: &Node) -> u32 {
    let mut max_nesting: u32 = 0;
    let mut current_nesting: u32 = 0;

    let kind = node.kind();
    if kind == "function_declaration"
        || kind == "function_definition"
        || kind == "method_definition"
        || kind == "class_declaration"
        || kind == "class_definition"
    {
        current_nesting += 1;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let child_nesting = calculate_nesting(&child);
        if child_nesting > max_nesting {
            max_nesting = child_nesting;
        }
    }

    current_nesting + max_nesting
}

fn resolve_import_refs(symbols: &mut [Symbol], imports: &[ImportInfo]) {
    for _import in imports {
        for sym in symbols.iter_mut() {
            for imp in &_import.imported {
                if sym.name == imp.name {
                    sym.imports.push(ImportInfo {
                        path: _import.path.clone(),
                        imported: vec![imp.clone()],
                        is_external: _import.is_external,
                    });
                }
            }
        }
    }
}

fn get_node_text(node: Option<Node>, source: &[u8]) -> Option<String> {
    let node = node?;
    let start = node.start_byte();
    let end = node.end_byte();
    if start < end {
        String::from_utf8(source[start..end].to_vec()).ok()
    } else {
        None
    }
}

fn is_supported_file(path: &Path) -> bool {
    let ext = match path.extension() {
        Some(e) => e.to_str().unwrap_or(""),
        None => "",
    };
    SUPPORTED_EXTS.contains(&ext)
}

fn is_ignored_path(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    IGNORED_DIRS.iter().any(|&d| path_str.contains(d))
}

fn scan_files(path: &str) -> Vec<std::path::PathBuf> {
    let walker = WalkBuilder::new(path)
        .hidden(true)
        .git_ignore(true)
        .build();

    walker
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file() && is_supported_file(e.path()) && !is_ignored_path(e.path()))
        .map(|e| e.path().to_path_buf())
        .collect()
}

fn compute_hash(path: &Path) -> Option<String> {
    let data = fs::read(path).ok()?;
    Some(blake3::hash(&data).to_hex().to_string())
}

#[napi]
pub fn index(path: String) -> String {
    let start = std::time::Instant::now();
    let files_paths = scan_files(&path);
    let dependencies = extract_package_json_dependencies(&path);

    let mut files: Vec<FileNode> = Vec::new();
    let mut total_symbols = 0;

    for path in files_paths {
        if let Some(file_node) = process_file(&path) {
            total_symbols += file_node.symbols.len();
            files.push(file_node);
        }
    }

    let stats = IndexStats {
        total_files: files.len(),
        cached_files: 0,
        total_symbols,
        indexing_time_ms: start.elapsed().as_millis() as u64,
    };

    serde_json::to_string(&IndexResult { files, dependencies, stats }).unwrap_or_default()
}

fn classify_file(path: &str, symbols: &[Symbol]) -> Option<FileClassification> {
    let path_lower = path.to_lowercase();
    let file_name = Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    if path_lower.contains(".test.") || path_lower.contains(".spec.") || file_name.ends_with(".test") || file_name.ends_with(".spec") {
        return Some(FileClassification::Test);
    }

    if path_lower.contains("/components/") || path_lower.contains("/component/") || file_name.contains("component") {
        return Some(FileClassification::Component);
    }

    if path_lower.contains("/pages/") || path_lower.contains("/page/") || path_lower.contains("/routes/") || file_name == "index" && path_lower.contains("/page") {
        return Some(FileClassification::Page);
    }

    if path_lower.contains("/hooks/") || path_lower.contains("/hook/") || file_name.starts_with("use") {
        return Some(FileClassification::Hook);
    }

    if path_lower.contains("/services/") || path_lower.contains("/service/") || file_name.ends_with("service") {
        return Some(FileClassification::Service);
    }

    if path_lower.contains("/contexts/") || path_lower.contains("/context/") || file_name.ends_with("context") {
        return Some(FileClassification::Context);
    }

    if path_lower.contains("/reducers/") || path_lower.contains("/reducer/") || file_name.ends_with("reducer") {
        return Some(FileClassification::Reducer);
    }

    if path_lower.contains("/utils/") || path_lower.contains("/util/") || path_lower.contains("/helpers/") || path_lower.contains("/helper/") || file_name.ends_with("util") || file_name.ends_with("helper") {
        return Some(FileClassification::Util);
    }

    if path_lower.contains("/config/") || file_name == "config" || file_name == "settings" {
        return Some(FileClassification::Config);
    }

    if file_name.ends_with("type") || file_name.ends_with("types") || path_lower.contains("/types/") {
        return Some(FileClassification::Type);
    }

    if file_name.chars().all(|c| c.is_uppercase()) || (file_name.chars().all(|c| c.is_ascii_digit() || c == '_') && file_name.len() > 1) {
        return Some(FileClassification::Constant);
    }

    if path_lower.ends_with(".png") || path_lower.ends_with(".jpg") || path_lower.ends_with(".svg") || path_lower.ends_with(".ico") || path_lower.ends_with(".gif") {
        return Some(FileClassification::Asset);
    }

    if symbols.iter().any(|s| s.kind == "function_declaration" || s.kind == "function" || s.kind == "method_definition") {
        let exports: Vec<&str> = symbols.iter().filter_map(|s| s.exports.first().map(|e| e.name.as_str())).collect();
        if exports.iter().any(|e| e.ends_with("Provider") || e.ends_with("Context")) {
            return Some(FileClassification::Context);
        }
    }

    None
}

#[napi]
pub fn index_with_cache(path: String, cache_path: String) -> String {
    let start = std::time::Instant::now();
    let cache_file = Path::new(&cache_path).join("cache.json");
    let mut cached_hashes: HashMap<String, String> = HashMap::new();

    if cache_file.exists() {
        if let Ok(content) = fs::read_to_string(&cache_file) {
            if let Ok(hashes) = serde_json::from_str(&content) {
                cached_hashes = hashes;
            }
        }
    }

    let files_paths = scan_files(&path);
    let dependencies = extract_package_json_dependencies(&path);
    let mut files: Vec<FileNode> = Vec::new();
    let mut total_symbols = 0;
    let mut cached = 0;

    for path in files_paths {
        let path_str = path.to_string_lossy().to_string();
        let hash = match compute_hash(&path) {
            Some(h) => h,
            None => continue,
        };

        if cached_hashes.get(&path_str) == Some(&hash) {
            cached += 1;
            continue;
        }

        if let Some(mut file_node) = process_file(&path) {
            total_symbols += file_node.symbols.len();
            file_node.hash = hash.clone();
            files.push(file_node);
            cached_hashes.insert(path_str, hash);
        }
    }

    if let Ok(json) = serde_json::to_string(&cached_hashes) {
        let _ = fs::create_dir_all(&cache_path);
        let _ = fs::write(&cache_file, json);
    }

    let stats = IndexStats {
        total_files: files.len(),
        cached_files: cached,
        total_symbols,
        indexing_time_ms: start.elapsed().as_millis() as u64,
    };

    serde_json::to_string(&IndexResult { files, dependencies, stats }).unwrap_or_default()
}

fn extract_package_json_dependencies(path: &str) -> Vec<Dependency> {
    let pkg_path = Path::new(path).join("package.json");
    if !pkg_path.exists() {
        return vec![];
    }

    let content = match fs::read_to_string(&pkg_path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let pkg: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let mut deps = Vec::new();

    if let Some(deps_obj) = pkg.get("dependencies").and_then(|v| v.as_object()) {
        for (name, version) in deps_obj {
            deps.push(Dependency {
                name: name.clone(),
                version: version.as_str().map(|s| s.to_string()),
                dev: false,
                optional: false,
            });
        }
    }

    if let Some(dev_deps) = pkg.get("devDependencies").and_then(|v| v.as_object()) {
        for (name, version) in dev_deps {
            deps.push(Dependency {
                name: name.clone(),
                version: version.as_str().map(|s| s.to_string()),
                dev: true,
                optional: false,
            });
        }
    }

    if let Some(opt_deps) = pkg.get("optionalDependencies").and_then(|v| v.as_object()) {
        for (name, version) in opt_deps {
            deps.push(Dependency {
                name: name.clone(),
                version: version.as_str().map(|s| s.to_string()),
                dev: false,
                optional: true,
            });
        }
    }

    deps
}