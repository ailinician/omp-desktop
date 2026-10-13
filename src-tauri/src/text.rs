//! Byte-bounded text, shared by every path that caps a string before it
//! reaches the UI or a child process: the stderr tail of a dead omp
//! (`agent::reader`), extension status lines and widgets (`agent::ext_ui`),
//! a widget plugin's context fields (`widgets::context`) and the Changes
//! panel's diff (`workspace`).

/// The longest prefix of `text` that fits in `max_bytes` without splitting a
/// char: slicing or `String::truncate` at a non-boundary panics, and none of
/// these inputs is pure ASCII.
pub fn prefix_within(text: &str, max_bytes: usize) -> &str {
    &text[..text.floor_char_boundary(max_bytes)]
}

#[cfg(test)]
mod tests {
    use super::*;

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
