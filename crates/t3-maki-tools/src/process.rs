//! Shared process-launch helpers.

use std::path::{Path, PathBuf};

/// True when `path` is a regular file that can be executed.
fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// Candidate pathnames for a bare executable name in one PATH entry.
fn candidates_in(dir: &Path, program: &str) -> Vec<PathBuf> {
    let exact = dir.join(program);
    if !cfg!(windows) || Path::new(program).extension().is_some() {
        return vec![exact];
    }

    let mut out = vec![exact];
    // Git for Windows and Python are .exe launchers. Batch shims are
    // deliberately not considered: std::process::Command does not provide a
    // shell and batch files would need cmd.exe.
    for ext in [".exe", ".com"] {
        out.push(dir.join(format!("{program}{ext}")));
    }
    out
}

/// Resolve a program name to an absolute executable path using PATH.
///
/// Relative and empty PATH entries are intentionally ignored. In particular,
/// this prevents Windows process creation from picking up a `git.exe` or
/// `python.exe` planted in the current directory.
pub fn resolve_executable(program: &str) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    let direct = Path::new(program);
    let has_separator = program.contains('/')
        || program.contains('\\')
        || direct.is_absolute()
        || direct.components().count() > 1;
    if has_separator {
        return is_executable_file(direct).then(|| direct.to_path_buf());
    }

    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        if dir.as_os_str().is_empty() || !dir.is_absolute() {
            continue;
        }
        for candidate in candidates_in(&dir, program) {
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_a_host_executable_from_path() {
        let program = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(resolve_executable(program).is_some());
    }

    #[test]
    fn rejects_empty_and_missing_programs() {
        assert_eq!(resolve_executable(""), None);
        assert_eq!(resolve_executable("definitely-not-a-t3mt-program"), None);
    }
}
