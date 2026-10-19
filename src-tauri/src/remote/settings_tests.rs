use super::*;

#[test]
fn a_remote_json_with_the_retired_allow_http_field_still_loads_without_it() {
    let dir = crate::test_support::tempdir("remote-settings", "allow-http");
    let path = dir.join("remote.json");
    std::fs::write(
        &path,
        r#"{"enabled":true,"interface":"en0","port":7480,"allowHttp":true,"push":{"enabled":true}}"#,
    )
    .unwrap();
    let settings = RemoteSettings::load(&path);
    assert!(settings.enabled && settings.push.enabled);
    assert_eq!(
        (settings.interface.as_deref(), settings.port),
        (Some("en0"), 7480)
    );
    settings.save(&path).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(!saved.contains("allowHttp"), "{saved}");
    assert_eq!(RemoteSettings::load(&path), settings);
}
