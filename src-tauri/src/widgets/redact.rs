//! Best-effort redaction of credential-shaped text that a plugin printed and
//! the rail would show: the tail of a failed run and the preview of a non-JSON
//! stdout (`run.rs`). A CLI's diagnostics (`gh`, `glab`, `curl -v`) can echo
//! an `Authorization` header, a token, or a URL with a password.
//!
//! Keys are the shared [`REDACTED_KEYS`] (the agent reader's JSON list) plus
//! a few free-text spellings, matched with the same
//! [`normalized_key_ends_with`]. Recognised shapes:
//! - `key=value` / `key:value` / `"key":"value"` anywhere inside a word, so
//!   compact JSON, query strings and connection strings are covered;
//! - `key: value`, `key= value`, and for the [`REDACTED_KEYS`] also
//!   `key = value` (the value is the next word); a quoted value is masked up
//!   to its closing quote, at most to the end of the line;
//! - `Bearer x` anywhere, `token x` / `basic x` only after a credential key
//!   (`Authorization: token x`);
//! - known token prefixes ([`PREFIXES`]) and URL userinfo
//!   (`https://user:pw@host`).
//!
//! It is a linear scanner, not a parser: unknown shapes pass through. The
//! input is returned borrowed when nothing matches; the output is built once,
//! only when something is masked.
//!
//! [`REDACTED_KEYS`]: crate::redaction::REDACTED_KEYS

use std::borrow::Cow;

use crate::redaction::{is_credential_key, normalized_key_ends_with, REDACTED_PLACEHOLDER};

/// Free-text key spellings on top of [`REDACTED_KEYS`]. Only in the tight
/// forms (`token=x`, `token: x`): a bare `token = …` is usually source code
/// in a traceback, not a secret.
///
/// [`REDACTED_KEYS`]: crate::redaction::REDACTED_KEYS
const FREE_TEXT_KEYS: &[&str] = &["token", "passwd", "cookie"];

/// Token prefixes that identify a credential on their own.
const PREFIXES: &[&str] = &[
    "sk-",
    "ghp_",
    "gho_",
    "ghs_",
    "ghu_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "glcbt-",
    "glrt-",
    "xoxa-",
    "xoxb-",
    "xoxp-",
    "AKIA",
];
const PREFIX_MIN_BODY: usize = 8;

/// Bytes that end a key: a new key starts after them.
const KEY_BOUNDARY: &[u8] = b",;&?{}[]()<>|";
/// Bytes that end an unquoted value inside a word.
const VALUE_END: &[u8] = b",;&)]}>\"'";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Expect {
    Nothing,
    /// The previous word was a strong credential key without separator
    /// (`password`); a lone `=`/`:` word makes the word after it the value.
    Separator,
    /// The next word is a credential value, unless it is an auth scheme.
    Value,
    /// The next word is the credential after an auth scheme.
    SchemeValue,
}

/// Mask credential-shaped text. Borrowed when nothing matches.
pub fn redact_secrets(text: &str) -> Cow<'_, str> {
    let mut scan = Scanner {
        text,
        masks: Vec::new(),
        expect: Expect::Nothing,
        quote: None,
    };
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let gap = i;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if bytes[gap..i].contains(&b'\n') {
            scan.quote = None; // an unclosed quote never masks past its line
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if start < i {
            scan.word(start, i);
        }
    }
    scan.finish()
}

struct Scanner<'a> {
    text: &'a str,
    /// Byte ranges to replace, each on char boundaries (they start and end at
    /// ASCII bytes or word edges). Unordered; merged in [`Self::finish`].
    masks: Vec<(usize, usize)>,
    expect: Expect,
    /// Open quote of a value that spans words.
    quote: Option<u8>,
}

impl<'a> Scanner<'a> {
    fn mask(&mut self, from: usize, to: usize) {
        if from < to {
            self.masks.push((from, to));
        }
    }

    fn word(&mut self, start: usize, end: usize) {
        let text = self.text;
        let word = &text[start..end];
        self.prefixed_tokens(start, end);
        self.userinfo(start, end);
        let cursor = if let Some(q) = self.quote {
            let Some(close) = find_quote(text.as_bytes(), start, end, q) else {
                self.mask(start, end);
                return;
            };
            self.mask(start, close);
            self.quote = None;
            close + 1
        } else {
            match std::mem::replace(&mut self.expect, Expect::Nothing) {
                Expect::Value if is_one_of(word, &["bearer", "basic", "token"]) => {
                    self.expect = Expect::SchemeValue;
                    return;
                }
                Expect::Value | Expect::SchemeValue => {
                    let next = self.value(start, end);
                    if self.quote.is_some() {
                        return;
                    }
                    next
                }
                Expect::Separator if word == "=" || word == ":" => {
                    self.expect = Expect::Value;
                    return;
                }
                Expect::Nothing | Expect::Separator => {
                    if word.eq_ignore_ascii_case("bearer") {
                        self.expect = Expect::SchemeValue;
                        return;
                    }
                    if !word.contains(['=', ':']) && is_credential_key(trim_quotes(word)) {
                        self.expect = Expect::Separator;
                        return;
                    }
                    start
                }
            }
        };
        self.pairs(cursor, end);
    }

    /// Mask the value starting at `v` (non-empty, before `end`) and return
    /// where scanning continues. A quoted value runs to its closing quote; if
    /// that is not in this word, the quote stays open for the next words.
    fn value(&mut self, v: usize, end: usize) -> usize {
        let bytes = self.text.as_bytes(); // `&'a`, independent of the `&mut self` below
        let q = bytes[v];
        if q == b'"' || q == b'\'' {
            if let Some(close) = find_quote(bytes, v + 1, end, q) {
                self.mask(v + 1, close);
                return close + 1;
            }
            self.mask(v + 1, end);
            self.quote = Some(q);
            return end;
        }
        let stop = bytes[v..end]
            .iter()
            .position(|b| VALUE_END.contains(b))
            .map_or(end, |p| v + p);
        self.mask(v, stop);
        stop
    }

    /// `key=value` / `key:value` pairs inside `[from, end)`: every separator
    /// is checked, a key starts after the previous separator or boundary.
    fn pairs(&mut self, from: usize, end: usize) {
        let text = self.text;
        let bytes = text.as_bytes();
        let mut key_start = from;
        let mut i = from;
        while i < end {
            let b = bytes[i];
            if b == b'=' || b == b':' {
                let v = i + 1;
                let url = b == b':' && bytes[v..end].starts_with(b"//");
                if !url && is_key(trim_quotes(&text[key_start..i])) {
                    if v == end {
                        self.expect = Expect::Value;
                        return;
                    }
                    if is_one_of(&text[v..end], &["bearer", "basic", "token"]) {
                        self.expect = Expect::SchemeValue;
                        return;
                    }
                    i = self.value(v, end);
                    if self.quote.is_some() {
                        return;
                    }
                    key_start = i;
                    continue;
                }
                key_start = v;
            } else if KEY_BOUNDARY.contains(&b) {
                key_start = i + 1;
            }
            i += 1;
        }
    }

    /// Runs of `[A-Za-z0-9_-]` that start with a known token prefix.
    fn prefixed_tokens(&mut self, start: usize, end: usize) {
        let text = self.text;
        let bytes = text.as_bytes();
        let mut i = start;
        while i < end {
            if !is_token_byte(bytes[i]) {
                i += 1;
                continue;
            }
            let run_start = i;
            while i < end && is_token_byte(bytes[i]) {
                i += 1;
            }
            let run = &text[run_start..i];
            if PREFIXES
                .iter()
                .any(|p| run.starts_with(p) && run.len() >= p.len() + PREFIX_MIN_BODY)
            {
                self.mask(run_start, i);
            }
        }
    }

    /// `scheme://userinfo@host` → the userinfo. The authority runs to the
    /// first `/`, `?` or `#`, and the userinfo to its *last* `@`, as in URL
    /// parsers and curl: `https://u:p@ss@host` has the password `p@ss`.
    fn userinfo(&mut self, start: usize, end: usize) {
        let word = &self.text[start..end]; // `&'a`, independent of the `&mut self` below
        let mut search = 0;
        while let Some(found) = word[search..].find("://") {
            let info_start = search + found + 3;
            let rest = &word.as_bytes()[info_start..];
            let authority = rest
                .iter()
                .position(|b| matches!(b, b'/' | b'?' | b'#'))
                .unwrap_or(rest.len());
            if let Some(at) = rest[..authority].iter().rposition(|&b| b == b'@') {
                self.mask(start + info_start, start + info_start + at);
            }
            // Not past the authority: its end can be the `/` of the next
            // URL's `://` in the same word.
            search = info_start;
        }
    }

    fn finish(mut self) -> Cow<'a, str> {
        if self.masks.is_empty() {
            return Cow::Borrowed(self.text);
        }
        self.masks.sort_unstable();
        let mut out = String::with_capacity(self.text.len());
        let mut done = 0;
        for &(from, to) in &self.masks {
            if to <= done {
                continue; // inside the previous mask
            }
            if from >= done {
                out.push_str(&self.text[done..from]);
                out.push_str(REDACTED_PLACEHOLDER);
            } // else overlapping: extend the placeholder already written
            done = to;
        }
        out.push_str(&self.text[done..]);
        Cow::Owned(out)
    }
}

const fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn trim_quotes(s: &str) -> &str {
    s.trim_matches(['"', '\''])
}

fn is_one_of(word: &str, names: &[&str]) -> bool {
    names.iter().any(|n| word.eq_ignore_ascii_case(n))
}

fn is_key(key: &str) -> bool {
    is_credential_key(key)
        || FREE_TEXT_KEYS
            .iter()
            .any(|k| normalized_key_ends_with(key, k))
}

/// First `q` in `bytes[from..end]` not escaped by a backslash.
fn find_quote(bytes: &[u8], from: usize, end: usize, q: u8) -> Option<usize> {
    (from..end).find(|&i| bytes[i] == q && (i == 0 || bytes[i - 1] != b'\\'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_masked(input: &str, leaks: &[&str]) -> String {
        let out = redact_secrets(input).into_owned();
        for leak in leaks {
            assert!(!out.contains(leak), "{leak} leaked: {out}");
        }
        assert!(out.contains(REDACTED_PLACEHOLDER), "{out}");
        out
    }

    fn assert_untouched(input: &str) {
        let out = redact_secrets(input);
        assert!(matches!(out, Cow::Borrowed(_)), "{input} changed to {out}");
    }

    #[test]
    fn nothing_to_redact_is_borrowed_and_identical() {
        assert_untouched("exit 2: fatal: not a git repository (or any parent)");
        assert_untouched(&"plain log line with https://example.test/path\n".repeat(2000));
    }

    #[test]
    fn bearer_after_authorization_header() {
        let out = assert_masked(
            "request failed\nAuthorization: Bearer SYNTH-abc.def_123,\nstatus 401",
            &["SYNTH-abc"],
        );
        assert!(out.contains("Authorization: Bearer [REDACTED],"), "{out}");
        assert!(out.ends_with("\nstatus 401"), "{out}");
    }

    #[test]
    fn token_and_basic_schemes_after_a_credential_key() {
        let out = assert_masked("Authorization: token SYNTH1 next", &["SYNTH1"]);
        assert_eq!(out, "Authorization: token [REDACTED] next");
        let out = assert_masked("authorization: Basic SYNTH2=", &["SYNTH2"]);
        assert_eq!(out, "authorization: Basic [REDACTED]");
    }

    #[test]
    fn inline_scheme_after_key() {
        let out = assert_masked("Authorization:Bearer SYNTH1 next", &["SYNTH1"]);
        assert!(out.ends_with(" next"), "{out}");
    }

    #[test]
    fn key_value_shapes() {
        let out = assert_masked(
            "cfg api_key=SYNTH1 x-token: SYNTH2 \"client_secret\":\"SYNTH3\" GITLAB_TOKEN=SYNTH4 limit=5",
            &["SYNTH1", "SYNTH2", "SYNTH3", "SYNTH4"],
        );
        assert!(out.contains("limit=5"), "{out}");
        assert!(out.contains("api_key=[REDACTED]"), "{out}");
    }

    #[test]
    fn shared_keys_cover_aws_and_private_keys() {
        assert_masked("AWS_SECRET_ACCESS_KEY=SYNTHaws", &["SYNTHaws"]);
        assert_masked("private_key: SYNTHpk", &["SYNTHpk"]);
        assert_masked("aws_secret_access_key = SYNTHini", &["SYNTHini"]);
        assert_masked("credentials=SYNTHcred", &["SYNTHcred"]);
        assert_masked("passwd=SYNTHpw cookie: SYNTHck", &["SYNTHpw", "SYNTHck"]);
    }

    #[test]
    fn compact_json_masks_every_credential_pair() {
        let out = assert_masked(r#"{"login":"x","token":"SYNTH"}"#, &["SYNTH"]);
        assert_eq!(out, r#"{"login":"x","token":"[REDACTED]"}"#);
    }

    #[test]
    fn later_query_parameter_is_masked() {
        let out = assert_masked("https://h/x?page=2&access_token=SYNTH&x=1", &["SYNTH"]);
        assert_eq!(out, "https://h/x?page=2&access_token=[REDACTED]&x=1");
    }

    #[test]
    fn connection_string_password_is_masked() {
        let out = assert_masked("Server=x;Password=SYNTH;Database=d", &["SYNTH"]);
        assert_eq!(out, "Server=x;Password=[REDACTED];Database=d");
    }

    #[test]
    fn userinfo_and_query_token_are_both_masked() {
        let out = assert_masked("https://u:SYNTH@h/?token=T0KEN", &["SYNTH", "T0KEN"]);
        assert_eq!(out, "https://[REDACTED]@h/?token=[REDACTED]");
    }

    #[test]
    fn url_userinfo() {
        assert_eq!(
            redact_secrets("cloning https://user:SYNTHpw@gitlab.example/g/r.git failed"),
            "cloning https://[REDACTED]@gitlab.example/g/r.git failed"
        );
        assert_untouched("see https://example.test/path?q=a@b");
    }

    #[test]
    fn url_userinfo_runs_to_the_last_at_of_the_authority() {
        let out = assert_masked("fatal: https://u:SY@NTH@h/x?q=a@b", &["SY", "NTH"]);
        assert_eq!(out, "fatal: https://[REDACTED]@h/x?q=a@b");
        // Two URLs in one word: the second one's userinfo is still found.
        let out = assert_masked("https://h,https://u:SYNTH@h2/", &["SYNTH"]);
        assert_eq!(out, "https://h,https://[REDACTED]@h2/");
    }

    #[test]
    fn known_prefixes() {
        let out = assert_masked(
            "using ghp_SYNTHETIC0123456789 and (sk-SYNTH0123456789) but not sk-short",
            &["ghp_SYNTHETIC", "sk-SYNTH"],
        );
        assert!(out.contains("([REDACTED])"), "{out}");
        assert!(out.contains("sk-short"), "{out}");
        for token in [
            "ghs_SYNTH0123456789",
            "ghu_SYNTH0123456789",
            "ghr_SYNTH0123456789",
            "glcbt-SYNTH0123456789",
            "glrt-SYNTH0123456789",
            "AKIASYNTH0123456789",
        ] {
            assert_eq!(
                redact_secrets(&format!("x {token} y")),
                "x [REDACTED] y",
                "{token}"
            );
        }
        assert_untouched("disk-0123456789abc task-ghp_ is fine");
    }

    #[test]
    fn multibyte_values_do_not_panic() {
        let out = assert_masked(
            "Bearer é, token: SYNTHé\u{FFFD}) Authorization: Basic ö",
            &["é", "ö"],
        );
        assert!(out.contains("[REDACTED],"), "{out}");
        assert!(out.contains("[REDACTED])"), "{out}");
    }

    #[test]
    fn quoted_values_span_whitespace() {
        let out = assert_masked(
            "password=\"SYNTH a b\", \"client_secret\":\"SYNTH c d\" ok=1",
            &["SYNTH", " a ", " c "],
        );
        assert!(out.ends_with(" ok=1"), "{out}");
        assert!(out.starts_with("password=\"[REDACTED]"), "{out}");
    }

    #[test]
    fn closing_quote_mid_word_ends_the_value() {
        let out = assert_masked(r#"{"password":"a b","user":"x"} after"#, &["a b"]);
        assert!(out.ends_with(r#"","user":"x"} after"#), "{out}");
    }

    #[test]
    fn quoted_value_containing_a_separator_is_masked_whole() {
        let out = assert_masked(r#""password": "a:b c" next"#, &["a:b", " c\""]);
        assert!(out.ends_with("\" next"), "{out}");
    }

    #[test]
    fn escaped_quote_does_not_close_the_value() {
        assert_masked(r#"{"token":"a\"SYNTH c","x":1}"#, &["SYNTH", " c"]);
    }

    #[test]
    fn unclosed_quote_stops_at_end_of_line() {
        let out = assert_masked(
            "password=\"SYNTH oops\nlater line stays",
            &["SYNTH", "oops"],
        );
        assert!(out.ends_with("\nlater line stays"), "{out}");
    }

    #[test]
    fn ordinary_diagnostics_are_not_masked() {
        assert_untouched("    token = os.environ[\"GITLAB_TOKEN\"]");
        assert_untouched("error: token expired");
        assert_untouched("basic auth failed");
        assert_untouched("missing \"password\" in config");
        assert_untouched("the password is wrong");
        assert_untouched("usage: input_tokens=3 output_tokens=5");
        assert_untouched("at 12:30:45 C:\\path\\file.txt");
    }
}
