//! `diff --cwd <abs>` — compact `git diff` with lockfile/generated exclusions,
//! a shortstat + name-status header, and `-U2` hunks for token savings.

use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde_json::json;

use crate::process::{drain_capped, resolve_executable};
use crate::text::head_tail_bytes;
use crate::{emit, fail};

const GENERATED: &[&str] = &[
    "pnpm-lock.yaml",
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "*.map",
    "*.min.js",
    "*.min.css",
];

const MAX_HEADER_FILES: usize = 60;
const STDERR_CAP_BYTES: usize = 64 * 1024;

fn generated_set() -> GlobSet {
    let mut b = GlobSetBuilder::new();
    for g in GENERATED {
        b.add(Glob::new(g).unwrap());
        b.add(Glob::new(&format!("**/{g}")).unwrap());
    }
    b.build().unwrap()
}

fn git_bin() -> Result<PathBuf, String> {
    static GIT_BIN: OnceLock<Option<PathBuf>> = OnceLock::new();
    GIT_BIN
        .get_or_init(|| resolve_executable("git"))
        .clone()
        .ok_or_else(|| "cannot find git on PATH".to_string())
}

fn valid_rev(rev: &str) -> bool {
    // A revision/range is passed before `--`; never allow it to become another
    // git option. Whitespace remains safe because exec arguments are not
    // shell-split and is valid in forms such as `HEAD@{2 days ago}`.
    !rev.is_empty() && !rev.starts_with('-')
}

fn validate_diff_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("invalid --path: path is empty".to_string());
    }
    if path.bytes().any(|b| b == 0) {
        return Err("invalid --path: NUL bytes are not allowed".to_string());
    }
    if path.starts_with(':') {
        return Err("invalid --path: git pathspec magic is not allowed".to_string());
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err("invalid --path: absolute paths are not allowed".to_string());
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err("invalid --path: Windows drive prefixes are not allowed".to_string());
    }
    if Path::new(path).components().any(|c| {
        matches!(
            c,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        )
    }) {
        return Err(
            "invalid --path: parent, root, and prefix components are not allowed".to_string(),
        );
    }
    Ok(())
}

fn literal_pathspec(path: &str) -> String {
    format!(":(literal){path}")
}

fn base_pathspec(path: Option<&str>) -> Vec<String> {
    let mut spec = vec!["--".to_string()];
    if let Some(path) = path {
        spec.push(literal_pathspec(path));
    }
    spec
}

fn excluded_pathspec(path: Option<&str>, path_targets_generated: bool) -> Vec<String> {
    let mut spec = base_pathspec(path);
    if !path_targets_generated {
        for generated in GENERATED {
            spec.push(format!(":!{generated}"));
        }
    }
    spec
}

fn git(
    cwd: &str,
    diff_args: &[&str],
    pathspec: &[String],
    stdout_cap: usize,
) -> Result<(String, bool), String> {
    let bin = git_bin()?;
    let mut cmd = Command::new(bin);
    cmd.env("LC_ALL", "C");
    cmd.args([
        "--no-pager",
        "-c",
        "core.quotePath=false",
        "-C",
        cwd,
        "diff",
        "--no-ext-diff",
        "--no-textconv",
    ]);
    cmd.args(diff_args);
    cmd.args(pathspec);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("cannot run git: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "cannot capture git stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "cannot capture git stderr".to_string())?;
    let out_thread = std::thread::spawn(move || drain_capped(stdout, stdout_cap));
    let err_thread = std::thread::spawn(move || drain_capped(stderr, STDERR_CAP_BYTES));
    let status = child
        .wait()
        .map_err(|e| format!("cannot wait for git: {e}"))?;
    let (stdout_bytes, stdout_total) = out_thread
        .join()
        .map_err(|_| "git stdout reader panicked".to_string())?;
    let (stderr_bytes, _) = err_thread
        .join()
        .map_err(|_| "git stderr reader panicked".to_string())?;
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        return Err(format!("git diff failed ({}): {}", status, stderr.trim()));
    }
    Ok((
        String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stdout_total > stdout_cap as u64,
    ))
}

fn parse_shortstat(stat: &str) -> (u64, u64, u64) {
    let mut files = 0u64;
    let mut ins = 0u64;
    let mut del = 0u64;
    for part in stat.split(',') {
        let n: u64 = part
            .split_whitespace()
            .next()
            .and_then(|t| t.parse().ok())
            .unwrap_or(0);
        if part.contains("file") {
            files = n;
        } else if part.contains("insertion") {
            ins = n;
        } else if part.contains("deletion") {
            del = n;
        }
    }
    (files, ins, del)
}

struct StatusEntry {
    status: String,
    old_path: String,
    new_path: Option<String>,
    generated: bool,
}

impl StatusEntry {
    fn render(&self) -> String {
        match &self.new_path {
            Some(new_path) => format!("{} {} -> {}", self.status, self.old_path, new_path),
            None => format!("{} {}", self.status, self.old_path),
        }
    }
}

fn parse_name_status(name_status: &str, gen: &GlobSet) -> Vec<StatusEntry> {
    name_status
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut parts = line.split('\t');
            let status = parts.next().unwrap_or("").to_string();
            let old_path = parts.next().unwrap_or("").to_string();
            let new_path = if matches!(status.chars().next(), Some('R' | 'C')) {
                parts.next().map(str::to_string)
            } else {
                None
            };
            let generated =
                gen.is_match(&old_path) || new_path.as_ref().is_some_and(|path| gen.is_match(path));
            StatusEntry {
                status,
                old_path,
                new_path,
                generated,
            }
        })
        .collect()
}

pub(crate) fn cmd_diff(cwd: &str, rev: Option<&str>, path: Option<&str>, max_bytes: usize) -> ! {
    if !Path::new(cwd).is_dir() {
        fail(format!("cwd is not a directory: {cwd}"));
    }
    if let Some(path) = path {
        if let Err(error) = validate_diff_path(path) {
            fail(error);
        }
    }
    let gen = generated_set();
    let path_targets_generated = path.map(|p| gen.is_match(p)).unwrap_or(false);
    let capture_cap = max_bytes.max(64 * 1024).saturating_add(4096);

    if let Some(r) = rev {
        if !valid_rev(r) {
            fail(format!(
                "invalid --rev '{r}': expected a single revision or range, not an option"
            ));
        }
    }
    let rev_args: Vec<&str> = rev.into_iter().collect();
    let filtered_spec = excluded_pathspec(path, path_targets_generated);
    let unfiltered_spec = base_pathspec(path);

    let mut stat_args = vec!["--shortstat"];
    stat_args.extend(&rev_args);
    let (shortstat, _) = match git(cwd, &stat_args, &filtered_spec, capture_cap) {
        Ok(s) => s,
        Err(e) => fail(e),
    };
    let (files_changed, insertions, deletions) = parse_shortstat(&shortstat);

    let mut ns_args = vec!["--name-status"];
    ns_args.extend(&rev_args);
    let (name_status, _) = match git(cwd, &ns_args, &unfiltered_spec, capture_cap) {
        Ok(s) => s,
        Err(e) => fail(e),
    };
    let entries = parse_name_status(&name_status, &gen);
    let skipped_generated = if path_targets_generated {
        0
    } else {
        entries.iter().filter(|entry| entry.generated).count()
    };
    let shown_entries: Vec<&StatusEntry> = entries
        .iter()
        .filter(|entry| path_targets_generated || !entry.generated)
        .collect();

    let mut body_args = vec!["-U2", "--no-color"];
    body_args.extend(&rev_args);
    let (body, stdout_capped) = match git(cwd, &body_args, &filtered_spec, capture_cap) {
        Ok(s) => s,
        Err(e) => fail(e),
    };

    let mut diff_out = String::new();
    if !shortstat.trim().is_empty() {
        diff_out.push_str(shortstat.trim());
        diff_out.push('\n');
    }
    if skipped_generated > 0 {
        diff_out.push_str(&format!(
            "{skipped_generated} generated/lock files skipped\n"
        ));
    }
    for (shown_files, entry) in shown_entries.iter().enumerate() {
        if shown_files >= MAX_HEADER_FILES {
            let rest = shown_entries.len() - shown_files;
            diff_out.push_str(&format!("[... {rest} more files]\n"));
            break;
        }
        diff_out.push_str(&entry.render());
        diff_out.push('\n');
    }
    if !diff_out.is_empty() && !body.is_empty() {
        diff_out.push('\n');
    }
    diff_out.push_str(&body);

    let (diff_out, truncated) = head_tail_bytes(&diff_out, max_bytes);
    emit(&json!({
        "ok": true,
        "cwd": cwd,
        "rev": rev,
        "filesChanged": files_changed,
        "insertions": insertions,
        "deletions": deletions,
        "skippedGenerated": skipped_generated,
        "truncated": truncated || stdout_capped,
        "diff": diff_out,
    }));
    std::process::exit(0);
}
