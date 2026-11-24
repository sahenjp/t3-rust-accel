use std::io::Read;
use std::process::exit;

use t3_maki_tools::{diff, econ, exec_py, grep, prune, skeleton, slice};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "t3-maki-tools — Maki-grade tree-sitter index/slice, AST grep, compact git diff, sandboxed exec, dsh econ/prune

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
        Run a Python 3 script from stdin inside --cwd (rlimits + SIGKILL
        timeout + head/tail truncation). Helpers: index, slice_code, read,
        grep, glob_files.
  econ
        Read compaction CostInput+Prices JSON from stdin; emit break-even
        economics (port of the reference native compaction cost model).
  prune [--max-chars <n>]
        Read text from stdin; head+tail prune to the char budget.

Options:
  -h, --help       Print this help
  -V, --version    Print version

Every command prints a single JSON object: {\"ok\": true, ...} or
{\"ok\": false, \"error\": \"...\"} (exit 1).";

struct Args {
    positional: Vec<String>,
    flags: std::collections::HashMap<String, String>,
    bools: std::collections::HashSet<String>,
}

fn parse_args(argv: &[String]) -> Args {
    let mut a = Args {
        positional: Vec::new(),
        flags: std::collections::HashMap::new(),
        bools: std::collections::HashSet::new(),
    };
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        if arg.starts_with("--") {
            let key = arg[2..].to_string();
            if i + 1 < argv.len() && !argv[i + 1].starts_with("--") {
                a.flags.insert(key, argv[i + 1].clone());
                i += 2;
            } else {
                a.bools.insert(key);
                i += 1;
            }
        } else {
            a.positional.push(arg.clone());
            i += 1;
        }
    }
    a
}

impl Args {
    fn get(&self, k: &str) -> Option<&str> {
        self.flags.get(k).map(|s| s.as_str())
    }
    fn get_req(&self, k: &str) -> &str {
        match self.flags.get(k) {
            Some(v) => v.as_str(),
            None => t3_maki_tools::fail(format!("missing required --{k}")),
        }
    }
    fn get_usize(&self, k: &str, default: usize) -> usize {
        self.flags
            .get(k)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
    fn get_opt_usize(&self, k: &str) -> Option<usize> {
        self.flags.get(k).and_then(|v| v.parse().ok())
    }
    fn get_u64(&self, k: &str) -> Option<u64> {
        self.flags.get(k).and_then(|v| v.parse().ok())
    }
}

fn read_stdin() -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut buf);
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
            let a = parse_args(&argv[1..]);
            if a.bools.contains("help") {
                println!("{USAGE}");
                exit(0);
            }
            let path = a.get_req("path").to_string();
            skeleton::cmd_index(
                &path,
                a.get_usize("max-depth", 3),
                a.get_usize("max-lines", 250),
            );
        }
        "slice" => {
            let a = parse_args(&argv[1..]);
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
            let a = parse_args(&argv[1..]);
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
            let a = parse_args(&argv[1..]);
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
            let a = parse_args(&argv[1..]);
            let cwd = a.get_req("cwd").to_string();
            diff::cmd_diff(
                &cwd,
                a.get("rev"),
                a.get("path"),
                a.get_usize("max-bytes", 14000),
            );
        }
        "econ" => {
            let input = read_stdin();
            let text = String::from_utf8_lossy(&input);
            econ::cmd_econ(&text);
        }
        "prune" => {
            let a = parse_args(&argv[1..]);
            let input = read_stdin();
            let text = String::from_utf8_lossy(&input);
            prune::cmd_prune(a.get_usize("max-chars", 16384), &text);
        }
        other => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            exit(2);
        }
    }
}
