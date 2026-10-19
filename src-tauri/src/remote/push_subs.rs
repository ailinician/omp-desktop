//! Validation of a phone's Web Push subscription. The desktop later POSTs to
//! the endpoint, so a phone must not be able to point it at an arbitrary
//! host: only the push services of the browsers' vendors are accepted.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::Value;
use tauri::Url;

use super::devices::PushSubscription;

const MAX_FIELD_LEN: usize = 2048;
/// Uncompressed P-256 point.
const P256DH_BYTES: usize = 65;
const AUTH_BYTES: usize = 16;

fn allowed_host(host: &str) -> bool {
    let under = |suffix: &str| {
        host.strip_suffix(suffix)
            .is_some_and(|label| !label.is_empty())
    };
    matches!(
        host,
        "fcm.googleapis.com" | "updates.push.services.mozilla.com"
    ) || under(".push.apple.com")
        || under(".notify.windows.com")
}

pub fn endpoint_ok(endpoint: &str) -> bool {
    let Ok(url) = Url::parse(endpoint) else {
        return false;
    };
    // `port()` is `None` for the scheme's default, so `:443` passes too.
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.domain().is_some_and(allowed_host)
}

fn key_ok(value: &str, bytes: usize) -> bool {
    URL_SAFE_NO_PAD
        .decode(value.trim_end_matches('='))
        .is_ok_and(|raw| raw.len() == bytes)
}

fn text(from: &Value, name: &str) -> Option<String> {
    from.get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= MAX_FIELD_LEN)
        .map(str::to_owned)
}

/// A browser's `{endpoint, keys: {p256dh, auth}}` (keys may also sit at the
/// top level), or the error to tell the phone.
pub fn parse_subscription(sub: Option<&Value>) -> Result<PushSubscription, String> {
    let bad = || "bad subscription".to_owned();
    let sub = sub.ok_or_else(bad)?;
    let keys = sub.get("keys").unwrap_or(sub);
    let (Some(endpoint), Some(p256dh), Some(auth)) = (
        text(sub, "endpoint"),
        text(keys, "p256dh"),
        text(keys, "auth"),
    ) else {
        return Err(bad());
    };
    if !endpoint_ok(&endpoint) || !key_ok(&p256dh, P256DH_BYTES) || !key_ok(&auth, AUTH_BYTES) {
        return Err(bad());
    }
    Ok(PushSubscription {
        endpoint,
        p256dh,
        auth,
    })
}

#[cfg(test)]
#[path = "push_subs_tests.rs"]
mod tests;
