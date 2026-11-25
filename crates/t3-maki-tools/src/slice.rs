//! `slice --path` — extract a symbol body or an exact line range as `L<n>:` lines.

use std::fmt::Write as _;
use std::path::Path;

use serde_json::json;

use crate::skeleton::{die, find_symbols, read_source, Lang};
use crate::text::head_tail_bytes;
use crate::{emit, fail};

fn format_lines(src: &str, start_line: usize, end_line: usize) -> String {
    let mut out = String::new();
    for (i, line) in src.lines().enumerate() {
        if i + 1 < start_line || i >= end_line {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        write!(&mut out, "L{}: {line}", i + 1).expect("writing to a String cannot fail");
    }
    out
}

pub(crate) fn cmd_slice(
    path: &str,
    symbol: Option<&str>,
    start: Option<usize>,
    end: Option<usize>,
    max_bytes: usize,
) -> ! {
    let p = Path::new(path);
    if !p.exists() {
        fail(format!("path does not exist: {path}"));
    }
    let src = match read_source(p) {
        Ok(s) => s,
        Err(v) => die(v),
    };
    let total_lines = src.lines().count().max(1);

    let (out_symbol, start_line, end_line, others): (
        Option<String>,
        usize,
        usize,
        Vec<serde_json::Value>,
    ) = if let Some(sym) = symbol {
        let lang = Lang::from_path(p);
        if !lang.is_tree_sitter() {
            fail(format!(
                "symbol lookup is unsupported for {}; use --start/--end",
                lang.name()
            ));
        }
        let matches = find_symbols(lang, &src, sym);
        if matches.is_empty() {
            fail(format!("no symbol matching '{sym}' in {path}"));
        }
        // Best match: exact qualified hit first, then bare name, then first.
        let best = matches
            .iter()
            .enumerate()
            .min_by_key(|(_, m)| {
                if m.qualified == sym {
                    0
                } else if m.name == sym {
                    1
                } else {
                    2
                }
            })
            .map(|(i, _)| i)
            .unwrap_or(0);
        let m = &matches[best];
        let others: Vec<_> = matches
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != best)
            .take(20)
            .map(|(_, o)| {
                json!({
                    "symbol": o.qualified,
                    "startLine": o.start_line,
                    "endLine": o.end_line,
                })
            })
            .collect();
        (
            Some(m.qualified.clone()),
            m.start_line,
            m.end_line.max(m.start_line),
            others,
        )
    } else if start.is_some() || end.is_some() {
        let s = start.unwrap_or(1).max(1);
        let e = end.unwrap_or(total_lines).min(total_lines);
        if s > e {
            fail(format!("invalid range: start {s} > end {e}"));
        }
        (None, s, e, Vec::new())
    } else {
        fail("slice requires --symbol <name> or --start/--end <lines>");
    };

    let content = format_lines(&src, start_line, end_line);
    let (content, truncated) = head_tail_bytes(&content, max_bytes);
    emit(&json!({
        "ok": true,
        "path": path,
        "symbol": out_symbol,
        "startLine": start_line,
        "endLine": end_line,
        "content": content,
        "otherMatches": others,
        "truncated": truncated,
    }));
    std::process::exit(0);
}
