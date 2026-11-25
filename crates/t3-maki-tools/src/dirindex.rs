//! `index --path <dir>` — .gitignore-aware compact tree with line/symbol counts.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use ignore::WalkBuilder;
use serde_json::json;

use crate::skeleton::{count_symbols, Lang, MAX_FILE_BYTES};

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    ".venv",
    "__pycache__",
];

enum Tree {
    Dir(BTreeMap<String, Tree>),
    File { lines: Option<usize>, syms: usize },
}

fn file_stats(path: &Path) -> (Option<usize>, usize) {
    read_stats(path).unwrap_or((None, 0))
}

fn read_stats(path: &Path) -> std::io::Result<(Option<usize>, usize)> {
    let mut reader = BufReader::new(File::open(path)?);
    if reader.fill_buf()?.contains(&0) {
        return Ok((None, 0));
    }
    let lang = Lang::from_path(path);
    let mut source = lang.is_tree_sitter().then(Vec::new);
    let mut lines = 0;
    let mut last = None;
    loop {
        let bytes = reader.fill_buf()?;
        if bytes.is_empty() {
            break;
        }
        lines += bytes.iter().filter(|&&byte| byte == b'\n').count();
        last = bytes.last().copied();
        if let Some(buffer) = &mut source {
            if buffer.len() + bytes.len() <= MAX_FILE_BYTES as usize {
                buffer.extend_from_slice(bytes);
            } else {
                source = None;
            }
        }
        let len = bytes.len();
        reader.consume(len);
    }
    let symbols = source
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .map_or(0, |src| count_symbols(lang, src));
    Ok((
        Some(lines + usize::from(last.is_some_and(|byte| byte != b'\n'))),
        symbols,
    ))
}

fn render(map: &BTreeMap<String, Tree>, indent: usize, lines: &mut Vec<String>) {
    let pad = "  ".repeat(indent);
    for (name, node) in map {
        if let Tree::Dir(children) = node {
            lines.push(format!("{pad}{name}/"));
            render(children, indent + 1, lines);
        }
    }
    for (name, node) in map {
        if let Tree::File { lines: lc, syms } = node {
            let desc = match lc {
                Some(l) if *syms > 0 => format!("{l} lines, {syms} symbols"),
                Some(l) => format!("{l} lines"),
                None => "binary".to_string(),
            };
            lines.push(format!("{pad}{name} ({desc})"));
        }
    }
}

fn count_files(tree: &Tree) -> (usize, usize) {
    match tree {
        Tree::File { syms, .. } => (1, *syms),
        Tree::Dir(map) => map
            .values()
            .map(count_files)
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1)),
    }
}

pub fn index_dir(root: &Path, max_depth: usize, max_lines: usize) -> ! {
    let mut builder = WalkBuilder::new(root);
    builder
        .max_depth(Some(max_depth))
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .parents(true)
        .filter_entry(|e| {
            !(e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref()))
        });
    let mut tree = Tree::Dir(BTreeMap::new());
    for entry in builder.build().flatten() {
        if entry.path() == root {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let comps: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        if comps.is_empty() {
            continue;
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        let is_dir = file_type.is_dir();
        if !is_dir && !file_type.is_file() {
            continue;
        }
        let mut cur = &mut tree;
        for (i, comp) in comps.iter().enumerate() {
            let last = i + 1 == comps.len();
            let Tree::Dir(map) = cur else { break };
            if last && !is_dir {
                let (lines, syms) = file_stats(entry.path());
                map.insert(comp.clone(), Tree::File { lines, syms });
                break;
            }
            cur = map
                .entry(comp.clone())
                .or_insert_with(|| Tree::Dir(BTreeMap::new()));
        }
    }

    let (file_count, symbol_count) = count_files(&tree);
    let mut lines = vec![format!(
        "{} ({file_count} files, {symbol_count} symbols)",
        root.display()
    )];
    if let Tree::Dir(map) = &tree {
        render(map, 1, &mut lines);
    }
    let mut truncated = false;
    if lines.len() > max_lines {
        let extra = lines.len() + 1 - max_lines;
        lines.truncate(max_lines.saturating_sub(1));
        lines.push(format!("[... {extra} more lines truncated]"));
        truncated = true;
    }
    crate::emit(&json!({
        "ok": true,
        "kind": "dir",
        "path": root.display().to_string(),
        "skeleton": lines.join("\n"),
        "fileCount": file_count,
        "symbolCount": symbol_count,
        "truncated": truncated,
    }));
    std::process::exit(0);
}
