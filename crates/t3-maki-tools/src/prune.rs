//! `prune`: head+tail char-budget pruner, reference implementation style.
//! `native_harness::compaction::prune` (which keeps the JSON envelope intact
//! and only shrinks payload text fields).

use serde_json::{json, Value};

use crate::text::head_tail_chars;
use crate::emit;

/// Total chars of every string leaf in the JSON value.
fn string_chars(v: &Value) -> usize {
    match v {
        Value::String(s) => s.chars().count(),
        Value::Array(a) => a.iter().map(string_chars).sum(),
        Value::Object(o) => o.values().map(string_chars).sum(),
        _ => 0,
    }
}

/// Prune each string field with a budget proportional to its size, keeping
/// the JSON structure intact. Returns the new total string chars.
fn prune_json_strings(v: &mut Value, budget: usize, total_string_chars: usize) {
    match v {
        Value::String(s) => {
            let share = s.chars().count() * budget / total_string_chars.max(1);
            if s.chars().count() > share.max(64) {
                let (p, _, _) = head_tail_chars(s, share.max(64));
                *s = p;
            }
        }
        Value::Array(a) => {
            for x in a.iter_mut() {
                prune_json_strings(x, budget, total_string_chars);
            }
        }
        Value::Object(o) => {
            for x in o.values_mut() {
                prune_json_strings(x, budget, total_string_chars);
            }
        }
        _ => {}
    }
}

pub fn prune_text(text: &str, max_chars: usize) -> (String, bool) {
    let original = text.chars().count();
    if original <= max_chars {
        return (text.to_string(), false);
    }
    // JSON-aware path: keep the structure, shrink string payloads only.
    // Escaped marker newlines make the serialized form longer than the raw
    // char count, so converge by shrinking the budget on each retry.
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        let overhead = text.len().saturating_sub(string_chars(&v));
        if overhead < max_chars {
            let total = string_chars(&v);
            let mut budget = max_chars - overhead;
            for _ in 0..5 {
                if total == 0 || budget == 0 {
                    break;
                }
                let mut v2 = v.clone();
                prune_json_strings(&mut v2, budget, total);
                let out = serde_json::to_string(&v2).unwrap_or_default();
                let c = out.chars().count();
                if c <= max_chars {
                    return (out, true);
                }
                budget = budget.saturating_sub(c - max_chars);
            }
        }
    }
    let (out, _, pruned) = head_tail_chars(text, max_chars);
    (out, pruned)
}

pub fn cmd_prune(max_chars: usize, stdin_text: &str) -> ! {
    let original_chars = stdin_text.chars().count();
    let (text, pruned) = prune_text(stdin_text, max_chars);
    emit(&json!({
        "ok": true,
        "text": text,
        "pruned": pruned,
        "originalChars": original_chars,
        "finalChars": text.chars().count(),
    }));
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_text_passes_through() {
        let (out, pruned) = prune_text("short text", 4000);
        assert_eq!(out, "short text");
        assert!(!pruned);
    }

    #[test]
    fn head_tail_marker_and_budget() {
        let s = "x".repeat(20000);
        let (out, pruned) = prune_text(&s, 100);
        assert!(pruned);
        assert!(out.contains("chars pruned"));
        assert!(out.chars().count() <= 100);
        assert!(out.starts_with('x'));
        assert!(out.ends_with('x'));
    }

    #[test]
    fn utf8_boundaries_are_respected() {
        for source in ["é".repeat(9000), "日本語abc".repeat(3000)] {
            for budget in [0, 1, 5, 40, 64, 100, 500] {
                let (out, _) = prune_text(&source, budget);
                assert!(out.chars().count() <= budget, "budget {budget}");
            }
        }
    }

    #[test]
    fn head_and_tail_order_preserved() {
        let s: String = (0..5000).map(|i| format!("L{i:04} ")).collect();
        let (out, pruned) = prune_text(&s, 120);
        assert!(pruned);
        let marker = out.find("chars pruned").expect("marker");
        assert!(out[..marker].contains("L0000"));
        assert!(out[marker..].contains("L4999"));
    }

    #[test]
    fn json_structure_is_preserved() {
        let big = "y".repeat(5000);
        let doc = json!({"ok": true, "stdout": big, "exitCode": 0}).to_string();
        let (out, pruned) = prune_text(&doc, 500);
        assert!(pruned);
        let parsed: Value = serde_json::from_str(&out).expect("still valid JSON");
        assert_eq!(parsed["exitCode"], 0);
        assert!(out.chars().count() <= 500);
    }
}
