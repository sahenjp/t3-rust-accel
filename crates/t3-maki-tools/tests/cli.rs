//! End-to-end CLI tests against the real `t3-maki-tools` binary.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_t3-maki-tools");

fn tmpdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("t3mt_test_{}_{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn run(args: &[&str], stdin: &[u8]) -> (Value, i32) {
    run_with_env(args, stdin, &[])
}

fn run_with_env(args: &[&str], stdin: &[u8], envs: &[(&str, &str)]) -> (Value, i32) {
    let mut cmd = Command::new(BIN);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    let code = out.status.code().unwrap_or(-1);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "bad JSON from {args:?}: {e}\n{}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    (v, code)
}

/// Quote a string as a Python literal. JSON string escaping is compatible for
/// the path text used here and, unlike the current tests' original single
/// quotes, remains correct when Windows paths contain backslashes.
fn py_string(text: &str) -> String {
    serde_json::to_string(text).unwrap()
}

fn ok(v: &Value) -> &Value {
    assert_eq!(v["ok"], true, "expected ok: {v}");
    v
}

// ---------------- index: file mode ----------------

const RUST_SRC: &str = r#"use std::io;
use std::fmt::Write as _;

const MAX: usize = 42;

pub struct Foo {
    x: u8,
}

pub enum Kind { A, B }

impl Foo {
    pub fn new(x: u8) -> Self { Self { x } }
    fn bar(&self) -> u8 { self.x + 1 }
}

pub trait Greet {
    fn hello(&self) -> String;
}

fn helper() -> u8 { 7 }

mod inner {
    pub fn nested() {}
}
"#;

#[test]
fn index_rust_file() {
    let dir = tmpdir("rust");
    let f = dir.join("a.rs");
    write(&f, RUST_SRC);
    let (v, code) = run(&["index", "--path", f.to_str().unwrap()], b"");
    assert_eq!(code, 0);
    let v = ok(&v);
    assert_eq!(v["language"], "rust");
    let sk = v["skeleton"].as_str().unwrap();
    assert!(sk.contains("imports (2): [1-2]"), "{sk}");
    assert!(sk.contains("struct Foo"), "{sk}");
    assert!(sk.contains("enum Kind"), "{sk}");
    assert!(sk.contains("impl Foo"), "{sk}");
    assert!(sk.contains("  pub fn new(x: u8) -> Self"), "{sk}"); // indented member
    assert!(sk.contains("trait Greet"), "{sk}");
    assert!(sk.contains("fn hello(&self) -> String;"), "{sk}");
    assert!(sk.contains("fn helper() -> u8"), "{sk}");
    // Signatures only: no function bodies in the skeleton.
    assert!(!sk.contains("self.x + 1"), "{sk}");
    assert!(!sk.contains("Self { x }"), "{sk}");
}

#[test]
fn index_typescript_file() {
    let dir = tmpdir("ts");
    let f = dir.join("b.ts");
    write(
        &f,
        "import { a } from \"./a\";\nimport { b } from \"./b\";\n\nexport interface Cfg {\n  readonly x: number;\n  run(): void;\n}\n\nexport class Impl implements Cfg {\n  readonly x = 1;\n  run(): void { console.log(1); }\n  private helper(): number { return 2; }\n}\n\nexport const answer = 42;\nexport function top(): void {}\nexport type Alias = Cfg | null;\n",
    );
    let (v, _) = run(&["index", "--path", f.to_str().unwrap()], b"");
    let v = ok(&v);
    assert_eq!(v["language"], "typescript");
    let sk = v["skeleton"].as_str().unwrap();
    assert!(sk.contains("imports (2): [1-2]"), "{sk}");
    assert!(sk.contains("interface Cfg"), "{sk}");
    assert!(sk.contains("run(): void"), "{sk}");
    assert!(sk.contains("class Impl implements Cfg"), "{sk}");
    assert!(sk.contains("helper(): number"), "{sk}");
    assert!(sk.contains("answer"), "{sk}");
    assert!(sk.contains("type Alias"), "{sk}");
    assert!(!sk.contains("console.log"), "{sk}");
}

#[test]
fn index_python_and_go_files() {
    let dir = tmpdir("pygo");
    let py = dir.join("c.py");
    write(
        &py,
        "import os\nfrom sys import path\n\nCONST = 1\n\nclass K:\n    attr = 2\n    def m(self, x: int) -> int:\n        return x\n\ndef f(a: int, b: str) -> None:\n    pass\n",
    );
    let (v, _) = run(&["index", "--path", py.to_str().unwrap()], b"");
    let v = ok(&v);
    assert_eq!(v["language"], "python");
    let sk = v["skeleton"].as_str().unwrap();
    assert!(sk.contains("imports (2): [1-2]"), "{sk}");
    assert!(sk.contains("class K:"), "{sk}");
    assert!(sk.contains("def m(self, x: int) -> int:"), "{sk}");
    assert!(sk.contains("def f(a: int, b: str) -> None:"), "{sk}");
    assert!(!sk.contains("return x"), "{sk}");

    let go = dir.join("d.go");
    write(
        &go,
        "package main\n\nimport \"fmt\"\n\ntype Server struct {\n\taddr string\n}\n\nfunc (s *Server) Start() error { return nil }\n\nfunc main() { fmt.Println(\"x\") }\n",
    );
    let (v, _) = run(&["index", "--path", go.to_str().unwrap()], b"");
    let v = ok(&v);
    assert_eq!(v["language"], "go");
    let sk = v["skeleton"].as_str().unwrap();
    assert!(sk.contains("package main"), "{sk}");
    assert!(sk.contains("imports: [3-3]"), "{sk}");
    assert!(sk.contains("type Server struct"), "{sk}");
    assert!(sk.contains("func (s *Server) Start() error"), "{sk}");
    assert!(!sk.contains("Println(\"x\")"), "{sk}");
}

#[test]
fn index_dir_mode_and_gitignore() {
    let dir = tmpdir("dir");
    write(&dir.join("src/a.rs"), RUST_SRC);
    write(&dir.join("src/nested/b.ts"), "export function x() {}\n");
    write(&dir.join("README.md"), "# Title\n\nbody\n");
    write(&dir.join(".gitignore"), "ignored/\n");
    write(&dir.join("ignored/skip.rs"), "fn hidden() {}\n");
    write(&dir.join("node_modules/dep.js"), "function dep() {}\n");
    let (v, _) = run(&["index", "--path", dir.to_str().unwrap()], b"");
    let v = ok(&v);
    assert_eq!(v["kind"], "dir");
    let sk = v["skeleton"].as_str().unwrap();
    assert!(sk.contains("a.rs"), "{sk}");
    assert!(sk.contains("nested/"), "{sk}");
    assert!(sk.contains("README.md"), "{sk}");
    assert!(!sk.contains("skip.rs"), "{sk}");
    assert!(!sk.contains("dep.js"), "{sk}");
    assert!(v["fileCount"].as_u64().unwrap() >= 3);
    assert!(v["symbolCount"].as_u64().unwrap() > 0);
}

#[test]
fn index_refuses_over_2mib() {
    let dir = tmpdir("big");
    let f = dir.join("big.rs");
    std::fs::write(&f, vec![b'x'; 2 * 1024 * 1024 + 1]).unwrap();
    let (v, code) = run(&["index", "--path", f.to_str().unwrap()], b"");
    assert_eq!(code, 1);
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().contains("2 MiB"));
}

#[test]
fn index_markdown_fallback() {
    let dir = tmpdir("md");
    let f = dir.join("doc.md");
    write(&f, "# Top\n\ntext\n\n## Sub A\n\nbody\n\n## Sub B\n");
    let (v, code) = run(&["index", "--path", f.to_str().unwrap()], b"");
    assert_eq!(code, 0);
    let v = ok(&v);
    assert_eq!(v["language"], "markdown");
    let sk = v["skeleton"].as_str().unwrap();
    assert!(sk.contains("Top [1-1]"), "{sk}");
    assert!(sk.contains("Sub A"), "{sk}");
}

#[test]
fn index_max_lines_truncation() {
    let dir = tmpdir("trunc");
    let f = dir.join("many.py");
    let src: String = (0..100)
        .map(|i| format!("def f{i}():\n    return {i}\n\n"))
        .collect();
    write(&f, &src);
    let (v, _) = run(
        &["index", "--path", f.to_str().unwrap(), "--max-lines", "10"],
        b"",
    );
    let v = ok(&v);
    assert_eq!(v["truncated"], true);
    assert!(v["skeleton"]
        .as_str()
        .unwrap()
        .contains("more lines truncated"));
}

// ---------------- slice ----------------

#[test]
fn slice_by_symbol_bare_and_qualified() {
    let dir = tmpdir("slice_sym");
    let f = dir.join("s.rs");
    write(&f, RUST_SRC);
    let (v, _) = run(
        &["slice", "--path", f.to_str().unwrap(), "--symbol", "helper"],
        b"",
    );
    let v = ok(&v);
    assert_eq!(v["symbol"], "helper");
    let c = v["content"].as_str().unwrap();
    assert!(c.contains("fn helper() -> u8 { 7 }"), "{c}");

    // Type::method qualified lookup.
    let (v, _) = run(
        &[
            "slice",
            "--path",
            f.to_str().unwrap(),
            "--symbol",
            "Foo::new",
        ],
        b"",
    );
    let v = ok(&v);
    assert_eq!(v["symbol"], "Foo::new");
    let c = v["content"].as_str().unwrap();
    assert!(c.contains("pub fn new(x: u8) -> Self"), "{c}");
}

#[test]
fn slice_by_line_range() {
    let dir = tmpdir("slice_range");
    let f = dir.join("r.py");
    write(&f, "a = 1\nb = 2\nc = 3\nd = 4\n");
    let (v, _) = run(
        &[
            "slice",
            "--path",
            f.to_str().unwrap(),
            "--start",
            "2",
            "--end",
            "3",
        ],
        b"",
    );
    let v = ok(&v);
    assert_eq!(v["startLine"], 2);
    assert_eq!(v["endLine"], 3);
    assert_eq!(v["content"], "L2: b = 2\nL3: c = 3");
}

#[test]
fn slice_python_class_method_dot_syntax() {
    let dir = tmpdir("slice_py");
    let f = dir.join("m.py");
    write(
        &f,
        "class Worker:\n    def run(self):\n        return 1\n    def stop(self):\n        return 2\n",
    );
    let (v, _) = run(
        &[
            "slice",
            "--path",
            f.to_str().unwrap(),
            "--symbol",
            "Worker.stop",
        ],
        b"",
    );
    let v = ok(&v);
    assert_eq!(v["symbol"], "Worker::stop");
    assert!(v["content"].as_str().unwrap().contains("def stop"));
}

#[test]
fn slice_go_receiver_qualified_method() {
    let dir = tmpdir("slice_go");
    let f = dir.join("g.go");
    write(
        &f,
        "package main\n\ntype S struct{ n int }\n\nfunc (s *S) Get() int { return s.n }\n",
    );
    let (v, _) = run(
        &["slice", "--path", f.to_str().unwrap(), "--symbol", "S::Get"],
        b"",
    );
    let v = ok(&v);
    assert_eq!(v["symbol"], "S::Get");
    assert!(v["content"]
        .as_str()
        .unwrap()
        .contains("func (s *S) Get() int"));
}

#[test]
fn index_and_slice_cpp_template_struct_inside_header_guard() {
    let dir = tmpdir("cpp_guard");
    let f = dir.join("policy.hpp");
    write(
        &f,
        "#ifndef POLICY_GUARD\n#define POLICY_GUARD\n\ntemplate <typename T>\nstruct Policy {\n    void apply();\n};\n\n#endif\n",
    );
    let (v, code) = run(&["index", "--path", f.to_str().unwrap()], b"");
    assert_eq!(code, 0);
    let v = ok(&v);
    let sk = v["skeleton"].as_str().unwrap();
    assert!(sk.contains("struct Policy"), "{sk}");
    assert!(sk.contains("apply"), "{sk}");

    let (v, code) = run(
        &[
            "slice",
            "--path",
            f.to_str().unwrap(),
            "--symbol",
            "Policy::apply",
        ],
        b"",
    );
    assert_eq!(code, 0, "{v}");
    let v = ok(&v);
    assert_eq!(v["symbol"], "Policy::apply");
    assert!(v["content"].as_str().unwrap().contains("void apply();"));
}

// ---------------- grep ----------------

#[test]
fn grep_ast_annotation_rust_and_ts() {
    let dir = tmpdir("grep_ast");
    write(
        &dir.join("a.rs"),
        "fn outer() {\n    let needle = 1;\n    println!(\"{}\", needle);\n}\n",
    );
    write(
        &dir.join("sub/b.ts"),
        "export class Svc {\n  run() { return needleFunc(); }\n}\n",
    );
    let (v, code) = run(
        &[
            "grep",
            "--pattern",
            "needle",
            "--path",
            dir.to_str().unwrap(),
        ],
        b"",
    );
    assert_eq!(code, 0);
    let v = ok(&v);
    assert_eq!(v["filesMatched"], 2);
    assert_eq!(v["totalHits"], 3);
    assert_eq!(v["shownHits"], 3);
    assert_eq!(v["truncated"], false);
    let out = v["output"].as_str().unwrap();
    assert!(out.contains("a.rs\n"), "{out}");
    assert!(out.contains("L2 [outer]: let needle = 1;"), "{out}");
    assert!(out.contains("sub/b.ts"), "{out}");
    assert!(
        out.contains("L2 [Svc::run]: run() { return needleFunc(); }"),
        "{out}"
    );
}

#[test]
fn grep_glob_filter() {
    let dir = tmpdir("grep_glob");
    write(&dir.join("a.rs"), "fn f() { needle(); }\n");
    write(&dir.join("b.ts"), "function g() { needle(); }\n");
    let (v, _) = run(
        &[
            "grep",
            "--pattern",
            "needle",
            "--path",
            dir.to_str().unwrap(),
            "--glob",
            "*.rs",
        ],
        b"",
    );
    let v = ok(&v);
    assert_eq!(v["filesMatched"], 1);
    let out = v["output"].as_str().unwrap();
    assert!(out.contains("a.rs"), "{out}");
    assert!(!out.contains("b.ts"), "{out}");
}

#[test]
fn grep_lockfile_skipped_in_dir_but_targeted_directly() {
    let dir = tmpdir("grep_lock");
    write(&dir.join("Cargo.lock"), "version = 3\nneedle = true\n");
    write(&dir.join("a.rs"), "fn f() {}\n");
    let (v, _) = run(
        &[
            "grep",
            "--pattern",
            "needle",
            "--path",
            dir.to_str().unwrap(),
        ],
        b"",
    );
    let v = ok(&v);
    assert_eq!(v["filesMatched"], 0, "{}", v["output"]);
    assert_eq!(v["totalHits"], 0);

    let lock = dir.join("Cargo.lock");
    let (v, _) = run(
        &[
            "grep",
            "--pattern",
            "needle",
            "--path",
            lock.to_str().unwrap(),
        ],
        b"",
    );
    let v = ok(&v);
    assert_eq!(v["filesMatched"], 1);
    assert!(v["output"].as_str().unwrap().contains("needle = true"));
}

#[test]
fn grep_max_hits_and_byte_truncation() {
    let dir = tmpdir("grep_hits");
    let mut src = String::new();
    for i in 0..20 {
        src.push_str(&format!("fn f{i}() {{ needle{i}(); }}\n"));
    }
    write(&dir.join("many.rs"), &src);
    let (v, _) = run(
        &[
            "grep",
            "--pattern",
            "needle",
            "--path",
            dir.to_str().unwrap(),
            "--max-hits",
            "3",
        ],
        b"",
    );
    let v = ok(&v);
    assert_eq!(v["totalHits"], 20);
    assert_eq!(v["shownHits"], 3);
    assert_eq!(v["truncated"], true);
    assert_eq!(v["output"].as_str().unwrap().matches("needle").count(), 3);

    let (v, _) = run(
        &[
            "grep",
            "--pattern",
            "needle",
            "--path",
            dir.to_str().unwrap(),
            "--max-bytes",
            "120",
        ],
        b"",
    );
    let v = ok(&v);
    assert_eq!(v["truncated"], true);
    let out = v["output"].as_str().unwrap();
    assert!(out.len() <= 120, "{}", out.len());
    assert!(out.contains("bytes truncated"), "{out}");
}

#[test]
fn grep_invalid_regex_error_json() {
    let dir = tmpdir("grep_badre");
    write(&dir.join("a.rs"), "fn f() {}\n");
    let (v, code) = run(
        &["grep", "--pattern", "(", "--path", dir.to_str().unwrap()],
        b"",
    );
    assert_eq!(code, 1);
    assert_eq!(v["ok"], false);
    assert!(v["error"]
        .as_str()
        .unwrap()
        .contains("invalid --pattern regex"));
}

#[test]
fn grep_accepts_dash_prefixed_pattern_value() {
    let dir = tmpdir("grep_dash_pattern");
    write(&dir.join("a.rs"), "fn f() {} // --foo\n");
    let (v, code) = run(
        &[
            "grep",
            "--pattern",
            "--foo",
            "--path",
            dir.to_str().unwrap(),
        ],
        b"",
    );
    assert_eq!(code, 0, "{v}");
    let v = ok(&v);
    assert_eq!(v["totalHits"], 1);
    assert!(v["output"].as_str().unwrap().contains("--foo"));
}

#[test]
fn grep_skips_noise_dirs() {
    let dir = tmpdir("grep_skip");
    write(&dir.join("src/a.rs"), "fn f() { needle(); }\n");
    write(&dir.join("node_modules/dep.js"), "needle();\n");
    write(&dir.join("target/out.rs"), "needle();\n");
    write(&dir.join("x.min.js"), "needle();\n");
    let (v, _) = run(
        &[
            "grep",
            "--pattern",
            "needle",
            "--path",
            dir.to_str().unwrap(),
        ],
        b"",
    );
    let v = ok(&v);
    assert_eq!(v["filesMatched"], 1);
    let out = v["output"].as_str().unwrap();
    assert!(out.contains("src/a.rs"), "{out}");
    assert!(!out.contains("dep.js"), "{out}");
    assert!(!out.contains("out.rs"), "{out}");
    assert!(!out.contains("x.min.js"), "{out}");
}

#[test]
fn grep_deeply_nested_source_is_bounded() {
    let dir = tmpdir("grep_deep_nesting");
    let depth = 1200;
    let src = format!(
        "const x = {}needle{};\n",
        "(".repeat(depth),
        ")".repeat(depth)
    );
    write(&dir.join("deep.ts"), &src);
    let (v, code) = run(
        &[
            "grep",
            "--pattern",
            "needle",
            "--path",
            dir.to_str().unwrap(),
        ],
        b"",
    );
    assert_eq!(code, 0);
    let v = ok(&v);
    assert_eq!(v["totalHits"], 1);
}

// ---------------- diff ----------------

fn git(args: &[&str], dir: &Path) {
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}

fn git_repo(name: &str) -> PathBuf {
    let dir = tmpdir(name);
    git(&["init", "-q"], &dir);
    git(&["config", "user.email", "t@t"], &dir);
    git(&["config", "user.name", "t"], &dir);
    dir
}

fn git_commit(dir: &Path, msg: &str) {
    git(&["add", "-A"], dir);
    git(&["commit", "-qm", msg], dir);
}

#[test]
fn diff_working_tree() {
    let dir = git_repo("diff_wt");
    write(&dir.join("a.rs"), "fn main() {\n    println!(\"v1\");\n}\n");
    git_commit(&dir, "init");
    write(
        &dir.join("a.rs"),
        "fn main() {\n    println!(\"v2\");\n    println!(\"x\");\n}\n",
    );
    let (v, code) = run(&["diff", "--cwd", dir.to_str().unwrap()], b"");
    assert_eq!(code, 0);
    let v = ok(&v);
    assert_eq!(v["filesChanged"], 1);
    assert_eq!(v["insertions"], 2);
    assert_eq!(v["deletions"], 1);
    assert!(v["rev"].is_null());
    let d = v["diff"].as_str().unwrap();
    assert!(d.contains("1 file changed"), "{d}");
    assert!(d.contains("M a.rs"), "{d}");
    assert!(d.contains("+    println!(\"v2\");"), "{d}");
    // -U2 context, not the default 3: a 4-line file shows all lines anyway,
    // so assert the hunk header shape instead.
    assert!(d.contains("@@ "), "{d}");
}

#[test]
fn diff_rev_range() {
    let dir = git_repo("diff_rev");
    write(&dir.join("a.rs"), "fn main() {}\n");
    git_commit(&dir, "v1");
    write(&dir.join("a.rs"), "fn main() {\n    go();\n}\n");
    git_commit(&dir, "v2");
    let (v, _) = run(
        &[
            "diff",
            "--cwd",
            dir.to_str().unwrap(),
            "--rev",
            "HEAD~1..HEAD",
        ],
        b"",
    );
    let v = ok(&v);
    assert_eq!(v["rev"], "HEAD~1..HEAD");
    assert_eq!(v["filesChanged"], 1);
    assert!(v["diff"].as_str().unwrap().contains("+    go();"));
    // Bad rev surfaces as ok:false.
    let (v, code) = run(
        &["diff", "--cwd", dir.to_str().unwrap(), "--rev", "bogus-rev"],
        b"",
    );
    assert_eq!(code, 1);
    assert_eq!(v["ok"], false);
}

#[test]
fn diff_lockfile_excluded_and_counted() {
    let dir = git_repo("diff_lock");
    write(&dir.join("a.rs"), "fn main() {}\n");
    write(&dir.join("Cargo.lock"), "lockv1\n");
    git_commit(&dir, "init");
    write(&dir.join("a.rs"), "fn main() { go(); }\n");
    write(&dir.join("Cargo.lock"), "lockv2\n");
    let (v, _) = run(&["diff", "--cwd", dir.to_str().unwrap()], b"");
    let v = ok(&v);
    assert_eq!(v["filesChanged"], 1);
    assert_eq!(v["skippedGenerated"], 1);
    let d = v["diff"].as_str().unwrap();
    assert!(!d.contains("lockv2"), "{d}");
    assert!(d.contains("generated/lock files skipped"), "{d}");

    // --path explicitly targeting the lockfile includes it.
    let (v, _) = run(
        &[
            "diff",
            "--cwd",
            dir.to_str().unwrap(),
            "--path",
            "Cargo.lock",
        ],
        b"",
    );
    let v = ok(&v);
    assert_eq!(v["skippedGenerated"], 0);
    assert!(v["diff"].as_str().unwrap().contains("lockv2"));
}

#[test]
fn diff_rejects_option_like_rev_without_invoking_git() {
    let dir = git_repo("diff_rev_injection");
    write(&dir.join("a.rs"), "fn main() {}\n");
    git_commit(&dir, "init");
    let planted = dir.join("evil.patch");
    let rev = format!("--output={}", planted.display());
    let (v, code) = run(
        &[
            "diff",
            "--cwd",
            dir.to_str().unwrap(),
            &format!("--rev={rev}"),
        ],
        b"",
    );
    assert_eq!(code, 1);
    assert_eq!(v["ok"], false);
    assert!(
        v["error"].as_str().unwrap().contains("invalid --rev"),
        "{v}"
    );
    assert!(!planted.exists(), "git --output must not be reached");
}

#[test]
fn diff_rejects_option_like_rev_in_separate_argv_slot() {
    let dir = git_repo("diff_rev_separate_injection");
    write(&dir.join("a.rs"), "fn main() {}\n");
    git_commit(&dir, "init");
    let planted = dir.join("evil-separate.patch");
    let rev = format!("--output={}", planted.display());
    let (v, code) = run(
        &["diff", "--cwd", dir.to_str().unwrap(), "--rev", &rev],
        b"",
    );
    assert_eq!(code, 1);
    assert_eq!(v["ok"], false);
    assert!(
        v["error"].as_str().unwrap().contains("invalid --rev"),
        "{v}"
    );
    assert!(!planted.exists(), "git --output must not be reached");
}

#[test]
fn cli_rejects_unknown_flags_positionals_and_invalid_numbers() {
    let dir = tmpdir("cli_strict_args");
    let f = dir.join("a.rs");
    write(&f, "fn main() {}\n");

    let (v, code) = run(&["index", "--paths", f.to_str().unwrap()], b"");
    assert_eq!(code, 1);
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().contains("unknown flag"), "{v}");

    let (v, code) = run(&["index", "--path", f.to_str().unwrap(), "extra"], b"");
    assert_eq!(code, 1);
    assert_eq!(v["ok"], false);
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("unexpected positional"),
        "{v}"
    );

    let (v, code) = run(
        &[
            "index",
            "--path",
            f.to_str().unwrap(),
            "--max-lines",
            "nope",
        ],
        b"",
    );
    assert_eq!(code, 1);
    assert_eq!(v["ok"], false);
    assert!(
        v["error"].as_str().unwrap().contains("invalid integer"),
        "{v}"
    );
}

#[test]
fn diff_rejects_pathspec_escape_and_parent_traversal() {
    let dir = git_repo("diff_path_escape");
    write(&dir.join("outside.rs"), "fn outside() { secret(); }\n");
    write(&dir.join("sub/inside.rs"), "fn inside() {}\n");
    git_commit(&dir, "init");
    write(&dir.join("outside.rs"), "fn outside() { leaked(); }\n");
    write(&dir.join("sub/inside.rs"), "fn inside() { changed(); }\n");
    let sub = dir.join("sub");

    for bad in [":(top)", "../outside.rs"] {
        let (v, code) = run(
            &["diff", "--cwd", sub.to_str().unwrap(), "--path", bad],
            b"",
        );
        assert_eq!(code, 1, "{bad}: {v}");
        assert_eq!(v["ok"], false);
        assert!(v.get("diff").is_none(), "{bad} leaked a diff: {v}");
    }
}

#[test]
fn diff_rename_header_keeps_old_and_new_paths() {
    let dir = git_repo("diff_rename");
    write(&dir.join("old.txt"), "same contents\n");
    git_commit(&dir, "init");
    git(&["mv", "old.txt", "new.txt"], &dir);
    git_commit(&dir, "rename");
    let (v, code) = run(
        &[
            "diff",
            "--cwd",
            dir.to_str().unwrap(),
            "--rev",
            "HEAD~1..HEAD",
        ],
        b"",
    );
    assert_eq!(code, 0, "{v}");
    let v = ok(&v);
    let d = v["diff"].as_str().unwrap();
    assert!(d.contains("R100 old.txt -> new.txt"), "{d}");
}

#[test]
fn diff_max_bytes_truncation() {
    let dir = git_repo("diff_trunc");
    let mut src = String::new();
    for i in 0..60 {
        src.push_str(&format!("fn f{i}() {{}}\n"));
    }
    write(&dir.join("big.rs"), &src);
    git_commit(&dir, "init");
    let mut src = String::new();
    for i in 0..60 {
        src.push_str(&format!("fn f{i}() {{ touch{i}(); }}\n"));
    }
    write(&dir.join("big.rs"), &src);
    let (v, _) = run(
        &["diff", "--cwd", dir.to_str().unwrap(), "--max-bytes", "800"],
        b"",
    );
    let v = ok(&v);
    assert_eq!(v["truncated"], true);
    let d = v["diff"].as_str().unwrap();
    assert!(d.len() <= 800, "{}", d.len());
    assert!(d.contains("bytes truncated"), "{d}");
}

// ---------------- exec ----------------

#[test]
fn exec_chains_helpers_and_truncates() {
    let dir = tmpdir("exec with space");
    let f = dir.join("target.rs");
    write(&f, RUST_SRC);
    let script = format!(
        "sk = index({})\nprint('SYMS', len(sk.splitlines()))\nbody = slice_code({}, symbol='helper')\nprint('BODY', body.strip())\nhits = grep(r'fn ', {})\nprint('HITS', len(hits.splitlines()))\nfiles = glob_files('*.rs', {})\nprint('FILES', files)\n",
        py_string(&f.to_string_lossy()),
        py_string(&f.to_string_lossy()),
        py_string(&dir.to_string_lossy()),
        py_string(&dir.to_string_lossy())
    );
    let (v, _) = run(&["exec", "--cwd", dir.to_str().unwrap()], script.as_bytes());
    let v = ok(&v);
    assert_eq!(v["exitCode"], 0, "{}", v["stderr"]);
    let out = v["stdout"].as_str().unwrap();
    assert!(out.contains("SYMS"), "{out}");
    assert!(out.contains("BODY L"), "{out}");
    assert!(out.contains("HITS"), "{out}");
    assert!(out.contains("target.rs"), "{out}");
}

#[test]
fn exec_timeout_kills_child() {
    let dir = tmpdir("exec_to");
    let t0 = std::time::Instant::now();
    let (v, _) = run(
        &[
            "exec",
            "--cwd",
            dir.to_str().unwrap(),
            "--timeout-ms",
            "800",
        ],
        b"import time\ntime.sleep(30)\n",
    );
    let v = ok(&v);
    assert_eq!(v["timedOut"], true);
    assert_eq!(v["exitCode"], -9);
    assert!(t0.elapsed().as_millis() < 10_000, "took {:?}", t0.elapsed());
}

#[test]
fn exec_reaps_descendants_after_the_script_exits() {
    let dir = tmpdir("exec_descendant");
    let started = std::time::Instant::now();
    let (result, _) = run(
        &["exec", "--cwd", dir.to_str().unwrap()],
        b"import subprocess, sys
subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])
print('done')
",
    );
    let result = ok(&result);
    assert_eq!(result["exitCode"], 0);
    assert_eq!(result["timedOut"], false);
    assert!(started.elapsed().as_secs() < 5);
}

#[cfg(unix)]
#[test]
fn exec_setsid_grandchild_pipe_does_not_hang() {
    let dir = tmpdir("exec_setsid_pipe");
    let started = std::time::Instant::now();
    let (v, _) = run(
        &["exec", "--cwd", dir.to_str().unwrap()],
        b"import os, subprocess, sys
subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'], preexec_fn=os.setsid)
print('done')
",
    );
    assert!(
        started.elapsed().as_secs() < 5,
        "took {:?}",
        started.elapsed()
    );
    let v = ok(&v);
    assert_eq!(v["exitCode"], 0, "{v}");
    assert_eq!(v["timedOut"], false);
}

#[test]
fn exec_head_tail_truncation_keeps_end() {
    let dir = tmpdir("exec_trunc");
    let script = b"print('H' * 4000)\nprint('TAIL_MARKER' + 'T' * 4000)\n";
    let (v, _) = run(
        &[
            "exec",
            "--cwd",
            dir.to_str().unwrap(),
            "--max-bytes",
            "2000",
        ],
        script,
    );
    let v = ok(&v);
    assert_eq!(v["truncated"], true);
    let out = v["stdout"].as_str().unwrap();
    assert!(out.len() <= 2000, "{}", out.len());
    assert!(out.starts_with('H'), "{out}");
    assert!(out.contains("bytes truncated"), "{out}");
    assert!(out.trim_end().ends_with('T'), "{out}");
}

#[test]
fn exec_does_not_inherit_secret_environment() {
    let dir = tmpdir("exec_env");
    let marker = "t3mt-test-marker-value";
    let script = b"import os\nprint('PRESENT', 'T3MT_SECRET_TEST' in os.environ)\nprint('VALUE', os.environ.get('T3MT_SECRET_TEST'))\n";
    let (v, _) = run_with_env(
        &["exec", "--cwd", dir.to_str().unwrap()],
        script,
        &[("T3MT_SECRET_TEST", marker)],
    );
    let v = ok(&v);
    assert_eq!(v["exitCode"], 0, "{}", v["stderr"]);
    let out = v["stdout"].as_str().unwrap();
    assert!(out.contains("PRESENT False"), "{out}");
    assert!(!out.contains(marker), "{out}");
}

#[test]
fn exec_read_rejects_workspace_symlink_escape() {
    let dir = tmpdir("exec_symlink");
    let outside = tmpdir("exec_symlink_outside").join("outside.txt");
    write(&outside, "outside-secret\n");
    let link = dir.join("inside-link.txt");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    #[cfg(windows)]
    if std::os::windows::fs::symlink_file(&outside, &link).is_err() {
        // Windows test hosts commonly require a developer-mode privilege for
        // symlinks. The production path check itself is platform-independent.
        return;
    }

    let script = b"try:\n    read('inside-link.txt')\n    print('ALLOWED')\nexcept RuntimeError as e:\n    print('BLOCKED', 'outside workspace' in str(e))\n";
    let (v, _) = run(&["exec", "--cwd", dir.to_str().unwrap()], script);
    let v = ok(&v);
    assert_eq!(v["exitCode"], 0, "{}", v["stderr"]);
    assert!(
        v["stdout"].as_str().unwrap().contains("BLOCKED True"),
        "{}",
        v["stdout"]
    );
}

#[test]
fn exec_huge_output_still_preserves_tail() {
    let dir = tmpdir("exec_huge_tail");
    let script =
        b"import sys\nsys.stdout.write('H' * (9 * 1024 * 1024))\nsys.stdout.write('TAIL_MARKER')\n";
    let (v, _) = run(
        &[
            "exec",
            "--cwd",
            dir.to_str().unwrap(),
            "--max-bytes",
            "2000",
        ],
        script,
    );
    let v = ok(&v);
    assert_eq!(v["exitCode"], 0, "{}", v["stderr"]);
    assert_eq!(v["truncated"], true);
    let out = v["stdout"].as_str().unwrap();
    assert!(out.len() <= 2000, "{}", out.len());
    assert!(
        out.ends_with("TAIL_MARKER"),
        "tail missing: {}",
        &out[out.len().saturating_sub(100)..]
    );
}

// ---------------- econ ----------------

#[test]
fn econ_matches_reference_break_even() {
    // reference cost-model unit test: one_time 69_000, saving 12_600.
    let input = br#"{"prefix":10000,"replaced":130000,"summary":4000,"tail":26000,"instruction":500,"summarizer_reuses_prefix":true,"prices":{"input":1.0,"output":5.0,"cache_read":0.1,"cache_write":1.25},"remaining_turns":10}"#;
    let (v, _) = run(&["econ"], input);
    let v = ok(&v);
    assert!((v["oneTimeCost"].as_f64().unwrap() - 69_000.0).abs() < 1e-6);
    assert!((v["savingPerTurn"].as_f64().unwrap() - 12_600.0).abs() < 1e-6);
    assert!((v["breakEvenTurns"].as_f64().unwrap() - 69_000.0 / 12_600.0).abs() < 1e-6);
    assert_eq!(v["shouldCompact"], true); // 5.48 <= 10

    // Never pays off: replaced == summary.
    let input2 = br#"{"prefix":0,"replaced":1000,"summary":1000,"tail":0,"instruction":0,"summarizer_reuses_prefix":true,"prices":{"input":1.0,"output":5.0,"cache_read":0.1,"cache_write":1.25}}"#;
    let (v, _) = run(&["econ"], input2);
    let v = ok(&v);
    assert!(v["breakEvenTurns"].is_null());
    assert_eq!(v["shouldCompact"], false);

    // No prefix reuse: summarizer reads at input price.
    let input3 = br#"{"prefix":100,"replaced":1000,"summary":100,"tail":50,"instruction":10,"summarizer_reuses_prefix":false,"prices":{"input":1.0,"output":5.0,"cache_read":0.1,"cache_write":0.0},"remaining_turns":3}"#;
    let (v, _) = run(&["econ"], input3);
    let v = ok(&v);
    // call: (100+1000)*1.0 + 10*1.0 + 100*5.0 = 1610; rewrite: 150*(1.0-0.1)=135
    assert!((v["oneTimeCost"].as_f64().unwrap() - 1745.0).abs() < 1e-6);
    assert!((v["savingPerTurn"].as_f64().unwrap() - 90.0).abs() < 1e-6);
    // 19.39 turns > 3 remaining => skip
    assert_eq!(v["shouldCompact"], false);
}

#[test]
fn econ_rejects_negative_and_nonfinite_numeric_inputs() {
    let negative = br#"{"prefix":0,"replaced":1000,"summary":100,"tail":0,"instruction":0,"summarizer_reuses_prefix":true,"prices":{"input":-1.0,"output":5.0,"cache_read":0.1,"cache_write":1.25}}"#;
    let (v, code) = run(&["econ"], negative);
    assert_eq!(code, 1);
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().contains("prices.input"), "{v}");

    let negative_turns = br#"{"prefix":0,"replaced":1000,"summary":100,"tail":0,"instruction":0,"summarizer_reuses_prefix":true,"prices":{"input":1.0,"output":5.0,"cache_read":0.1,"cache_write":1.25},"remaining_turns":-1}"#;
    let (v, code) = run(&["econ"], negative_turns);
    assert_eq!(code, 1);
    assert_eq!(v["ok"], false);
    assert!(
        v["error"].as_str().unwrap().contains("remaining_turns"),
        "{v}"
    );
}

// ---------------- stdin defensive cap ----------------

#[test]
fn stdin_over_defensive_cap_is_json_error() {
    let input = vec![b'x'; 16 * 1024 * 1024 + 1];
    let (v, code) = run(&["econ"], &input);
    assert_eq!(code, 1);
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().unwrap().contains("input limit"), "{v}");
}

// ---------------- misc ----------------

#[test]
fn version_and_help() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("t3-maki-tools"));
    let out = Command::new(BIN).arg("--help").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("index --path"));
}

#[test]
fn error_shape_exit_1() {
    let (v, code) = run(&["index", "--path", "/nonexistent/xyz.rs"], b"");
    assert_eq!(code, 1);
    assert_eq!(v["ok"], false);
    assert!(v["error"].is_string());
}
