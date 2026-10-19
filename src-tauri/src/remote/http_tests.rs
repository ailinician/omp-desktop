use super::*;

#[test]
fn a_stale_server_pairing_reply_says_service_unavailable() {
    let mut out = Vec::new();
    write_response(&mut out, &Response::status(503)).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("HTTP/1.1 503 Service Unavailable\r\n"));
    assert!(text.ends_with("\r\n\r\nService Unavailable"));
}

#[test]
fn an_unknown_status_falls_back_to_internal_server_error() {
    assert_eq!(reason(500), "Internal Server Error");
    assert_eq!(reason(599), "Internal Server Error");
}
