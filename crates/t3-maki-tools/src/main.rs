use std::io::Read;
use std::process::exit;

mod diff;
mod dirindex;
mod econ;
mod exec_py;
mod grep;
mod process;
mod skeleton;
mod slice;
mod text;

use serde_json::{json, Value};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_STDIN_BYTES: u64 = 16 * 1024 * 1024;

const USAGE: &str = "t3-maki-tools — Maki-grade tree-sitter index/slice, AST grep, compact git diff, resource-limited exec, dsh econ

Usage: t3-maki-tools <command> [options]

Commands:
  index --path <abs> [--max-depth <n>] [--max-lines <n>]
        Structural skeleton of a file or directory (tree-sitter, 2 MiB cap).
  slice --path <abs> [--symbol <name>] [--start <n>] [--end <n>] [--max-bytes <n>]
        Extract a symbol body or a 1-based inclusive line range as 'L<n>:' lines.
  grep --pattern <regex> --path <abs> [--glob <pat>] [--max-hits <n>] [--max-bytes <n>]
        Regex search grouped by file with enclosing tree-sitter symbol
        annotations. Skips .git/node_modules/target/dist, binaries,
        lockfiles and minified files (single-file --path exempt).
  diff --cwd <abs> [--rev <git-rev-or-range>] [--path <rel>] [--max-bytes <n>]
        Compact git diff: shortstat + name-status header, -U2 hunks,
        generated/lock files excluded and counted as skippedGenerated.
  exec --cwd <abs> [--timeout-ms <n>] [--max-bytes <n>]
        Run a trusted Python 3 script from stdin in --cwd with process limits,
        timeout and output truncation. Helpers: index, slice_code, read,
        grep, glob_files.
  econ
        Read compaction CostInput+Prices JSON from stdin; emit break-even
        economics (port of the reference native compaction cost model).

Options:
  -h, --help       Print this help
  -V, --version    Print version

Every command prints a single JSON object: {\"ok\": true, ...} or
{\"ok\": false, \"error\": \"...\"} (exit 1).";

/// Print one JSON object to stdout.
pub(crate) fn emit(v: &Value) {
    println!(
        "{}",
        serde_json::to_string(v).unwrap_or_else(|_| "{}".into())
    );
}

/// Print `{ "ok": false, "error": ... }` and exit 1.
pub(crate) fn fail(msg: impl std::fmt::Display) -> ! {
    emit(&json!({ "ok": false, "error": msg.to_string() }));
    std::process::exit(1);
}

struct Args {
    flags: std::collections::HashMap<String, String>,
}

const VALUE_FLAGS: &[&str] = &[
    "path",
    "cwd",
    "pattern",
    "glob",
    "rev",
    "symbol",
    "max-depth",
    "max-lines",
    "max-bytes",
    "max-hits",
    "timeout-ms",
    "start",
    "end",
];

fn print_help() -> ! {
    println!("{USAGE}");
    exit(0);
}

fn parse_args(argv: &[String], command: &str, allowed: &[&str]) -> Args {
    let mut flags = std::collections::HashMap::new();
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        if arg == "-h" || arg == "--help" {
            print_help();
        }
        if let Some(flag) = arg.strip_prefix("--") {
            let (key, inline_value) = match flag.split_once('=') {
                Some((key, value)) => (key, Some(value.to_string())),
                None => (flag, None),
            };
            if !VALUE_FLAGS.contains(&key) {
                fail(format!("unknown flag --{key} for {command}"));
            }
            if !allowed.contains(&key) {
                fail(format!("flag --{key} is not valid for {command}"));
            }
            let value = match inline_value {
                Some(value) => value,
                None => {
                    i += 1;
                    if i >= argv.len() {
                        fail(format!("missing value for --{key}"));
                    }
                    argv[i].clone()
                }
            };
            flags.insert(key.to_string(), value);
            i += 1;
        } else if arg.starts_with('-') {
            fail(format!("unknown flag {arg} for {command}"));
        } else {
            fail(format!(
                "unexpected positional argument for {command}: {arg}"
            ));
        }
    }
    Args { flags }
}

impl Args {
    fn get(&self, k: &str) -> Option<&str> {
        self.flags.get(k).map(|s| s.as_str())
    }
    fn get_req(&self, k: &str) -> &str {
        match self.flags.get(k) {
            Some(v) => v.as_str(),
            None => fail(format!("missing required --{k}")),
        }
    }
    fn get_usize(&self, k: &str, default: usize) -> usize {
        match self.flags.get(k) {
            Some(v) => v.parse().unwrap_or_else(|_| {
                fail(format!("invalid integer for --{k}: '{v}'"));
            }),
            None => default,
        }
    }
    fn get_opt_usize(&self, k: &str) -> Option<usize> {
        self.flags.get(k).map(|v| {
            v.parse().unwrap_or_else(|_| {
                fail(format!("invalid integer for --{k}: '{v}'"));
            })
        })
    }
    fn get_u64(&self, k: &str) -> Option<u64> {
        self.flags.get(k).map(|v| {
            v.parse().unwrap_or_else(|_| {
                fail(format!("invalid integer for --{k}: '{v}'"));
            })
        })
    }
}

fn read_stdin() -> Vec<u8> {
    let mut buf = Vec::new();
    let mut limited = std::io::stdin().take(MAX_STDIN_BYTES + 1);
    if let Err(e) = limited.read_to_end(&mut buf) {
        fail(format!("cannot read stdin: {e}"));
    }
    if buf.len() as u64 > MAX_STDIN_BYTES {
        fail(format!(
            "stdin exceeds the {} MiB input limit",
            MAX_STDIN_BYTES / (1024 * 1024)
        ));
    }
    buf
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let first = argv.first().map(|s| s.as_str()).unwrap_or("");
    match first {
        "-V" | "--version" | "version" => {
            println!("t3-maki-tools {VERSION}");
            exit(0);
        }
        "" | "-h" | "--help" | "help" => {
            println!("{USAGE}");
            exit(if first.is_empty() { 1 } else { 0 });
        }
        "index" => {
            let a = parse_args(&argv[1..], "index", &["path", "max-depth", "max-lines"]);
            let path = a.get_req("path").to_string();
            skeleton::cmd_index(
                &path,
                a.get_usize("max-depth", 3),
                a.get_usize("max-lines", 250),
            );
        }
        "slice" => {
            let a = parse_args(
                &argv[1..],
                "slice",
                &["path", "symbol", "start", "end", "max-bytes"],
            );
            let path = a.get_req("path").to_string();
            slice::cmd_slice(
                &path,
                a.get("symbol"),
                a.get_opt_usize("start"),
                a.get_opt_usize("end"),
                a.get_usize("max-bytes", 16384),
            );
        }
        "exec" => {
            let a = parse_args(&argv[1..], "exec", &["cwd", "timeout-ms", "max-bytes"]);
            let cwd = a.get_req("cwd").to_string();
            let script = read_stdin();
            exec_py::cmd_exec(
                &cwd,
                a.get_u64("timeout-ms"),
                a.get_usize("max-bytes", 16384),
                &script,
            );
        }
        "grep" => {
            let a = parse_args(
                &argv[1..],
                "grep",
                &["pattern", "path", "glob", "max-hits", "max-bytes"],
            );
            let pattern = a.get_req("pattern").to_string();
            let path = a.get_req("path").to_string();
            grep::cmd_grep(
                &pattern,
                &path,
                a.get("glob"),
                a.get_usize("max-hits", 50),
                a.get_usize("max-bytes", 12000),
            );
        }
        "diff" => {
            let a = parse_args(&argv[1..], "diff", &["cwd", "rev", "path", "max-bytes"]);
            let cwd = a.get_req("cwd").to_string();
            diff::cmd_diff(
                &cwd,
                a.get("rev"),
                a.get("path"),
                a.get_usize("max-bytes", 14000),
            );
        }
        "econ" => {
            parse_args(&argv[1..], "econ", &[]);
            let input = read_stdin();
            let text = String::from_utf8_lossy(&input);
            econ::cmd_econ(&text);
        }
        other => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            exit(2);
        }
    }
}
