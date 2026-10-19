use super::*;
use serde_json::json;

fn key(bytes: usize) -> String {
    URL_SAFE_NO_PAD.encode(vec![7_u8; bytes])
}

fn sub(endpoint: &str) -> Value {
    json!({ "endpoint": endpoint, "keys": { "p256dh": key(65), "auth": key(16) } })
}

#[test]
fn vendor_endpoints_are_accepted() {
    for endpoint in [
        "https://fcm.googleapis.com/fcm/send/abc",
        "https://updates.push.services.mozilla.com/wpush/v2/abc",
        "https://web.push.apple.com/QWxs",
        "https://wns2-par02p.notify.windows.com/w/?token=x",
        "https://fcm.googleapis.com:443/fcm/send/abc",
    ] {
        assert!(
            parse_subscription(Some(&sub(endpoint))).is_ok(),
            "{endpoint}"
        );
    }
}

#[test]
fn other_endpoints_are_rejected() {
    for endpoint in [
        "http://fcm.googleapis.com/fcm/send/abc",
        "https://127.0.0.1/x",
        "https://[::1]/x",
        "https://192.168.1.5/x",
        "https://evil.example/x",
        "https://fcm.googleapis.com.evil.example/x",
        "https://evilpush.apple.com.evil.example/",
        "https://push.apple.com/x",
        "https://user:pw@fcm.googleapis.com/x",
        "https://fcm.googleapis.com@evil.example/x",
        "https://fcm.googleapis.com:8443/x",
        "https://fcm.googleapis.com./x",
        "ftp://fcm.googleapis.com/x",
        "not a url",
    ] {
        assert!(
            parse_subscription(Some(&sub(endpoint))).is_err(),
            "{endpoint}"
        );
    }
}

#[test]
fn keys_must_be_base64url_of_the_right_size() {
    let endpoint = "https://fcm.googleapis.com/fcm/send/abc";
    for (p256dh, auth) in [
        (key(64), key(16)),
        (key(65), key(15)),
        ("k".to_owned(), "a".to_owned()),
        (format!("{}+/", key(63)), key(16)),
    ] {
        let value = json!({ "endpoint": endpoint, "keys": { "p256dh": p256dh, "auth": auth } });
        assert!(parse_subscription(Some(&value)).is_err());
    }
    assert!(parse_subscription(None).is_err());
}
