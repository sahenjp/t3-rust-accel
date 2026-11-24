//! UTF-8 boundary helpers + head/tail truncation shared by slice/exec/prune.

/// Largest index <= `i` that is a UTF-8 char boundary.
pub fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest index >= `i` that is a UTF-8 char boundary.
pub fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Byte offset just past the first `n` chars.
pub fn head_byte_end(s: &str, n: usize) -> usize {
    let mut chars = 0usize;
    for (i, &b) in s.as_bytes().iter().enumerate() {
        if b & 0xC0 != 0x80 {
            chars += 1;
            if chars > n {
                return i;
            }
        }
    }
    s.len()
}

/// Byte offset where the last `n` chars start.
pub fn tail_byte_start(s: &str, n: usize) -> usize {
    let mut chars = 0usize;
    for (i, &b) in s.as_bytes().iter().enumerate().rev() {
        if b & 0xC0 != 0x80 {
            chars += 1;
            if chars > n {
                return i + utf8_len(b);
            }
        }
    }
    0
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b & 0xE0 == 0xC0 {
        2
    } else if b & 0xF0 == 0xE0 {
        3
    } else {
        4
    }
}

/// Head 40% / tail 60% byte truncation so errors and summaries at the end
/// survive. Returns (text, truncated).
pub fn head_tail_bytes(s: &str, max_bytes: usize) -> (String, bool) {
    if s.len() <= max_bytes {
        return (s.to_string(), false);
    }
    if max_bytes == 0 {
        return (String::new(), true);
    }
    // Marker length depends on the digit count of the removed byte total;
    // iterate twice to settle.
    let mut keep = max_bytes;
    let marker;
    loop {
        let removed = s.len().saturating_sub(keep);
        let m = format!("\n\n[... {removed} bytes truncated ...]\n\n");
        let new_keep = max_bytes.saturating_sub(m.len());
        if new_keep == keep || m.len() >= max_bytes {
            marker = m;
            keep = new_keep;
            break;
        }
        keep = new_keep;
    }
    if marker.len() >= max_bytes {
        // Tiny budget: head-only, no marker.
        return (s[..floor_boundary(s, max_bytes)].to_string(), true);
    }
    let head = keep * 2 / 5;
    let tail = keep - head;
    let head_end = floor_boundary(s, head);
    let tail_start = ceil_boundary(s, s.len() - tail.min(s.len()));
    let tail_start = tail_start.max(head_end);
    (
        format!("{}{}{}", &s[..head_end], marker, &s[tail_start..]),
        true,
    )
}

/// Head 40% / tail 60% char-budget prune with `[... N chars pruned ...]`
/// marker, in the spirit of the reference `tokens::prune_to_budget`.
/// Returns (text, removed_chars, pruned).
pub fn head_tail_chars(s: &str, max_chars: usize) -> (String, usize, bool) {
    let total = s.chars().count();
    if total <= max_chars {
        return (s.to_string(), 0, false);
    }
    if max_chars == 0 {
        return (String::new(), total, true);
    }
    let mut keep = max_chars;
    let marker;
    loop {
        let removed = total.saturating_sub(keep);
        let m = format!("\n\n[... {removed} chars pruned ...]\n\n");
        let mlen = m.chars().count();
        let new_keep = max_chars.saturating_sub(mlen);
        if new_keep == keep || mlen >= max_chars {
            marker = m;
            keep = new_keep;
            break;
        }
        keep = new_keep;
    }
    let mlen = marker.chars().count();
    if mlen >= max_chars {
        let cut = s[..head_byte_end(s, max_chars)].to_string();
        return (cut, total - max_chars, true);
    }
    let head = keep * 2 / 5;
    let tail = keep - head;
    let head_s = &s[..head_byte_end(s, head)];
    let tail_s = &s[tail_byte_start(s, tail.min(total))..];
    (format!("{head_s}{marker}{tail_s}"), total - keep, true)
}
