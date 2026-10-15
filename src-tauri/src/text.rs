//! Byte-bounded text, shared by every path that caps a string before it
//! reaches the UI or a child process: the stderr tail of a dead omp
//! (`agent::reader`), extension status lines and widgets (`agent::ext_ui`)
//! and a widget plugin's context fields (`widgets::context`). Also the one
//! way a child's stderr tail becomes a single UI line ([`join_nonblank`]).

/// The longest prefix of `text` that fits in `max_bytes` without splitting a
/// char: slicing or `String::truncate` at a non-boundary panics, and none of
/// these inputs is pure ASCII.
pub fn prefix_within(text: &str, max_bytes: usize) -> &str {
    &text[..text.floor_char_boundary(max_bytes)]
}

/// `lines` trimmed, blank ones dropped, joined with `sep`; `None` when
/// nothing is left. Turns a child's stderr tail into one UI line: a dead
/// omp's (`agent::reader`) and a Chromium that failed to start
/// (`browser::process::stderr`).
pub fn join_nonblank<'a>(lines: impl IntoIterator<Item = &'a str>, sep: &str) -> Option<String> {
    let joined = lines
        .into_iter()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .fold(String::new(), |mut out, line| {
            if !out.is_empty() {
                out.push_str(sep);
            }
            out.push_str(line);
            out
        });
    (!joined.is_empty()).then_some(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_nonblank_trims_and_drops_blank_lines() {
        assert_eq!(join_nonblank([], " "), None);
        assert_eq!(join_nonblank(["", "  "], " "), None);
        assert_eq!(
            join_nonblank([" a ", "", "b"], " | "),
            Some("a | b".to_owned())
        );
    }

    #[test]
    fn prefix_within_cuts_on_char_boundaries() {
        assert_eq!(prefix_within(&"x".repeat(5000), 2048).len(), 2048);
        assert_eq!(
            prefix_within("abc", 2048),
            "abc",
            "shorter text is untouched"
        );
        assert_eq!(prefix_within("abc", 0), "");

        // 2-byte chars, an even cap: whole chars only, and the last is complete.
        let umlauts = "ä".repeat(1500);
        let umlauts = prefix_within(&umlauts, 2048);
        assert_eq!(umlauts.len(), 2048);
        assert_eq!(umlauts.chars().last(), Some('ä'));

        // 3-byte chars: the cap falls inside one, so the cut backs off.
        let euros = "€".repeat(2048);
        let euros = prefix_within(&euros, 2048);
        assert_eq!(euros.len(), 2046);
        assert!(euros.chars().all(|c| c == '€'));
    }
}
