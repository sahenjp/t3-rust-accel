//! `exec` — run a short Python 3 script from stdin inside `--cwd` with a
//! wall-clock timeout and Maki-style head+tail output truncation. On Unix the
//! child is also constrained with rlimits and killed as a process group.
//! On Windows a Job Object bounds memory and owns the process tree.
//! Helpers injected in the preamble let the script chain index/slice/grep/read
//! inside the workspace and print only the compact result.

use std::io::Write;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::process::{drain_capped, resolve_executable};
use crate::text::head_tail_bytes;
use crate::{emit, fail};

const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const MAX_TIMEOUT_MS: u64 = 30_000;
const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
#[cfg(not(target_os = "macos"))]
const PROCESS_MEMORY_BYTES: usize = 512 * 1024 * 1024;
// Linux RLIMIT_AS caps virtual address space (VSZ), including thread stacks,
// shared libraries, and allocator arenas rather than resident memory alone.
#[cfg(all(unix, not(target_os = "macos")))]
const UNIX_RLIMIT_AS_BYTES: u64 = 2 * 1024 * 1024 * 1024;
#[cfg(unix)]
const RLIMIT_CPU_SECS: u64 = 15;
#[cfg(unix)]
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
_WORKSPACE = _os.path.realpath(_os.path.abspath(_os.getcwd()))
_SKIP_DIRS = {".git", "node_modules", "target", "dist", ".venv", "__pycache__", ".next", ".turbo", ".t3", "build"}

def _inside_workspace(p):
    try:
        workspace = _os.path.normcase(_WORKSPACE)
        return _os.path.commonpath([workspace, _os.path.normcase(p)]) == workspace
    except (OSError, ValueError):
        # Windows raises ValueError when the two paths are on different drives.
        return False

def _abspath(p):
    p = _os.path.realpath(_os.path.abspath(_os.path.join(_WORKSPACE, str(p))))
    if not _inside_workspace(p):
        raise RuntimeError("path is outside workspace: " + str(p))
    return p

def _run_t3mt(args):
    proc = _subprocess.run([_T3MT] + args, capture_output=True, text=True, timeout=60)
    try:
        return _json.loads(proc.stdout or "{}")
    except Exception:
        return {"ok": False, "error": (proc.stderr or proc.stdout or "no output")[:500]}

def index(path, max_depth=3):
    """Maki-style tree-sitter skeleton of a file or directory inside the workspace."""
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
    from itertools import islice
    with open(p, "r", encoding="utf-8", errors="replace") as f:
        first = max(0, start - 1)
        return "".join(islice(f, first, max(first, end)))

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
        if not _inside_workspace(_os.path.realpath(fp)):
            continue
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
    """fnmatch-style file listing under path inside the workspace, skipping build dirs."""
    root = _abspath(path)
    out = []
    for fp in _iter_files(root):
        if not _inside_workspace(_os.path.realpath(fp)):
            continue
        rel = _os.path.relpath(fp, root)
        if _fnmatch_mod.fnmatch(rel, pattern) or _fnmatch_mod.fnmatch(_os.path.basename(fp), pattern):
            out.append(rel)
    return "\n".join(sorted(out))
"#;

/// Resolve the Python interpreter without hard-coding a Unix-only path.
fn python_interpreter() -> Result<PathBuf, String> {
    if let Some(custom) = std::env::var_os("T3_MAKI_PYTHON") {
        let custom = custom.to_string_lossy().to_string();
        return resolve_executable(&custom)
            .ok_or_else(|| format!("T3_MAKI_PYTHON is not an executable program: {custom}"));
    }
    let candidates: &[&str] = if cfg!(windows) {
        &["python", "py"]
    } else {
        &["python3", "python"]
    };
    for program in candidates {
        if let Some(path) = resolve_executable(program) {
            return Ok(path);
        }
    }
    #[cfg(not(windows))]
    for fallback in ["/usr/bin/python3", "/usr/local/bin/python3", "/bin/python3"] {
        if let Some(path) = resolve_executable(fallback) {
            return Ok(path);
        }
    }
    Err(format!(
        "cannot find a Python 3 interpreter ({}) on PATH; set T3_MAKI_PYTHON",
        candidates.join(" or ")
    ))
}

fn passthrough_keys() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &[
            "PATH",
            "PATHEXT",
            "COMSPEC",
            "SystemRoot",
            "WINDIR",
            "TEMP",
            "TMP",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
        ]
    }
    #[cfg(not(windows))]
    {
        &["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TZ"]
    }
}

fn valid_extra_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first == b'_' || first.is_ascii_alphabetic())
        && bytes.all(|b| b == b'_' || b.is_ascii_alphanumeric())
}

/// Replace the inherited environment with a small allowlist. Python scripts
/// are useful for local code inspection and should not receive API tokens or
/// other secrets by default. Operators can explicitly pass additional names
/// through T3_MAKI_EXEC_ENV_ALLOW when a workflow needs them.
fn apply_sanitized_env(cmd: &mut Command, exe: &PathBuf) {
    let mut inherited: Vec<(std::ffi::OsString, std::ffi::OsString)> = Vec::new();
    for key in passthrough_keys() {
        if let Some(value) = std::env::var_os(key) {
            inherited.push(((*key).into(), value));
        }
    }
    if let Ok(extra) = std::env::var("T3_MAKI_EXEC_ENV_ALLOW") {
        for name in extra
            .split(',')
            .map(str::trim)
            .filter(|n| valid_extra_env_name(n))
        {
            if let Some(value) = std::env::var_os(name) {
                inherited.push((name.into(), value));
            }
        }
    }

    cmd.env_clear();
    // Bound glibc per-thread malloc arenas and BLAS/OpenMP thread pools by default
    // so virtual address reservations stay compact under RLIMIT_AS.
    cmd.env("MALLOC_ARENA_MAX", "2")
        .env("OPENBLAS_NUM_THREADS", "1")
        .env("OMP_NUM_THREADS", "1");
    for (key, value) in inherited {
        cmd.env(key, value);
    }
    cmd.env("T3_MAKI_TOOLS", exe)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PYTHONHASHSEED", "0");
}

#[cfg(unix)]
fn configure_platform_child(cmd: &mut Command) {
    // New process group so the timeout can SIGKILL the whole tree.
    cmd.process_group(0);
    unsafe {
        cmd.pre_exec(|| {
            let set_limit = |resource, limit| {
                let limits = libc::rlimit {
                    rlim_cur: limit,
                    rlim_max: limit,
                };
                if libc::setrlimit(resource, &limits) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            };
            // Darwin rejects this limit when the child's existing address map
            // already exceeds it, before Python can start.
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                set_limit(libc::RLIMIT_DATA, PROCESS_MEMORY_BYTES as _)?;
                set_limit(libc::RLIMIT_AS, UNIX_RLIMIT_AS_BYTES as _)?;
            }
            set_limit(libc::RLIMIT_CPU, RLIMIT_CPU_SECS as _)?;
            set_limit(libc::RLIMIT_FSIZE, RLIMIT_FSIZE_BYTES as _)?;
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_platform_child(_cmd: &mut Command) {}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    // The child starts its own group before exec.
    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
}

#[cfg(windows)]
fn attach_job(child: &std::process::Child) -> std::io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::System::JobObjects::*;

    // Null attributes create a non-inheritable handle. Closing it terminates
    // all processes still in the job, including any children holding our pipes.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_JOB_MEMORY;
    limits.JobMemoryLimit = PROCESS_MEMORY_BYTES;
    let configured = unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&limits).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
    };
    if configured == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Python is still waiting for stdin. Assign it before sending any script.
    let assigned = unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) };
    if assigned == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(job)
}

pub(crate) fn cmd_exec(cwd: &str, timeout_ms: Option<u64>, max_bytes: usize, script: &[u8]) -> ! {
    let cwd_path = std::path::Path::new(cwd);
    if !cwd_path.is_dir() {
        fail(format!("--cwd is not a directory: {cwd}"));
    }
    let timeout = timeout_ms
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .clamp(1, MAX_TIMEOUT_MS);
    let python = match python_interpreter() {
        Ok(p) => p,
        Err(e) => fail(e),
    };
    let exe = std::env::current_exe().unwrap_or_else(|_| "t3-maki-tools".into());

    let mut program = Vec::with_capacity(PREAMBLE.len() + script.len() + 2);
    program.extend_from_slice(PREAMBLE.as_bytes());
    program.push(b'\n');
    program.extend_from_slice(script);

    let mut cmd = Command::new(&python);
    cmd.arg("-I")
        .arg("-B")
        .arg("-")
        .current_dir(cwd_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_sanitized_env(&mut cmd, &exe);
    configure_platform_child(&mut cmd);

    let started = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => fail(format!("cannot spawn {} -I: {e}", python.display())),
    };
    #[cfg(unix)]
    let pid = child.id();
    #[cfg(windows)]
    let mut job = Some(match attach_job(&child) {
        Ok(job) => job,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            fail(format!(
                "cannot place Python in a Windows Job Object: {error}"
            ));
        }
    });

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = out_tx.send(drain_capped(stdout, MAX_CAPTURE_BYTES));
    });
    std::thread::spawn(move || {
        let _ = err_tx.send(drain_capped(stderr, MAX_CAPTURE_BYTES));
    });

    // Large scripts must not block the timeout loop on a full stdin pipe.
    std::thread::spawn(move || {
        let _ = stdin.write_all(&program);
    });

    let deadline = started + Duration::from_millis(timeout);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    #[cfg(unix)]
                    kill_process_group(pid);
                    #[cfg(windows)]
                    drop(job.take());
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

    // The direct child can exit with descendants still holding its pipes.
    #[cfg(unix)]
    if !timed_out {
        kill_process_group(pid);
    }
    #[cfg(windows)]
    drop(job);

    let (stdout_bytes, stdout_total) = out_rx
        .recv_timeout(Duration::from_millis(200))
        .unwrap_or_default();
    let (stderr_bytes, stderr_total) = err_rx
        .recv_timeout(Duration::from_millis(200))
        .unwrap_or_default();
    let duration_ms = started.elapsed().as_millis() as u64;

    let stdout_text = String::from_utf8_lossy(&stdout_bytes);
    let stderr_text = String::from_utf8_lossy(&stderr_bytes);
    let (stdout_out, t1) = head_tail_bytes(&stdout_text, max_bytes);
    let (stderr_out, t2) = head_tail_bytes(&stderr_text, max_bytes);
    let stdout_capped = stdout_total > MAX_CAPTURE_BYTES as u64;
    let stderr_capped = stderr_total > MAX_CAPTURE_BYTES as u64;

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
        "truncated": t1 || t2 || stdout_capped || stderr_capped,
        "timedOut": timed_out,
        "durationMs": duration_ms,
    }));
    std::process::exit(0);
}
