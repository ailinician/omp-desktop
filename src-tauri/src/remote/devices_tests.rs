use super::*;
use std::path::PathBuf;

fn tempdir(name: &str) -> PathBuf {
    crate::test_support::tempdir("remote-devices", name)
}

#[test]
fn add_stores_only_the_hash_and_authenticates_the_token() {
    let dir = tempdir("devices");
    let store = DeviceStore::load(dir.join("remote-devices.json"));
    let (dev, token) = store.add("iPhone").unwrap();
    let raw = std::fs::read_to_string(dir.join("remote-devices.json")).unwrap();
    assert!(!raw.contains(&token), "token never written");
    assert!(raw.contains(&dev.token_hash));
    assert_eq!(store.authenticate(&token).unwrap().id, dev.id);
    assert!(store.authenticate("wrong").is_none());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("remote-devices.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn revoke_removes_the_device_and_its_push_subscriptions() {
    let dir = tempdir("revoke");
    let store = DeviceStore::load(dir.join("remote-devices.json"));
    let (dev, token) = store.add("iPhone").unwrap();
    store
        .set_push(
            &dev.id,
            vec![PushSubscription {
                endpoint: "https://web.push.apple.com/x".into(),
                p256dh: "k".into(),
                auth: "a".into(),
            }],
        )
        .unwrap();
    assert!(store.revoke(&dev.id));
    assert!(store.authenticate(&token).is_none());
    let reloaded = DeviceStore::load(dir.join("remote-devices.json"));
    assert!(reloaded.list().is_empty());
}

#[test]
fn rename_applies_the_pairing_name_rules() {
    let dir = tempdir("rename-rules");
    let store = DeviceStore::load(dir.join("remote-devices.json"));
    let (dev, _) = store.add("iPhone").unwrap();
    store.rename(&dev.id, "  Kitchen iPad \n").unwrap();
    assert_eq!(store.list()[0].name, "Kitchen iPad");
    store.rename(&dev.id, &"é".repeat(200)).unwrap();
    assert_eq!(store.list()[0].name.chars().count(), MAX_DEVICE_NAME_CHARS);
    for blank in ["", "   ", "\t\n"] {
        assert!(store.rename(&dev.id, blank).is_err(), "{blank:?}");
    }
    assert_eq!(
        store.list()[0].name.chars().count(),
        MAX_DEVICE_NAME_CHARS,
        "rejected rename changes nothing"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn rename_and_push_survive_reload_and_unknown_ids_fail() {
    let dir = tempdir("rename-push");
    let path = dir.join("remote-devices.json");
    let store = DeviceStore::load(path);
    let (dev, token) = store.add("iPhone").unwrap();
    assert_eq!(dev.id.len(), 12);
    assert_eq!(token.len(), 43);
    assert_eq!(dev.token_hash.len(), 64);
    assert!(dev.paired_at > 0);
    store.rename(&dev.id, "Phone").unwrap();
    store
        .set_push(
            &dev.id,
            vec![PushSubscription {
                endpoint: "https://example.test/push".into(),
                p256dh: "key".into(),
                auth: "auth".into(),
            }],
        )
        .unwrap();
    let reloaded = DeviceStore::load(store.path);
    let saved = reloaded.authenticate(&token).unwrap();
    assert_eq!(saved.name, "Phone");
    assert_eq!(saved.push.len(), 1);
    assert_eq!(saved.push[0].endpoint, "https://example.test/push");
    assert_eq!(saved.push[0].p256dh, "key");
    assert_eq!(saved.push[0].auth, "auth");
    assert!(reloaded.rename("missing", "no").is_err());
    assert!(reloaded.set_push("missing", vec![]).is_err());
    assert!(!reloaded.revoke("missing"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn corrupt_store_is_empty_but_never_overwritten() {
    let dir = tempdir("corrupt");
    let path = dir.join("remote-devices.json");
    std::fs::write(&path, "broken").unwrap();
    let store = DeviceStore::load(path);
    assert!(store.list().is_empty());
    assert!(store.authenticate("anything").is_none());
    assert!(store.add("Phone").is_err());
    assert_eq!(std::fs::read_to_string(&store.path).unwrap(), "broken");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn legacy_devices_default_push_and_hash_known_token() {
    let dir = tempdir("legacy");
    let path = dir.join("remote-devices.json");
    std::fs::write(&path, r#"[{"id":"legacy","name":"Phone","tokenHash":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","pairedAt":1,"lastSeen":1}]"#).unwrap();
    let store = DeviceStore::load(path);
    assert!(store.list()[0].push.is_empty());
    let dev = store.authenticate("abc").unwrap();
    assert_eq!(dev.id, "legacy");
    assert!(dev.last_seen > 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn last_seen_updates_in_memory_and_persists_at_most_once_per_minute() {
    let dir = tempdir("last-seen");
    let path = dir.join("remote-devices.json");
    let store = DeviceStore::load(path);
    let (_, token) = store.add("Phone").unwrap();
    let now = std::time::Instant::now();
    let original = std::fs::read(&store.path).unwrap();
    let seen = store.authenticate_at(&token, 2_000_000_000, now).unwrap();
    assert_eq!(seen.last_seen, 2_000_000_000);
    assert_eq!(std::fs::read(&store.path).unwrap(), original);
    let first_write = now + std::time::Duration::from_secs(61);
    store
        .authenticate_at(&token, 2_000_000_061, first_write)
        .unwrap();
    let saved: Vec<Device> = serde_json::from_slice(&std::fs::read(&store.path).unwrap()).unwrap();
    assert_eq!(saved[0].last_seen, 2_000_000_061);
    store
        .authenticate_at(
            &token,
            2_000_000_062,
            first_write + std::time::Duration::from_secs(1),
        )
        .unwrap();
    let saved: Vec<Device> = serde_json::from_slice(&std::fs::read(&store.path).unwrap()).unwrap();
    assert_eq!(saved[0].last_seen, 2_000_000_061);
    store
        .authenticate_at(
            &token,
            2_000_000_121,
            first_write + std::time::Duration::from_secs(60),
        )
        .unwrap();
    let saved: Vec<Device> = serde_json::from_slice(&std::fs::read(&store.path).unwrap()).unwrap();
    assert_eq!(saved[0].last_seen, 2_000_000_121);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn write_failures_never_claim_to_revoke_or_change_cached_devices() {
    let dir = tempdir("write-failure");
    let path = dir.join("remote-devices.json");
    let store = DeviceStore::load(path);
    let (dev, token) = store.add("Phone").unwrap();
    std::fs::write(&store.path, "broken").unwrap();
    assert!(store.rename(&dev.id, "New name").is_err());
    assert!(store.set_push(&dev.id, vec![]).is_err());
    assert!(!store.revoke(&dev.id));
    assert_eq!(store.authenticate(&token).unwrap().name, "Phone");
    assert_eq!(store.list().len(), 1);
    assert_eq!(std::fs::read_to_string(&store.path).unwrap(), "broken");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn last_seen_flush_does_not_overwrite_a_corrupt_store() {
    let dir = tempdir("last-seen-corrupt");
    let path = dir.join("remote-devices.json");
    let store = DeviceStore::load(path);
    let (_, token) = store.add("Phone").unwrap();
    std::fs::write(&store.path, "broken").unwrap();
    let clock = std::time::Instant::now() + std::time::Duration::from_secs(61);
    assert!(store
        .authenticate_at(&token, 2_000_000_000, clock)
        .is_some());
    assert_eq!(std::fs::read_to_string(&store.path).unwrap(), "broken");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn every_device_authenticates_after_a_mismatching_entry() {
    let dir = tempdir("multiple-devices");
    let store = DeviceStore::load(dir.join("remote-devices.json"));
    let (first, first_token) = store.add("First").unwrap();
    let (second, second_token) = store.add("Second").unwrap();
    assert_ne!(first.id, second.id);
    assert_ne!(first_token, second_token);
    assert_eq!(store.authenticate(&first_token).unwrap().id, first.id);
    assert_eq!(store.authenticate(&second_token).unwrap().id, second.id);
    assert!(store.revoke(&first.id));
    assert!(store.authenticate(&first_token).is_none());
    assert_eq!(store.authenticate(&second_token).unwrap().id, second.id);
    std::fs::remove_dir_all(dir).unwrap();
}

/// Every temporary file the store writes is private from the moment it
/// exists, and the final file stays 0600.
#[cfg(unix)]
#[test]
fn the_store_is_never_readable_by_others_while_it_is_written() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempdir("private-writes");
    let store = DeviceStore::load(dir.join("remote-devices.json"));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher = {
        let (dir, stop) = (dir.clone(), std::sync::Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut loose = Vec::new();
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                    let mode = entry
                        .metadata()
                        .map(|meta| meta.permissions().mode() & 0o777);
                    if let Some(mode) = mode.ok().filter(|mode| {
                        mode & 0o077 != 0 && entry.path().extension() != Some("lock".as_ref())
                    }) {
                        loose.push((entry.file_name(), mode));
                    }
                }
            }
            loose
        })
    };
    for n in 0..300 {
        store.add(&format!("phone {n}")).unwrap();
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let loose = watcher.join().unwrap();
    assert!(loose.is_empty(), "group/other-accessible files: {loose:?}");
    let mode = std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    std::fs::remove_dir_all(dir).unwrap();
}
