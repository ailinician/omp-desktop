//! The VAPID contact the push services see.

use tauri::Url;

pub const CONTACT_ERROR: &str = "push contact must be a real mailto: or https: address";

/// The VAPID `sub` claim: Apple answers 403 to placeholder contacts, so
/// only an address that can belong to somebody is accepted. A bare email
/// address becomes `mailto:`.
pub fn contact_uri(raw: &str) -> Result<String, String> {
    let bad = || CONTACT_ERROR.to_owned();
    let raw = raw.trim();
    let uri = if raw.contains(':') || !raw.contains('@') {
        raw.to_owned()
    } else {
        format!("mailto:{raw}")
    };
    let host = if let Some(address) = uri.strip_prefix("mailto:") {
        let address = address.split('?').next().unwrap_or_default();
        let (local, domain) = address.rsplit_once('@').ok_or_else(bad)?;
        if !local_part_ok(local) || !domain_ok(domain) {
            return Err(bad());
        }
        domain.to_owned()
    } else if uri.starts_with("https://") {
        Url::parse(&uri)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .ok_or_else(bad)?
    } else {
        return Err(bad());
    };
    let host = host.to_ascii_lowercase();
    let placeholder = ["invalid", "test", "example", "localhost"]
        .iter()
        .any(|tld| host == *tld || host.ends_with(&format!(".{tld}")))
        || ["example.com", "example.org", "example.net"]
            .iter()
            .any(|name| host == *name);
    if !host.contains('.') || placeholder {
        return Err(bad());
    }
    Ok(uri)
}

/// RFC 5322 dot-atom for the local part: no `@`, spaces, commas (several
/// addresses) or empty / doubled dots.
fn local_part_ok(local: &str) -> bool {
    const SPECIALS: &str = "!#$%&'*+/=?^_`{|}~-";
    !local.is_empty()
        && local.split('.').all(|atom| {
            !atom.is_empty()
                && atom
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || SPECIALS.contains(c))
        })
}

/// A DNS name: labels of letters, digits and inner hyphens, none empty.
fn domain_ok(domain: &str) -> bool {
    domain.len() <= 253
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}
