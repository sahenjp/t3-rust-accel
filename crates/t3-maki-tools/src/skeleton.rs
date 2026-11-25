//! Maki-style structural skeletons from real tree-sitter parses.

use std::path::Path;

use serde_json::{json, Value};
use tree_sitter::{Language, Node, Parser};

use crate::fail;

/// Maki `max_file_size_mb = 2` default.
pub(crate) const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Bound recursive AST walks so adversarial deeply nested source cannot
/// overflow the CLI process stack. Real-world definitions are far shallower.
const MAX_AST_WALK_DEPTH: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Lang {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Jsx,
    Python,
    Go,
    C,
    Cpp,
    Bash,
    Json,
    Markdown,
    Toml,
    Yaml,
    Sql,
    Text,
}

impl Lang {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::JavaScript => "javascript",
            Lang::Jsx => "jsx",
            Lang::Python => "python",
            Lang::Go => "go",
            Lang::C => "c",
            Lang::Cpp => "cpp",
            Lang::Bash => "bash",
            Lang::Json => "json",
            Lang::Markdown => "markdown",
            Lang::Toml => "toml",
            Lang::Yaml => "yaml",
            Lang::Sql => "sql",
            Lang::Text => "text",
        }
    }

    pub(crate) fn from_path(path: &Path) -> Lang {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Lang::Rust,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "js" | "mjs" | "cjs" => Lang::JavaScript,
            "jsx" => Lang::Jsx,
            "py" | "pyi" => Lang::Python,
            "go" => Lang::Go,
            "c" | "h" => Lang::C,
            "cc" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" => Lang::Cpp,
            "sh" | "bash" => Lang::Bash,
            "json" => Lang::Json,
            "md" | "markdown" => Lang::Markdown,
            "toml" => Lang::Toml,
            "yaml" | "yml" => Lang::Yaml,
            "sql" => Lang::Sql,
            _ => Lang::Text,
        }
    }

    pub(crate) fn is_tree_sitter(self) -> bool {
        !matches!(
            self,
            Lang::Markdown | Lang::Toml | Lang::Yaml | Lang::Sql | Lang::Text
        )
    }

    fn grammar(self) -> Option<Language> {
        let l: Language = match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::JavaScript | Lang::Jsx => tree_sitter_javascript::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::C => tree_sitter_c::LANGUAGE.into(),
            Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Lang::Bash => tree_sitter_bash::LANGUAGE.into(),
            Lang::Json => tree_sitter_json::LANGUAGE.into(),
            _ => return None,
        };
        Some(l)
    }
}

fn parse(lang: Lang, src: &str) -> Option<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser.set_language(&lang.grammar()?).ok()?;
    parser.parse(src, None)
}

/// One skeleton entry in source order.
#[derive(Debug)]
enum Entry {
    Import {
        start: usize,
        end: usize,
    },
    Sym {
        indent: usize,
        text: String,
        start: usize,
        end: usize,
    },
}

/// What the walker should do with a node.
#[derive(Clone, Copy, PartialEq)]
enum Act {
    Skip,
    Import,
    Leaf,
    /// Emit a header signature, then walk the body for members.
    Container,
    /// Transparent wrapper: walk children at the same indent,
    /// keeping the wrapper's start position (export/decorated/template).
    Unwrap,
}

fn act(lang: Lang, node: &Node, src: &str) -> Act {
    let k = node.kind();
    match lang {
        Lang::Rust => match k {
            "use_declaration" | "extern_crate_declaration" => Act::Import,
            "function_item"
            | "function_signature_item"
            | "const_item"
            | "static_item"
            | "type_item"
            | "associated_type"
            | "struct_item"
            | "enum_item"
            | "union_item"
            | "macro_definition"
            | "trait_item"
            | "impl_item"
            | "mod_item"
            | "foreign_mod_item" => {
                if matches!(
                    k,
                    "trait_item" | "impl_item" | "mod_item" | "foreign_mod_item"
                ) {
                    if has_body(node) {
                        Act::Container
                    } else {
                        Act::Leaf
                    }
                } else {
                    Act::Leaf
                }
            }
            _ => Act::Skip,
        },
        Lang::TypeScript | Lang::Tsx => match k {
            "import_statement" | "import_alias" => Act::Import,
            "export_statement" | "ambient_declaration" | "module" => Act::Unwrap,
            "class_declaration"
            | "abstract_class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "internal_module" => {
                if has_body(node) {
                    Act::Container
                } else {
                    Act::Leaf
                }
            }
            "function_declaration"
            | "generator_function_declaration"
            | "function_signature"
            | "method_definition"
            | "abstract_method_signature"
            | "method_signature"
            | "property_signature"
            | "public_field_definition"
            | "index_signature"
            | "type_alias_declaration"
            | "lexical_declaration"
            | "variable_declaration"
            | "call_signature" => Act::Leaf,
            _ => Act::Skip,
        },
        Lang::JavaScript | Lang::Jsx => match k {
            "import_statement" => Act::Import,
            "export_statement" => Act::Unwrap,
            "class_declaration" => {
                if has_body(node) {
                    Act::Container
                } else {
                    Act::Leaf
                }
            }
            "function_declaration"
            | "generator_function_declaration"
            | "method_definition"
            | "field_definition"
            | "lexical_declaration"
            | "variable_declaration"
            | "method_signature"
            | "static_block" => Act::Leaf,
            _ => Act::Skip,
        },
        Lang::Python => match k {
            "import_statement" | "import_from_statement" | "future_import_statement" => Act::Import,
            "decorated_definition" => Act::Unwrap,
            "class_definition" => {
                if has_body(node) {
                    Act::Container
                } else {
                    Act::Leaf
                }
            }
            "function_definition" => Act::Leaf,
            "expression_statement" => {
                if node
                    .named_child(0)
                    .map(|c| c.kind() == "assignment" || c.kind() == "augmented_assignment")
                    .unwrap_or(false)
                {
                    Act::Leaf
                } else {
                    Act::Skip
                }
            }
            "assignment" => Act::Leaf,
            _ => Act::Skip,
        },
        Lang::Go => match k {
            "import_declaration" => Act::Import,
            "package_clause"
            | "function_declaration"
            | "method_declaration"
            | "type_declaration"
            | "const_declaration"
            | "var_declaration"
            | "type_element" => Act::Leaf,
            _ => Act::Skip,
        },
        Lang::C => match k {
            "preproc_include" => Act::Import,
            "preproc_ifdef" | "preproc_if" | "preproc_elif" | "preproc_else" => Act::Unwrap,
            "struct_specifier" | "class_specifier" | "union_specifier" | "enum_specifier" => {
                if has_body(node) {
                    Act::Container
                } else if node.child_by_field_name("name").is_some() {
                    Act::Leaf
                } else {
                    Act::Skip
                }
            }
            "declaration" => match node.named_child(0).map(|c| c.kind()) {
                Some("struct_specifier") | Some("union_specifier") | Some("enum_specifier") => {
                    if has_body(node) {
                        Act::Container
                    } else {
                        Act::Leaf
                    }
                }
                _ => Act::Leaf,
            },
            "function_definition"
            | "type_definition"
            | "preproc_function_def"
            | "preproc_def"
            | "field_declaration" => Act::Leaf,
            "linkage_specification" => Act::Unwrap,
            _ => Act::Skip,
        },
        Lang::Cpp => match k {
            "preproc_include" => Act::Import,
            "preproc_ifdef" | "preproc_if" | "preproc_elif" | "preproc_else" => Act::Unwrap,
            "template_declaration" | "linkage_specification" | "export_statement" => Act::Unwrap,
            "struct_specifier" | "class_specifier" | "union_specifier" | "enum_specifier" => {
                if has_body(node) {
                    Act::Container
                } else if node.child_by_field_name("name").is_some() {
                    Act::Leaf
                } else {
                    Act::Skip
                }
            }
            "namespace_definition" => {
                if has_body(node) {
                    Act::Container
                } else {
                    Act::Leaf
                }
            }
            "declaration" => {
                // `class Foo { ... };` surfaces as declaration > class_specifier.
                match node.named_child(0).map(|c| c.kind()) {
                    Some("struct_specifier")
                    | Some("class_specifier")
                    | Some("union_specifier")
                    | Some("enum_specifier") => {
                        if has_body(node) {
                            Act::Container
                        } else {
                            Act::Leaf
                        }
                    }
                    _ => Act::Leaf,
                }
            }
            "function_definition"
            | "type_definition"
            | "preproc_function_def"
            | "preproc_def"
            | "alias_declaration"
            | "using_declaration"
            | "concept_definition"
            | "field_declaration"
            | "friend_declaration"
            | "static_assert_declaration"
            | "access_specifier" => Act::Leaf,
            _ => Act::Skip,
        },
        Lang::Bash => match k {
            "function_definition" => Act::Leaf,
            "variable_assignment" | "declaration_command" => Act::Leaf,
            "command" => {
                // `source file` / `. file` count as imports.
                let name = node
                    .named_child(0)
                    .map(|c| &src[c.byte_range()])
                    .unwrap_or("");
                if name == "source" || name == "." {
                    Act::Import
                } else {
                    Act::Skip
                }
            }
            _ => Act::Skip,
        },
        Lang::Json => Act::Skip, // handled by json_skeleton
        _ => Act::Skip,
    }
}

const BODY_KINDS: &[&str] = &[
    "block",
    "compound_statement",
    "statement_block",
    "declaration_list",
    "class_body",
    "interface_body",
    "enum_body",
    "field_declaration_list",
    "enumerator_list",
    "object_type",
];

fn is_body_kind(k: &str) -> bool {
    BODY_KINDS.contains(&k)
}

fn body_node<'a>(node: &Node<'a>) -> Option<Node<'a>> {
    if let Some(b) = node.child_by_field_name("body") {
        return Some(b);
    }
    let last = node.named_children(&mut node.walk()).last()?;
    if is_body_kind(last.kind()) {
        return Some(last);
    }
    // `struct Foo { ... };` — the list lives inside the specifier child.
    if matches!(
        last.kind(),
        "class_specifier" | "union_specifier" | "struct_specifier" | "enum_specifier"
    ) {
        return last
            .named_children(&mut last.walk())
            .last()
            .filter(|c| is_body_kind(c.kind()));
    }
    None
}

fn has_body(node: &Node) -> bool {
    body_node(node).is_some()
}

/// Collapse the signature part of a node (everything before its body block,
/// or its first line when it has no body) into one compact line.
fn signature(node: &Node, src: &str) -> String {
    let body = body_node(node).filter(|b| b.start_byte() > node.start_byte());
    let (raw, multiline) = match body {
        Some(b) => (&src[node.start_byte()..b.start_byte()], true),
        None => {
            let text = &src[node.byte_range()];
            match text.find('\n') {
                Some(i) => (&text[..i], true),
                None => (text, false),
            }
        }
    };
    let mut sig: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if sig.chars().count() > 180 {
        sig = sig.chars().take(180).collect();
        sig.push('…');
    } else if multiline && !sig.ends_with('{') && !sig.ends_with('(') && !sig.ends_with("…") {
        sig.push_str(" …");
    }
    sig.trim().to_string()
}

fn text_of(node: &Node, src: &str) -> String {
    src[node.byte_range()].to_string()
}

/// Extract the declared name of a node (for `slice --symbol`).
fn node_name(node: &Node, src: &str) -> Option<String> {
    if let Some(n) = node.child_by_field_name("name") {
        return Some(text_of(&n, src));
    }
    match node.kind() {
        "impl_item" => node
            .child_by_field_name("type")
            .map(|t| simplify_type(&text_of(&t, src))),
        "method_declaration" => node.child_by_field_name("name").map(|n| text_of(&n, src)),
        "type_declaration" => node
            .named_child(0)
            .and_then(|s| s.child_by_field_name("name"))
            .map(|n| text_of(&n, src)),
        "function_definition" => c_decl_name(*node, src),
        "declaration" => c_decl_name(*node, src).or_else(|| {
            node.named_child(0)
                .filter(|child| {
                    matches!(
                        child.kind(),
                        "struct_specifier"
                            | "class_specifier"
                            | "union_specifier"
                            | "enum_specifier"
                    )
                })
                .and_then(|specifier| specifier.child_by_field_name("name"))
                .map(|name| text_of(&name, src))
        }),
        "field_declaration" => c_decl_name(*node, src),
        "type_definition" => node
            .child_by_field_name("declarator")
            .and_then(|d| declarator_ident(d, src)),
        "expression_statement" | "assignment" => {
            let a = if node.kind() == "assignment" {
                *node
            } else {
                node.named_child(0)?
            };
            a.child_by_field_name("left").map(|l| {
                text_of(&l, src)
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string()
            })
        }
        "variable_declaration" | "lexical_declaration" => node
            .named_children(&mut node.walk())
            .find(|c| c.kind() == "variable_declarator")
            .and_then(|d| d.child_by_field_name("name"))
            .map(|n| text_of(&n, src)),
        "decorated_definition"
        | "export_statement"
        | "template_declaration"
        | "ambient_declaration"
        | "module" => node
            .named_children(&mut node.walk())
            .find(|c| !matches!(c.kind(), "decorator" | "export_clause"))
            .and_then(|c| node_name(&c, src)),
        "declaration_command" | "variable_assignment" => {
            node.named_child(0).map(|c| text_of(&c, src))
        }
        "namespace_definition" | "internal_module" | "mod_item" => node
            .named_children(&mut node.walk())
            .find(|c| {
                matches!(
                    c.kind(),
                    "identifier" | "namespace_identifier" | "type_identifier" | "scoped_identifier"
                )
            })
            .map(|n| text_of(&n, src)),
        _ => None,
    }
}

/// `impl Foo<T>` / `impl dyn Trait for Foo` / `(s *S)` -> bare type name for
/// `Type::member` matching.
fn simplify_type(t: &str) -> String {
    let t = t
        .trim()
        .trim_start_matches("dyn ")
        .trim_start_matches('&')
        .trim_start_matches('*');
    let base: String = t.chars().take_while(|&c| c != '<').collect();
    base.rsplit("::").next().unwrap_or(&base).trim().to_string()
}

/// Name inside a C/C++ declarator chain: `int (*f)(int)` -> f.
fn declarator_ident(mut d: Node, src: &str) -> Option<String> {
    loop {
        match d.kind() {
            "identifier" | "field_identifier" | "type_identifier" => return Some(text_of(&d, src)),
            "qualified_identifier" | "scoped_identifier" => {
                if let Some(n) = d.child_by_field_name("name") {
                    return Some(text_of(&n, src));
                }
            }
            _ => {}
        }
        let next = d
            .child_by_field_name("declarator")
            .or_else(|| d.named_child(d.named_child_count().saturating_sub(1)));
        match next {
            Some(n) if n.id() != d.id() => d = n,
            _ => return None,
        }
    }
}

fn c_decl_name(node: Node, src: &str) -> Option<String> {
    let d = node.child_by_field_name("declarator")?;
    declarator_ident(d, src)
}

fn walk(node: &Node, src: &str, lang: Lang, indent: usize, depth: usize, entries: &mut Vec<Entry>) {
    if depth >= MAX_AST_WALK_DEPTH {
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match act(lang, &child, src) {
            Act::Skip => {}
            Act::Import => entries.push(Entry::Import {
                start: child.start_position().row + 1,
                end: child.end_position().row + 1,
            }),
            Act::Leaf => entries.push(Entry::Sym {
                indent,
                text: signature(&child, src),
                start: child.start_position().row + 1,
                end: child.end_position().row + 1,
            }),
            Act::Container => {
                entries.push(Entry::Sym {
                    indent,
                    text: signature(&child, src),
                    start: child.start_position().row + 1,
                    end: child.end_position().row + 1,
                });
                if let Some(body) = body_node(&child) {
                    walk(&body, src, lang, indent + 1, depth + 1, entries);
                }
            }
            Act::Unwrap => walk(&child, src, lang, indent, depth + 1, entries),
        }
    }
}

fn json_skeleton(tree: &tree_sitter::Tree, src: &str, entries: &mut Vec<Entry>) {
    let root = tree.root_node();
    let Some(doc) = root.named_child(0) else {
        return;
    };
    match doc.kind() {
        "object" => {
            let mut cursor = doc.walk();
            for pair in doc.named_children(&mut cursor) {
                if pair.kind() != "pair" {
                    continue;
                }
                let key = pair
                    .child_by_field_name("key")
                    .map(|k| text_of(&k, src))
                    .unwrap_or_else(|| "?".into());
                let vkind = pair
                    .child_by_field_name("value")
                    .map(|v| v.kind())
                    .unwrap_or("?");
                entries.push(Entry::Sym {
                    indent: 0,
                    text: format!("{key}: {vkind}"),
                    start: pair.start_position().row + 1,
                    end: pair.end_position().row + 1,
                });
            }
        }
        "array" => {
            entries.push(Entry::Sym {
                indent: 0,
                text: format!("array ({} items)", doc.named_child_count()),
                start: doc.start_position().row + 1,
                end: doc.end_position().row + 1,
            });
        }
        _ => entries.push(Entry::Sym {
            indent: 0,
            text: format!("{} literal", doc.kind()),
            start: 1,
            end: doc.end_position().row + 1,
        }),
    }
}

/// Heading/section outline for languages without a grammar.
fn text_outline(lang: Lang, src: &str, entries: &mut Vec<Entry>) {
    let mut emitted = 0usize;
    for (i, line) in src.lines().enumerate() {
        let l = line.trim_end();
        let (indent, text) = match lang {
            Lang::Markdown => {
                let hashes = l.chars().take_while(|&c| c == '#').count();
                if hashes == 0 || hashes > 6 || !l[hashes..].starts_with(' ') {
                    continue;
                }
                (hashes - 1, l.trim_start_matches('#').trim().to_string())
            }
            Lang::Toml => {
                if !(l.starts_with('[') && l.ends_with(']')) {
                    continue;
                }
                let depth = if l.starts_with("[[") { 1 } else { 0 };
                (depth, l.to_string())
            }
            Lang::Yaml => {
                if line.starts_with(' ') || line.starts_with('\t') || line.starts_with('-') {
                    continue;
                }
                match l.split_once(':') {
                    Some((k, _)) if !k.trim().is_empty() => (0, format!("{}:", k.trim())),
                    _ => continue,
                }
            }
            Lang::Sql => {
                let up = l.to_ascii_uppercase();
                if [
                    "CREATE", "ALTER", "DROP", "INSERT", "SELECT", "WITH", "GRANT", "USE",
                ]
                .iter()
                .any(|p| up.starts_with(p))
                {
                    (0, l.chars().take(100).collect::<String>())
                } else {
                    continue;
                }
            }
            _ => {
                // Generic: top-level (unindented) non-blank lines.
                if line.starts_with(' ') || line.starts_with('\t') || line.trim().is_empty() {
                    continue;
                }
                (0, l.chars().take(120).collect::<String>())
            }
        };
        entries.push(Entry::Sym {
            indent,
            text,
            start: i + 1,
            end: i + 1,
        });
        emitted += 1;
        if emitted >= 200 {
            break;
        }
    }
}

struct Skeleton {
    text: String,
    line_count: usize,
    symbol_count: usize,
    truncated: bool,
}

fn outline_entries(lang: Lang, src: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    if let Some(tree) = parse(lang, src) {
        if lang == Lang::Json {
            json_skeleton(&tree, src, &mut entries);
        } else {
            let root = tree.root_node();
            walk(&root, src, lang, 0, 0, &mut entries);
        }
    }
    if entries.is_empty() && !lang.is_tree_sitter() {
        // Unsupported text languages fall back to a heading/section outline;
        // never fail on .md/.toml/.yaml/.sql.
        text_outline(lang, src, &mut entries);
    }
    entries
}

pub(crate) fn count_symbols(lang: Lang, src: &str) -> usize {
    outline_entries(lang, src)
        .into_iter()
        .filter(|entry| matches!(entry, Entry::Sym { .. }))
        .count()
}

/// Build the skeleton for `src` parsed as `lang`, capped at `max_lines`.
fn build_skeleton(lang: Lang, src: &str, max_lines: usize) -> Skeleton {
    let line_count = src.lines().count();
    let entries = outline_entries(lang, src);
    let mut lines: Vec<String> = Vec::new();
    let mut symbol_count = 0usize;
    let mut pending_imports: Option<(usize, usize, usize)> = None; // start, end, count
    let flush_imports = |lines: &mut Vec<String>, p: &mut Option<(usize, usize, usize)>| {
        if let Some((s, e, n)) = p.take() {
            if n > 1 {
                lines.push(format!("imports ({n}): [{s}-{e}]"));
            } else {
                lines.push(format!("imports: [{s}-{e}]"));
            }
        }
    };
    for e in entries {
        match e {
            Entry::Import { start, end } => match pending_imports.as_mut() {
                Some((s, e2, n)) => {
                    *e2 = end;
                    *n += 1;
                    let _ = s;
                }
                None => pending_imports = Some((start, end, 1)),
            },
            Entry::Sym {
                indent,
                text,
                start,
                end,
            } => {
                flush_imports(&mut lines, &mut pending_imports);
                symbol_count += 1;
                let pad = "  ".repeat(indent.min(6));
                lines.push(format!("{pad}{text} [{start}-{end}]"));
            }
        }
    }
    flush_imports(&mut lines, &mut pending_imports);

    let mut truncated = false;
    if lines.len() > max_lines {
        let extra = lines.len() + 1 - max_lines;
        lines.truncate(max_lines.saturating_sub(1));
        lines.push(format!("[... {extra} more lines truncated]"));
        truncated = true;
    }
    Skeleton {
        text: lines.join("\n"),
        line_count,
        symbol_count,
        truncated,
    }
}

// ---------------- symbol search for `slice` ----------------

pub(crate) struct SymMatch {
    pub(crate) qualified: String,
    pub(crate) name: String,
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
}

/// Kinds whose members get `<Container>::` qualified names.
fn container_name(lang: Lang, node: &Node, src: &str) -> Option<String> {
    let k = node.kind();
    let is_container = match lang {
        Lang::Rust => matches!(k, "impl_item" | "trait_item" | "mod_item" | "enum_item"),
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript | Lang::Jsx => matches!(
            k,
            "class_declaration"
                | "abstract_class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "internal_module"
                | "class"
        ),
        Lang::Python => matches!(k, "class_definition"),
        Lang::Cpp => matches!(
            k,
            "namespace_definition" | "struct_specifier" | "class_specifier" | "union_specifier"
        ),
        Lang::C => matches!(k, "struct_specifier" | "union_specifier" | "enum_specifier"),
        _ => false,
    };
    if is_container {
        node_name(node, src)
    } else {
        None
    }
}

fn push_match(n: &Node, src: &str, ancestors: &[String], out: &mut Vec<SymMatch>) {
    let Some(mut name) = node_name(n, src) else {
        return;
    };
    if name.is_empty() {
        return;
    }
    // Go methods carry their receiver type in the qualified name: `S::method`.
    let qualified = if n.kind() == "method_declaration" {
        let recv = n
            .child_by_field_name("receiver")
            .and_then(|r| r.named_child(0).and_then(|p| p.child_by_field_name("type")))
            .map(|t| simplify_type(&text_of(&t, src)))
            .unwrap_or_default();
        if recv.is_empty() {
            name.clone()
        } else if ancestors.is_empty() {
            format!("{recv}::{name}")
        } else {
            format!("{}::{recv}::{name}", ancestors.join("::"))
        }
    } else if ancestors.is_empty() {
        name.clone()
    } else {
        format!("{}::{}", ancestors.join("::"), name)
    };
    let ext = extend_start(*n);
    // Trim `name` down to a bare identifier where extraction grabbed more.
    if name.contains('(') {
        name = name.split('(').next().unwrap_or(&name).trim().to_string();
    }
    out.push(SymMatch {
        qualified,
        name,
        start_line: ext.start_position().row + 1,
        end_line: n.end_position().row + 1,
    });
}

/// Recursive walk collecting every named definition-like node.
fn collect(
    node: &Node,
    src: &str,
    lang: Lang,
    ancestors: &[String],
    depth: usize,
    out: &mut Vec<SymMatch>,
) {
    if depth >= MAX_AST_WALK_DEPTH {
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let a = act(lang, &child, src);
        if matches!(a, Act::Leaf | Act::Container) {
            if matches!(child.kind(), "variable_declaration" | "lexical_declaration") {
                // `const a = .., b = ..`: multiple declarators are individual
                // symbols; a single declarator is the same range as the parent.
                let declarators: Vec<_> = child
                    .named_children(&mut child.walk())
                    .filter(|c| c.kind() == "variable_declarator")
                    .collect();
                if declarators.len() > 1 {
                    for d in &declarators {
                        push_match(d, src, ancestors, out);
                    }
                } else {
                    push_match(&child, src, ancestors, out);
                }
            } else {
                push_match(&child, src, ancestors, out);
            }
        }
        let mut anc_owned;
        let anc = if a == Act::Container {
            match container_name(lang, &child, src) {
                Some(cn) => {
                    anc_owned = ancestors.to_vec();
                    anc_owned.push(cn);
                    &anc_owned
                }
                None => ancestors,
            }
        } else {
            ancestors
        };
        collect(&child, src, lang, anc, depth + 1, out);
    }
}

/// Include leading attribute/decorator siblings directly above the node.
fn extend_start<'a>(node: Node<'a>) -> Node<'a> {
    let mut n = node;
    while let Some(prev) = n.prev_named_sibling() {
        let adjacent = prev.end_position().row + 1 >= n.start_position().row;
        if !adjacent
            || !matches!(
                prev.kind(),
                "attribute_item" | "decorator" | "line_comment" | "block_comment" | "comment"
            )
        {
            break;
        }
        n = prev;
    }
    n
}

/// Every named definition-like symbol in `src`, in source order.
/// Used by `grep` to annotate matches with their enclosing symbol.
pub(crate) fn all_symbols(lang: Lang, src: &str) -> Vec<SymMatch> {
    let Some(tree) = parse(lang, src) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect(&tree.root_node(), src, lang, &[], 0, &mut out);
    out
}

pub(crate) fn find_symbols(lang: Lang, src: &str, query: &str) -> Vec<SymMatch> {
    let Some(tree) = parse(lang, src) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect(&tree.root_node(), src, lang, &[], 0, &mut out);
    let q = query.replace('.', "::");
    let bare = !q.contains("::");
    out.into_iter()
        .filter(|m| {
            if bare {
                m.name == query || m.qualified == query
            } else {
                m.qualified == q || m.qualified.ends_with(&format!("::{q}"))
            }
        })
        .collect()
}

/// Read a file, refusing > 2 MiB (Maki max_file_size_mb default).
pub(crate) fn read_source(path: &Path) -> Result<String, Value> {
    let meta = std::fs::metadata(path).map_err(
        |e| json!({"ok": false, "error": format!("cannot stat {}: {e}", path.display())}),
    )?;
    if meta.is_dir() {
        return Err(json!({"ok": false, "error": format!("{} is a directory", path.display())}));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(json!({"ok": false, "error": format!(
            "{} exceeds the 2 MiB limit ({} bytes)", path.display(), meta.len()
        )}));
    }
    std::fs::read_to_string(path)
        .map_err(|e| json!({"ok": false, "error": format!("cannot read {}: {e}", path.display())}))
}

pub(crate) fn die(v: Value) -> ! {
    crate::emit(&v);
    std::process::exit(1);
}

/// `index --path` file mode.
pub(crate) fn cmd_index(path: &str, max_depth: usize, max_lines: usize) -> ! {
    let p = Path::new(path);
    if !p.exists() {
        fail(format!("path does not exist: {path}"));
    }
    if p.is_dir() {
        crate::dirindex::index_dir(p, max_depth, max_lines);
    }
    let src = match read_source(p) {
        Ok(s) => s,
        Err(v) => die(v),
    };
    let lang = Lang::from_path(p);
    let sk = build_skeleton(lang, &src, max_lines);
    crate::emit(&json!({
        "ok": true,
        "kind": "file",
        "path": path,
        "language": lang.name(),
        "skeleton": sk.text,
        "lineCount": sk.line_count,
        "symbolCount": sk.symbol_count,
        "truncated": sk.truncated,
    }));
    std::process::exit(0);
}
