//! `exec` — run a short Python 3 script from stdin inside `--cwd` with hard
//! rlimits, a wall-clock timeout (SIGKILL), and Maki-style head+tail output
//! truncation. Helpers injected in the preamble let the script chain
//! index/slice/grep/read locally and print only the compact result.

use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::text::head_tail_bytes;
use crate::{emit, fail};

const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const MAX_TIMEOUT_MS: u64 = 30_000;
const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
const RLIMIT_AS_BYTES: u64 = 512 * 1024 * 1024;
const RLIMIT_CPU_SECS: u64 = 15;
const RLIMIT_FSIZE_BYTES: u64 = 4 * 1024 * 1024;

/// Python helpers available directly in script scope. `T3_MAKI_TOOLS` is set
/// to this binary's own path by the Rust side before spawn.
const PREAMBLE: &str = r#"
import fnmatch as _fnmatch_mod
import json as _json
import os as _os
import re as _re
import subprocess as _subprocess

_T3MT = _os.environ.get("T3_MAKI_TOOLS", "t3-maki-tools")
_WORKSPACE = _os.path.abspath(_os.getcwd())
_SKIP_DIRS = {".git", "node_modules", "target", "dist", ".venv", "__pycache__", ".next", ".turbo", ".t3", "build"}

def _abspath(p):
    p = _os.path.abspath(_os.path.join(_WORKSPACE, str(p)))
    return p

def _run_t3mt(args):
    proc = _subprocess.run([_T3MT] + args, capture_output=True, text=True, timeout=60)
    try:
        return _json.loads(proc.stdout or "{}")
    except Exception:
        return {"ok": False, "error": (proc.stderr or proc.stdout or "no output")[:500]}

def index(path, max_depth=3):
    """Maki-style tree-sitter skeleton of a file or directory."""
    r = _run_t3mt(["index", "--path", _abspath(path), "--max-depth", str(max_depth)])
    if not r.get("ok"):
        raise RuntimeError(r.get("error", "index failed"))
    return r["skeleton"]

def slice_code(path, symbol=None, start=None, end=None, max_bytes=16384):
    """Extract a symbol body or 1-based inclusive line range as 'L<n>:' lines."""
    args = ["slice", "--path", _abspath(path), "--max-bytes", str(max_bytes)]
    if symbol is not None:
        args += ["--symbol", str(symbol)]
    if start is not None:
        args += ["--start", str(start)]
    if end is not None:
        args += ["--end", str(end)]
    r = _run_t3mt(args)
    if not r.get("ok"):
        raise RuntimeError(r.get("error", "slice failed"))
    return r["content"]

def read(path, start=1, end=200):
    """Read a 1-based inclusive line range of a file inside the workspace."""
    p = _abspath(path)
    if _os.path.commonpath([_WORKSPACE, p]) != _WORKSPACE:
        raise RuntimeError("read() is restricted to the workspace: " + str(p))
    with open(p, "r", encoding="utf-8", errors="replace") as f:
        lines = f.readlines()
    return "".join(lines[max(0, start - 1):end])

def _iter_files(root):
    for dirpath, dirnames, filenames in _os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in _SKIP_DIRS]
        for fn in filenames:
            yield _os.path.join(dirpath, fn)

def grep(pattern, path=".", max_hits=50):
    """Regex search over workspace files, skipping build dirs. Returns 'path:line:text'."""
    rx = _re.compile(pattern)
    root = _abspath(path)
    files = [root] if _os.path.isfile(root) else _iter_files(root)
    base = root if _os.path.isdir(root) else _os.path.dirname(root)
    hits = []
    for fp in files:
        try:
            with open(fp, "r", encoding="utf-8", errors="replace") as f:
                for i, line in enumerate(f, 1):
                    if rx.search(line):
                        hits.append(_os.path.relpath(fp, base) + ":" + str(i) + ":" + line.rstrip("\n"))
                        if len(hits) >= max_hits:
                            return "\n".join(hits)
        except (OSError, UnicodeDecodeError):
            continue
    return "\n".join(hits)

def glob_files(pattern, path="."):
    """fnmatch-style file listing under path, skipping build dirs."""
    root = _abspath(path)
    out = []
    for fp in _iter_files(root):
        rel = _os.path.relpath(fp, root)
        if _fnmatch_mod.fnmatch(rel, pattern) or _fnmatch_mod.fnmatch(_os.path.basename(fp), pattern):
            out.append(rel)
    return "\n".join(sorted(out))
"#;

fn setrlimit(resource: libc::__rlimit_resource_t, cur: u64, max: u64) -> std::io::Result<()> {
    let rl = libc::rlimit {
        rlim_cur: cur as libc::rlim_t,
        rlim_max: max as libc::rlim_t,
    };
    if unsafe { libc::setrlimit(resource, &rl as *const libc::rlimit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Read a pipe to EOF, keeping at most `cap` bytes (the stream is still
/// drained so the child never blocks on a full pipe).
fn drain_capped<R: Read>(mut r: R, cap: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.len() < cap {
                    let room = cap - out.len();
                    out.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
    }
    out
}

pub fn cmd_exec(cwd: &str, timeout_ms: Option<u64>, max_bytes: usize, script: &[u8]) -> ! {
    let cwd_path = std::path::Path::new(cwd);
    if !cwd_path.is_dir() {
        fail(format!("--cwd is not a directory: {cwd}"));
    }
    let timeout = timeout_ms
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .clamp(1, MAX_TIMEOUT_MS);
    let exe = std::env::current_exe().unwrap_or_else(|_| "t3-maki-tools".into());

    let mut program = Vec::with_capacity(PREAMBLE.len() + script.len() + 2);
    program.extend_from_slice(PREAMBLE.as_bytes());
    program.push(b'\n');
    program.extend_from_slice(script);

    let mut cmd = Command::new("/usr/bin/python3");
    cmd.arg("-I")
        .arg("-")
        .current_dir(cwd_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("T3_MAKI_TOOLS", &exe)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PYTHONHASHSEED", "0");
    // New process group so the timeout can SIGKILL the whole tree.
    cmd.process_group(0);
    unsafe {
        cmd.pre_exec(|| {
            setrlimit(libc::RLIMIT_AS, RLIMIT_AS_BYTES, RLIMIT_AS_BYTES)?;
            setrlimit(libc::RLIMIT_CPU, RLIMIT_CPU_SECS, RLIMIT_CPU_SECS)?;
            setrlimit(libc::RLIMIT_FSIZE, RLIMIT_FSIZE_BYTES, RLIMIT_FSIZE_BYTES)?;
            setrlimit(libc::RLIMIT_NOFILE, 256, 256)?;
            Ok(())
        });
    }

    let started = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => fail(format!("cannot spawn /usr/bin/python3 -I: {e}")),
    };
    let pid = child.id() as i32;

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out_thread = std::thread::spawn(move || drain_capped(stdout, MAX_CAPTURE_BYTES));
    let err_thread = std::thread::spawn(move || drain_capped(stderr, MAX_CAPTURE_BYTES));

    // Feed the program; ignore EPIPE if the child exits early.
    let _ = stdin.write_all(&program);
    drop(stdin);

    let deadline = started + Duration::from_millis(timeout);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                    let _ = child.kill();
                    break match child.wait() {
                        Ok(s) => s,
                        Err(e) => fail(format!("wait after kill failed: {e}")),
                    };
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => fail(format!("wait failed: {e}")),
        }
    };

    let stdout_bytes = out_thread.join().unwrap_or_default();
    let stderr_bytes = err_thread.join().unwrap_or_default();
    let duration_ms = started.elapsed().as_millis() as u64;

    let stdout_text = String::from_utf8_lossy(&stdout_bytes);
    let stderr_text = String::from_utf8_lossy(&stderr_bytes);
    let (stdout_out, t1) = head_tail_bytes(&stdout_text, max_bytes);
    let (stderr_out, t2) = head_tail_bytes(&stderr_text, max_bytes);

    let exit_code = if timed_out {
        -9
    } else {
        status.code().unwrap_or(-1)
    };
    emit(&json!({
        "ok": true,
        "exitCode": exit_code,
        "stdout": stdout_out,
        "stderr": stderr_out,
        "truncated": t1 || t2 || stdout_bytes.len() >= MAX_CAPTURE_BYTES || stderr_bytes.len() >= MAX_CAPTURE_BYTES,
        "timedOut": timed_out,
        "durationMs": duration_ms,
    }));
    std::process::exit(0);
}
