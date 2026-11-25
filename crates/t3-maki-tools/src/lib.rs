pub mod diff;
pub mod dirindex;
pub mod econ;
pub mod exec_py;
pub mod grep;
pub mod process;
pub mod prune;
pub mod skeleton;
pub mod slice;
pub mod text;

pub use serde_json::{json, Value};

/// Print one JSON object to stdout.
pub fn emit(v: &Value) {
    println!(
        "{}",
        serde_json::to_string(v).unwrap_or_else(|_| "{}".into())
    );
}

/// Print `{ "ok": false, "error": ... }` and exit 1.
pub fn fail(msg: impl std::fmt::Display) -> ! {
    emit(&json!({ "ok": false, "error": msg.to_string() }));
    std::process::exit(1);
}
