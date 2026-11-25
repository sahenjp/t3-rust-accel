//! UTF-8 boundary helpers + head/tail truncation shared by slice/exec/diff/grep.

/// Largest index <= `i` that is a UTF-8 char boundary.
fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest index >= `i` that is a UTF-8 char boundary.
fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Head 40% / tail 60% byte truncation so errors and summaries at the end
/// survive. Returns (text, truncated).
pub(crate) fn head_tail_bytes<'a>(
    s: &'a str,
    max_bytes: usize,
) -> (std::borrow::Cow<'a, str>, bool) {
    if s.len() <= max_bytes {
        return (std::borrow::Cow::Borrowed(s), false);
    }
    if max_bytes == 0 {
        return (std::borrow::Cow::Borrowed(""), true);
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
        return (
            std::borrow::Cow::Owned(s[..floor_boundary(s, max_bytes)].to_string()),
            true,
        );
    }
    let head = keep * 2 / 5;
    let tail = keep - head;
    let head_end = floor_boundary(s, head);
    let tail_start = ceil_boundary(s, s.len() - tail.min(s.len()));
    let tail_start = tail_start.max(head_end);
    (
        std::borrow::Cow::Owned(format!("{}{}{}", &s[..head_end], marker, &s[tail_start..])),
        true,
    )
}
