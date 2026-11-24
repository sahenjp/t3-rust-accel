//! `diff --cwd <abs>` — compact `git diff` with lockfile/generated exclusions,
//! a shortstat + name-status header, and `-U2` hunks for token savings.

use std::path::Path;
use std::process::Command;

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde_json::json;

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

fn generated_set() -> GlobSet {
    let mut b = GlobSetBuilder::new();
    for g in GENERATED {
        b.add(Glob::new(g).unwrap());
        b.add(Glob::new(&format!("**/{g}")).unwrap());
    }
    b.build().unwrap()
}

fn git(cwd: &str, diff_args: &[&str], pathspec: &[String]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(cwd).arg("diff");
    cmd.args(diff_args);
    cmd.args(pathspec);
    let out = cmd
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "git diff failed ({}): {}",
            out.status,
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn parse_shortstat(stat: &str) -> (u64, u64, u64) {
    let mut files = 0u64;
    let mut ins = 0u64;
    let mut del = 0u64;
    for part in stat.split(',') {
        let n: u64 = part
            .trim()
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

pub fn cmd_diff(cwd: &str, rev: Option<&str>, path: Option<&str>, max_bytes: usize) -> ! {
    if !Path::new(cwd).is_dir() {
        fail(format!("cwd is not a directory: {cwd}"));
    }
    let gen = generated_set();
    let path_targets_generated = path.map(|p| gen.is_match(p)).unwrap_or(false);

    let mut spec: Vec<String> = vec!["--".into()];
    if let Some(p) = path {
        spec.push(p.to_string());
    }
    if !path_targets_generated {
        for g in GENERATED {
            spec.push(format!(":!{g}"));
        }
    }

    let rev_args: Vec<&str> = rev.into_iter().collect();

    // Stats + name-status header (with exclusions applied).
    let mut stat_args = vec!["--shortstat"];
    stat_args.extend(&rev_args);
    let shortstat = match git(cwd, &stat_args, &spec) {
        Ok(s) => s,
        Err(e) => fail(e),
    };
    let (files_changed, insertions, deletions) = parse_shortstat(&shortstat);

    let mut ns_args = vec!["--name-status"];
    ns_args.extend(&rev_args);
    let name_status = match git(cwd, &ns_args, &spec) {
        Ok(s) => s,
        Err(e) => fail(e),
    };

    // How many changed files were skipped by the generated-file exclusions?
    let mut skipped_generated = 0usize;
    if !path_targets_generated {
        let mut names_spec: Vec<String> = vec!["--".into()];
        if let Some(p) = path {
            names_spec.push(p.to_string());
        }
        let mut names_args = vec!["--name-only"];
        names_args.extend(&rev_args);
        if let Ok(names) = git(cwd, &names_args, &names_spec) {
            skipped_generated = names
                .lines()
                .filter(|l| !l.is_empty() && gen.is_match(l))
                .count();
        }
    }

    let mut body_args = vec!["-U2", "--no-color"];
    body_args.extend(&rev_args);
    let body = match git(cwd, &body_args, &spec) {
        Ok(s) => s,
        Err(e) => fail(e),
    };

    let mut diff_out = String::new();
    if !shortstat.trim().is_empty() {
        diff_out.push_str(shortstat.trim());
        diff_out.push('\n');
    }
    if skipped_generated > 0 {
        diff_out.push_str(&format!("{skipped_generated} generated/lock files skipped\n"));
    }
    let mut shown_files = 0usize;
    for line in name_status.lines() {
        if shown_files >= MAX_HEADER_FILES {
            let rest = name_status.lines().count() - shown_files;
            diff_out.push_str(&format!("[... {rest} more files]\n"));
            break;
        }
        let (status, file) = line.split_once('\t').unwrap_or((line, ""));
        diff_out.push_str(&format!("{status} {file}\n"));
        shown_files += 1;
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
        "truncated": truncated,
        "diff": diff_out,
    }));
    std::process::exit(0);
}
