//! HTTP delivery and contact validation for Web Push.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use tauri::Url;

use super::devices::PushSubscription;
use super::push_crypto::{encrypt, Vapid};

const TTL_SECS: &str = "3600";
pub const CONTACT_ERROR: &str = "push contact must be a real mailto: or https: address";

/// POST one encrypted message; the HTTP status the push service answered.
pub fn send(
    client: &reqwest::blocking::Client,
    vapid: &Vapid,
    contact: &str,
    sub: &PushSubscription,
    payload: &[u8],
    urgency: &str,
) -> Result<u16, String> {
    let decode = |text: &str| {
        URL_SAFE_NO_PAD
            .decode(text.trim_end_matches('='))
            .map_err(|_| "bad subscription".to_owned())
    };
    let body = encrypt(&decode(&sub.p256dh)?, &decode(&sub.auth)?, payload)?;
    let url = Url::parse(&sub.endpoint).map_err(|_| "bad subscription".to_owned())?;
    let mut audience = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
    if let Some(port) = url.port() {
        let _ = write!(audience, ":{port}");
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let jwt = vapid.jwt(&audience, contact, now)?;
    let response = client
        .post(url)
        .header(
            "Authorization",
            format!("vapid t={jwt}, k={}", vapid.public_b64),
        )
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .header("TTL", TTL_SECS)
        .header("Urgency", urgency)
        .body(body)
        .send()
        .map_err(|error| format!("push request failed: {}", error.without_url()))?;
    Ok(response.status().as_u16())
}

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
        if local.is_empty() {
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
