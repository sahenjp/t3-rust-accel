//! `econ`: break-even cache economics for prompt-cache compaction decisions.

use serde::Deserialize;
use serde_json::json;

use crate::{emit, fail};

#[derive(Debug, Deserialize)]
struct Prices {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

#[derive(Debug, Deserialize)]
struct EconInput {
    prefix: u64,
    replaced: u64,
    summary: u64,
    tail: u64,
    instruction: u64,
    summarizer_reuses_prefix: bool,
    prices: Prices,
    #[serde(default)]
    remaining_turns: Option<f64>,
}

struct BreakEven {
    one_time_cost: f64,
    saving_per_turn: f64,
    turns: Option<f64>,
}

/// One compaction call's price = the summarizer call (read prefix+replaced at
/// the cached or fresh rate depending on `summarizer_reuses_prefix`, plus the
/// instruction at input price, plus the summary at output price) + the cache
/// rewrite of summary+tail at the fresh/cached spread.
/// `saving_per_turn` = (replaced - summary) x cache-read price.
fn break_even(input: &EconInput) -> BreakEven {
    let prices = &input.prices;
    let cached = prices.cache_read;
    let fresh = if prices.cache_write > 0.0 {
        prices.cache_write
    } else {
        prices.input
    };
    let read_side = if input.summarizer_reuses_prefix {
        cached
    } else {
        prices.input
    };
    let summary_call = (input.prefix as f64 + input.replaced as f64) * read_side
        + input.instruction as f64 * prices.input
        + input.summary as f64 * prices.output;
    let rewrite = (input.summary as f64 + input.tail as f64) * (fresh - cached).max(0.0);
    let one_time_cost = summary_call + rewrite;
    let saving_per_turn = (input.replaced as f64 - input.summary as f64) * cached;
    let turns = if saving_per_turn > 0.0 {
        Some(one_time_cost / saving_per_turn)
    } else {
        None
    };
    BreakEven {
        one_time_cost,
        saving_per_turn,
        turns,
    }
}

pub(crate) fn cmd_econ(stdin_json: &str) -> ! {
    let input: EconInput = match serde_json::from_str(stdin_json) {
        Ok(v) => v,
        Err(e) => fail(format!("invalid econ input JSON: {e}")),
    };
    for (name, value) in [
        ("prices.input", input.prices.input),
        ("prices.output", input.prices.output),
        ("prices.cache_read", input.prices.cache_read),
        ("prices.cache_write", input.prices.cache_write),
    ] {
        if !value.is_finite() || value < 0.0 {
            fail(format!("{name} must be finite and >= 0"));
        }
    }
    if let Some(remaining_turns) = input.remaining_turns {
        if !remaining_turns.is_finite() || remaining_turns < 0.0 {
            fail("remaining_turns must be finite and >= 0");
        }
    }
    let be = break_even(&input);
    let should_compact = match be.turns {
        Some(t) => t <= input.remaining_turns.unwrap_or(f64::INFINITY),
        None => false,
    };
    emit(&json!({
        "ok": true,
        "oneTimeCost": be.one_time_cost,
        "savingPerTurn": be.saving_per_turn,
        "breakEvenTurns": be.turns,
        "shouldCompact": should_compact,
    }));
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(l: f64, r: f64) -> bool {
        (l - r).abs() < 1e-6
    }

    #[test]
    fn break_even_with_a_shared_prefix() {
        // Mirrors the reference cost model break_even_with_a_shared_prefix.
        let input = EconInput {
            prefix: 10_000,
            replaced: 130_000,
            summary: 4_000,
            tail: 26_000,
            instruction: 500,
            summarizer_reuses_prefix: true,
            prices: Prices {
                input: 1.0,
                output: 5.0,
                cache_read: 0.1,
                cache_write: 1.25,
            },
            remaining_turns: None,
        };
        let r = break_even(&input);
        assert!(close(r.one_time_cost, 69_000.0));
        assert!(close(r.saving_per_turn, 12_600.0));
        assert!(close(r.turns.unwrap(), 69_000.0 / 12_600.0));
    }

    #[test]
    fn never_pays_off_when_the_summary_is_not_smaller() {
        let input = EconInput {
            prefix: 0,
            replaced: 1_000,
            summary: 1_000,
            tail: 0,
            instruction: 0,
            summarizer_reuses_prefix: true,
            prices: Prices {
                input: 1.0,
                output: 5.0,
                cache_read: 0.1,
                cache_write: 1.25,
            },
            remaining_turns: None,
        };
        assert_eq!(break_even(&input).turns, None);
    }
}
