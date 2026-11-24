//! `grep --pattern <regex> --path <abs>` — regex search grouped by file,
//! annotated with the enclosing tree-sitter symbol for supported languages.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use serde_json::json;

use crate::skeleton::{all_symbols, Lang, SymMatch, MAX_FILE_BYTES};
use crate::text::head_tail_bytes;
use crate::{emit, fail};

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    ".venv",
    "__pycache__",
];

const GENERATED_NAMES: &[&str] = &[
    "pnpm-lock.yaml",
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
];

fn is_generated_file(name: &str) -> bool {
    GENERATED_NAMES.contains(&name)
        || name.ends_with(".min.js")
        || name.ends_with(".min.css")
        || name.ends_with(".map")
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8192)].contains(&0)
}

/// Build a matcher for `--glob *.ts` / `--glob src/**` style patterns.
/// A bare pattern also matches at any directory depth.
fn build_globset(pattern: &str) -> Result<GlobSet, String> {
    let mut b = GlobSetBuilder::new();
    b.add(Glob::new(pattern).map_err(|e| format!("invalid --glob '{pattern}': {e}"))?);
    if !pattern.starts_with("**/") && !pattern.starts_with('/') {
        if let Ok(g) = Glob::new(&format!("**/{pattern}")) {
            b.add(g);
        }
    }
    b.build()
        .map_err(|e| format!("invalid --glob '{pattern}': {e}"))
}

/// Innermost symbol covering `line` (1-based), if any.
fn enclosing<'a>(syms: &'a [SymMatch], line: usize) -> Option<&'a str> {
    syms.iter()
        .filter(|s| s.start_line <= line && line <= s.end_line)
        .min_by_key(|s| s.end_line - s.start_line)
        .map(|s| s.qualified.as_str())
}

fn search_file(
    re: &regex::bytes::Regex,
    path: &Path,
    hits: &mut Vec<(usize, String)>,
    cap: usize,
) -> usize {
    let Ok(bytes) = std::fs::read(path) else {
        return 0;
    };
    if bytes.len() as u64 > MAX_FILE_BYTES || is_binary(&bytes) {
        return 0;
    }
    let mut total = 0usize;
    for (i, line) in bytes.split(|&b| b == b'\n').enumerate() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if re.is_match(line) {
            total += 1;
            if hits.len() < cap {
                let text = String::from_utf8_lossy(line).trim().to_string();
                let text: String = text.chars().take(200).collect();
                hits.push((i + 1, text));
            }
        }
    }
    total
}

pub fn cmd_grep(
    pattern: &str,
    path: &str,
    glob: Option<&str>,
    max_hits: usize,
    max_bytes: usize,
) -> ! {
    let re = match regex::bytes::Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => fail(format!("invalid --pattern regex: {e}")),
    };
    let root = Path::new(path);
    if !root.exists() {
        fail(format!("path does not exist: {path}"));
    }
    let glob_set = match glob {
        Some(g) => match build_globset(g) {
            Ok(s) => Some(s),
            Err(e) => fail(e),
        },
        None => None,
    };

    // display path -> matched (line_no, trimmed text)
    let mut by_file: BTreeMap<String, Vec<(usize, String)>> = BTreeMap::new();
    let mut total_hits = 0usize;
    let mut files_matched = 0usize;

    if root.is_file() {
        let name_ok = glob_set
            .as_ref()
            .map(|gs| {
                gs.is_match(root.file_name().unwrap_or_default().to_string_lossy().as_ref())
                    || gs.is_match(root)
            })
            .unwrap_or(true);
        if name_ok {
            let mut hits = Vec::new();
            let n = search_file(&re, root, &mut hits, max_hits);
            if n > 0 {
                by_file.insert(path.to_string(), hits);
                total_hits += n;
                files_matched += 1;
            }
        }
    } else {
        let mut builder = WalkBuilder::new(root);
        builder
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
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in builder.build().flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let p = entry.path();
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            if is_generated_file(&name) {
                continue;
            }
            let rel = p.strip_prefix(root).unwrap_or(p);
            if let Some(gs) = &glob_set {
                if !gs.is_match(rel) && !gs.is_match(name.as_ref()) {
                    continue;
                }
            }
            files.push(p.to_path_buf());
        }
        files.sort();
        for f in files {
            let rel = f.strip_prefix(root).unwrap_or(&f).to_string_lossy().to_string();
            let mut hits = Vec::new();
            let n = search_file(&re, &f, &mut hits, max_hits);
            if n > 0 {
                by_file.insert(rel, hits);
                total_hits += n;
                files_matched += 1;
            }
        }
    }

    let mut shown = 0usize;
    let mut out = String::new();
    let mut sym_cache: BTreeMap<String, Option<Vec<SymMatch>>> = BTreeMap::new();
    'files: for (disp, hits) in &by_file {
        out.push_str(disp);
        out.push('\n');
        // Lazily parse for symbol annotation only when the language has a grammar.
        let syms = sym_cache.entry(disp.clone()).or_insert_with(|| {
            let full = if root.is_file() {
                root.to_path_buf()
            } else {
                root.join(disp)
            };
            let lang = Lang::from_path(&full);
            if lang.is_tree_sitter() {
                std::fs::read_to_string(&full)
                    .ok()
                    .map(|src| all_symbols(lang, &src))
            } else {
                None
            }
        });
        for (line_no, text) in hits {
            if shown >= max_hits {
                break 'files;
            }
            let sym = syms
                .as_ref()
                .and_then(|ss| enclosing(ss, *line_no));
            match sym {
                Some(s) => out.push_str(&format!("L{line_no} [{s}]: {text}\n")),
                None => out.push_str(&format!("L{line_no}: {text}\n")),
            }
            shown += 1;
        }
    }

    let (output, byte_trunc) = head_tail_bytes(&out, max_bytes);
    emit(&json!({
        "ok": true,
        "pattern": pattern,
        "path": path,
        "filesMatched": files_matched,
        "totalHits": total_hits,
        "shownHits": shown,
        "truncated": shown < total_hits || byte_trunc,
        "output": output,
    }));
    std::process::exit(0);
}
