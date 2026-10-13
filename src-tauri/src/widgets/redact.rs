//! Best-effort redaction of credential-shaped text in a failed plugin run's
//! error tail (`run::tail`). A CLI's failure diagnostics (`gh`, `glab`,
//! `curl -v`) can echo an `Authorization` header or a token; the tail is
//! shown verbatim in the rail, so common shapes are masked here. This is a
//! linear, dependency-free scanner over words, not a parser: it cannot know
//! every secret format, which `docs/widgets.md` says.

use std::borrow::Cow;

const PLACEHOLDER: &str = "[REDACTED]";

/// Auth schemes whose following word is a credential.
const SCHEMES: &[&str] = &["bearer", "basic", "token"];

/// Normalized (lowercase, no `-`/`_`) key suffixes whose value is a
/// credential, mirroring `agent::reader::REDACTED_KEYS` for free text.
const KEY_SUFFIXES: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "apikey",
    "authorization",
    "cookie",
];

/// Token prefixes that identify a credential on their own.
const PREFIXES: &[&str] = &[
    "sk-",
    "ghp_",
    "gho_",
    "github_pat_",
    "glpat-",
    "xoxa-",
    "xoxb-",
    "xoxp-",
    "AKIA",
];
const PREFIX_MIN_BODY: usize = 8;

/// Mask credential-shaped words in `text`. Returns the input unchanged
/// (borrowed) when nothing matches.
pub fn redact_secrets(text: &str) -> Cow<'_, str> {
    let mut out = String::new();
    let mut changed = false;
    // State carried between words: the previous word was an auth scheme
    // (`Bearer`) or a credential key (`token:`), so this word is the value;
    // `in_quote` continues a quoted value (`password="a b"`) until it closes.
    let mut value_next = false;
    let mut in_quote = false;
    let mut rest = text;
    while !rest.is_empty() {
        let (sep, after_sep) = split_leading_whitespace(rest);
        let (word, after_word) = split_word(after_sep);
        out.push_str(sep);
        if word.is_empty() {
            break;
        }
        // `Authorization: Bearer x` — the scheme after a key is not the
        // value; the word after the scheme is.
        let masked = if in_quote {
            let closing = if word.trim_end_matches([',', ';']).ends_with('"') {
                &word[word.trim_end_matches([',', ';']).len() - 1..]
            } else {
                ""
            };
            Some(format!("{PLACEHOLDER}{closing}"))
        } else if value_next && !is_scheme(word) {
            Some(mask_value(word))
        } else {
            mask_word(word)
        };
        match masked {
            Some(m) => {
                changed = true;
                out.push_str(&m);
            }
            None => out.push_str(word),
        }
        in_quote = if in_quote {
            !word.trim_end_matches([',', ';']).ends_with('"')
        } else {
            masked_is_value(value_next, word) && opens_quote(word)
        };
        value_next = is_scheme(word) || is_credential_key(word) || inline_scheme(word);
        rest = after_word;
    }
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

/// Whether `word` was masked as a value (positional, or a `key=value` /
/// `key:value` with a credential key) — the only cases a quote can open.
fn masked_is_value(value_next: bool, word: &str) -> bool {
    value_next
        || word
            .split_once(['=', ':'])
            .is_some_and(|(key, _)| key_is_credential(key.trim_matches('"')))
}

fn split_leading_whitespace(s: &str) -> (&str, &str) {
    let end = s.find(|c: char| !c.is_whitespace()).unwrap_or(s.len());
    s.split_at(end)
}

fn split_word(s: &str) -> (&str, &str) {
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    s.split_at(end)
}

fn is_scheme(word: &str) -> bool {
    SCHEMES.iter().any(|s| word.eq_ignore_ascii_case(s))
}

/// `token:` / `"api_key":` — a key with a trailing separator and no inline
/// value, so the credential is the *next* word.
fn is_credential_key(word: &str) -> bool {
    let key = word
        .trim_end_matches([':', '='])
        .trim_matches('"')
        .trim_matches('\'');
    key.len() < word.len() && key_is_credential(key)
}

fn key_is_credential(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|c| *c != '-' && *c != '_')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    KEY_SUFFIXES.iter().any(|s| normalized.ends_with(s))
}

/// A word that is a value by position (after a scheme or key): mask it
/// wholesale, keeping a trailing delimiter so `Bearer abc,` stays readable.
fn mask_value(word: &str) -> String {
    let body = word.trim_end_matches([',', ';', '"', '\'', ')', ']']);
    format!("{PLACEHOLDER}{}", &word[body.len()..])
}

/// `Authorization:Bearer x` — the value part of a credential key is itself
/// a scheme, so the credential is the next word.
fn inline_scheme(word: &str) -> bool {
    word.split_once([':', '='])
        .is_some_and(|(key, value)| key_is_credential(key.trim_matches('"')) && is_scheme(value))
}

/// A quoted value that opens on this word and has not closed yet:
/// `password="a b c"` keeps masking until the closing quote.
fn opens_quote(word: &str) -> bool {
    let value = word.split_once(['=', ':']).map_or(word, |(_, v)| v);
    value.starts_with('"')
        && !(value.len() > 1 && value.trim_end_matches([',', ';']).ends_with('"'))
}

/// A word that is a credential on its own: `key=value`, `"key":"value"`,
/// a known token prefix, or URL userinfo.
fn mask_word(word: &str) -> Option<String> {
    if let Some((key, value)) = word.split_once('=') {
        if key_is_credential(key.trim_matches('"')) && !value.is_empty() {
            return Some(format!("{key}={PLACEHOLDER}"));
        }
    }
    if let Some((key, value)) = word.split_once(':') {
        // `"token":"abc"` / `token:abc` but not `https://…` (value starts
        // with `//`) and not a bare `key:` (handled by `is_credential_key`).
        let value = value.trim_start_matches('"');
        if !value.is_empty() && !value.starts_with("//") && key_is_credential(key.trim_matches('"'))
        {
            return Some(format!("{key}:{PLACEHOLDER}"));
        }
    }
    if let Some(masked) = mask_userinfo(word) {
        return Some(masked);
    }
    let stripped = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && !"-_".contains(c));
    let has_prefix = PREFIXES.iter().any(|p| {
        stripped.starts_with(p)
            && stripped.len() >= p.len() + PREFIX_MIN_BODY
            && stripped[p.len()..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    });
    has_prefix.then(|| word.replacen(stripped, PLACEHOLDER, 1))
}

/// `scheme://user:pass@host…` → `scheme://[REDACTED]@host…`.
fn mask_userinfo(word: &str) -> Option<String> {
    let scheme_end = word.find("://")? + 3;
    let rest = &word[scheme_end..];
    let at = rest.find('@')?;
    let userinfo = &rest[..at];
    if userinfo.is_empty() || userinfo.contains('/') {
        return None;
    }
    Some(format!(
        "{}{PLACEHOLDER}{}",
        &word[..scheme_end],
        &rest[at..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_to_redact_is_borrowed_and_identical() {
        let s = "exit 2: fatal: not a git repository (or any parent)";
        assert!(matches!(redact_secrets(s), Cow::Borrowed(_)));
        assert_eq!(redact_secrets(s), s);
    }

    #[test]
    fn bearer_after_authorization_header() {
        let s = "request failed\nAuthorization: Bearer SYNTH-abc.def_123,\nstatus 401";
        let r = redact_secrets(s);
        assert!(r.contains("Authorization: Bearer [REDACTED],"), "{r}");
        assert!(!r.contains("SYNTH-abc"));
    }

    #[test]
    fn key_value_shapes() {
        let s = "cfg api_key=SYNTH1 x-token: SYNTH2 \"client_secret\":\"SYNTH3\" GITLAB_TOKEN=SYNTH4 limit=5";
        let r = redact_secrets(s);
        for leak in ["SYNTH1", "SYNTH2", "SYNTH3", "SYNTH4"] {
            assert!(!r.contains(leak), "{r}");
        }
        assert!(r.contains("limit=5"), "{r}");
        assert!(r.contains("api_key=[REDACTED]"), "{r}");
    }

    #[test]
    fn known_prefixes() {
        let s = "using ghp_SYNTHETIC0123456789 and (sk-SYNTH0123456789) but not sk-short";
        let r = redact_secrets(s);
        assert!(!r.contains("ghp_SYNTHETIC"), "{r}");
        assert!(r.contains("([REDACTED])"), "{r}");
        assert!(r.contains("sk-short"), "{r}");
    }

    #[test]
    fn multibyte_values_do_not_panic() {
        let r = redact_secrets("Bearer é, token: SYNTHé\u{FFFD}) Basic ö");
        assert!(!r.contains('é') && !r.contains('ö'), "{r}");
        assert!(r.contains("[REDACTED],"), "{r}");
    }

    #[test]
    fn inline_scheme_after_key() {
        let r = redact_secrets("Authorization:Bearer SYNTH1 next");
        assert!(!r.contains("SYNTH1"), "{r}");
        assert!(r.ends_with(" next"), "{r}");
    }

    #[test]
    fn quoted_values_span_whitespace() {
        let r = redact_secrets("password=\"SYNTH a b\", \"client_secret\":\"SYNTH c d\" ok=1");
        assert!(!r.contains("SYNTH"), "{r}");
        assert!(r.ends_with(" ok=1"), "{r}");
        assert!(r.contains("password=[REDACTED]"), "{r}");
    }

    #[test]
    fn url_userinfo() {
        let r = redact_secrets("cloning https://user:SYNTHpw@gitlab.example/g/r.git failed");
        assert_eq!(
            r,
            "cloning https://[REDACTED]@gitlab.example/g/r.git failed"
        );
        assert_eq!(
            redact_secrets("see https://example.test/path"),
            "see https://example.test/path"
        );
    }
}
