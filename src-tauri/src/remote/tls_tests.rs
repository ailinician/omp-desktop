use super::*;
use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

fn tempdir(name: &str) -> PathBuf {
    crate::test_support::tempdir("remote-tls", name)
}

fn write_test_cert(dir: &Path, names: &[&str]) -> (PathBuf, PathBuf) {
    let rcgen::CertifiedKey { cert, key_pair } = rcgen::generate_simple_self_signed(
        names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();
    (cert_path, key_path)
}

fn client_config(roots: rustls::RootCertStore) -> rustls::ClientConfig {
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth()
}

fn handshake_ok(config: &Arc<rustls::ServerConfig>, name: &str) {
    let name = ServerName::try_from(name.to_owned()).unwrap();
    // Resolve the configured leaf from a real ClientHello, without bypassing verification.
    let mut hello = rustls::ClientConnection::new(
        Arc::new(client_config(rustls::RootCertStore::empty())),
        name.to_owned(),
    )
    .unwrap();
    let mut bytes = Vec::new();
    hello.write_tls(&mut bytes).unwrap();
    let mut acceptor = rustls::server::Acceptor::default();
    acceptor.read_tls(&mut bytes.as_slice()).unwrap();
    let accepted = acceptor.accept().unwrap().unwrap();
    let certified = config
        .cert_resolver
        .resolve(accepted.client_hello())
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    // Root store owns its DER independently of the resolver's certified key.
    roots.add(certified.cert[0].clone()).unwrap();
    let client = client_config(roots);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server_config = Arc::clone(config);
    let server = std::thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut stream = rustls::StreamOwned::new(
            rustls::ServerConnection::new(server_config).unwrap(),
            socket,
        );
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [7]);
        stream.write_all(&[9]).unwrap();
        stream.flush().unwrap();
    });
    let socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut stream = rustls::StreamOwned::new(
        rustls::ClientConnection::new(Arc::new(client), name).unwrap(),
        socket,
    );
    stream.write_all(&[7]).unwrap();
    stream.flush().unwrap();
    let mut byte = [0];
    stream.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [9]);
    server.join().unwrap();
}

#[test]
fn generated_cert_covers_ip_and_host_and_is_reused() {
    let dir = tempdir("tls-gen");
    let ip = "192.168.1.120".parse().unwrap();
    let a = load_or_generate(&dir, &CertSource::Generated, ip, Some("omp.home.lan")).unwrap();
    let b = load_or_generate(&dir, &CertSource::Generated, ip, Some("omp.home.lan")).unwrap();
    assert_eq!(a.fingerprint, b.fingerprint, "reused");
    let c = load_or_generate(
        &dir,
        &CertSource::Generated,
        "192.168.1.121".parse().unwrap(),
        Some("omp.home.lan"),
    )
    .unwrap();
    assert_ne!(a.fingerprint, c.fingerprint, "regenerated for a new ip");
    handshake_ok(&c.config, "omp.home.lan"); // rustls client trusting that cert
}

#[test]
fn generated_cert_explicitly_allows_tls_server_authentication() {
    let dir = tempdir("server-auth");
    load_or_generate(
        &dir,
        &CertSource::Generated,
        "192.168.1.120".parse().unwrap(),
        None,
    )
    .unwrap();
    let leaf = CertificateDer::from_pem_file(dir.join("remote/cert.pem")).unwrap();
    // DER OBJECT IDENTIFIER 1.3.6.1.5.5.7.3.1 (id-kp-serverAuth).
    let server_auth_oid = [0x06, 0x08, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];
    assert!(
        leaf.windows(server_auth_oid.len())
            .any(|bytes| bytes == server_auth_oid),
        "generated leaf must explicitly include the serverAuth EKU for iOS"
    );
}

#[test]
fn generated_cert_refreshes_when_expired_near_expiry_or_expiry_is_missing() {
    let dir = tempdir("expiry");
    let ip = "192.168.1.120".parse().unwrap();
    let metadata_path = dir.join("remote/cert.json");
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    for (not_after, refresh) in [
        (Some(1), true),
        (Some(now + 30 * 24 * 60 * 60), true),
        (None, true),
        (Some(now + 31 * 24 * 60 * 60), false),
    ] {
        let before = load_or_generate(&dir, &CertSource::Generated, ip, None).unwrap();
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&metadata_path).unwrap()).unwrap();
        if let Some(not_after) = not_after {
            metadata["notAfter"] = not_after.into();
        } else {
            metadata.as_object_mut().unwrap().remove("notAfter");
        }
        std::fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let after = load_or_generate(&dir, &CertSource::Generated, ip, None).unwrap();
        assert_eq!(
            before.fingerprint != after.fingerprint,
            refresh,
            "notAfter={not_after:?}"
        );
        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&metadata_path).unwrap()).unwrap();
        assert!(metadata["notAfter"].as_u64().unwrap() > now + 30 * 24 * 60 * 60);
    }
}

#[test]
fn own_files_that_do_not_cover_the_host_are_refused() {
    let dir = tempdir("tls-own");
    let (cert, key) = write_test_cert(&dir, &["other.example"]); // rcgen in the test
    let err = load_or_generate(
        &dir,
        &CertSource::Files { cert, key },
        "192.168.1.120".parse().unwrap(),
        Some("omp.home.lan"),
    )
    .unwrap_err();
    assert!(err.contains("omp.home.lan"), "{err}");
}

#[test]
fn generated_cert_regenerates_when_host_changes_or_is_removed() {
    let dir = tempdir("host-change");
    let ip = "192.168.1.120".parse().unwrap();
    let a = load_or_generate(&dir, &CertSource::Generated, ip, Some("first.example")).unwrap();
    let b = load_or_generate(&dir, &CertSource::Generated, ip, Some("second.example")).unwrap();
    assert_ne!(a.fingerprint, b.fingerprint);
    let c = load_or_generate(&dir, &CertSource::Generated, ip, None).unwrap();
    assert_ne!(b.fingerprint, c.fingerprint);
    handshake_ok(&c.config, "192.168.1.120");
}

#[test]
fn own_files_accept_wildcard_host_and_require_ip_without_host() {
    let dir = tempdir("own-valid");
    let (cert, key) = write_test_cert(&dir, &["*.home.lan", "192.168.1.120"]);
    let source = CertSource::Files { cert, key };
    let ip = "192.168.1.120".parse().unwrap();
    let material = load_or_generate(&dir, &source, ip, Some("omp.home.lan")).unwrap();
    handshake_ok(&material.config, "omp.home.lan");
    assert_eq!(
        load_or_generate(&dir, &source, ip, None)
            .unwrap()
            .fingerprint,
        material.fingerprint
    );
    let err = load_or_generate(&dir, &source, "192.168.1.121".parse().unwrap(), None).unwrap_err();
    assert!(err.contains("192.168.1.121"), "{err}");
}

#[test]
fn own_files_reject_mismatched_keys_and_malformed_or_missing_pem() {
    let dir = tempdir("own-errors");
    let (cert, key) = write_test_cert(&dir, &["omp.example"]);
    let source = CertSource::Files { cert, key };
    let CertSource::Files { cert, key } = &source else {
        unreachable!("the test uses a file certificate source");
    };
    let ip = "192.168.1.120".parse().unwrap();
    std::fs::write(key, rcgen::KeyPair::generate().unwrap().serialize_pem()).unwrap();
    assert!(load_or_generate(&dir, &source, ip, Some("omp.example")).is_err());
    std::fs::write(cert, "not a certificate").unwrap();
    assert!(load_or_generate(&dir, &source, ip, Some("omp.example")).is_err());
    std::fs::remove_file(cert).unwrap();
    assert!(load_or_generate(&dir, &source, ip, Some("omp.example")).is_err());
}

#[test]
fn generated_files_are_private_and_fingerprint_hashes_leaf() {
    use sha2::{Digest, Sha256};
    let dir = tempdir("private");
    let material = load_or_generate(
        &dir,
        &CertSource::Generated,
        "192.168.1.120".parse().unwrap(),
        None,
    )
    .unwrap();
    let leaf = CertificateDer::from_pem_file(dir.join("remote/cert.pem")).unwrap();
    let expected = Sha256::digest(&leaf)
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":");
    assert_eq!(material.fingerprint, expected);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for file in ["cert.pem", "key.pem"] {
            let mode = std::fs::metadata(dir.join("remote").join(file))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}

#[test]
fn generated_rejects_duplicate_ip_sans_that_omit_host() {
    let dir = tempdir("duplicate-sans");
    let remote = dir.join("remote");
    std::fs::create_dir(&remote).unwrap();
    write_test_cert(&remote, &["192.168.1.120", "192.168.1.120"]);
    let material = load_or_generate(
        &dir,
        &CertSource::Generated,
        "192.168.1.120".parse().unwrap(),
        Some("omp.home.lan"),
    )
    .unwrap();
    handshake_ok(&material.config, "omp.home.lan");
}

#[test]
fn generated_regenerates_when_metadata_is_missing_or_corrupt() {
    let dir = tempdir("metadata");
    let ip = "192.168.1.120".parse().unwrap();
    let first = load_or_generate(&dir, &CertSource::Generated, ip, Some("omp.example")).unwrap();
    let metadata = dir.join("remote/cert.json");
    std::fs::remove_file(&metadata).unwrap();
    let second = load_or_generate(&dir, &CertSource::Generated, ip, Some("omp.example")).unwrap();
    assert_ne!(first.fingerprint, second.fingerprint);
    std::fs::write(&metadata, "broken JSON").unwrap();
    let third = load_or_generate(&dir, &CertSource::Generated, ip, Some("omp.example")).unwrap();
    assert_ne!(second.fingerprint, third.fingerprint);
    handshake_ok(&third.config, "omp.example");
}

#[test]
fn generated_does_not_trust_metadata_for_replaced_certificate() {
    let dir = tempdir("replaced-cert");
    let ip = "192.168.1.120".parse().unwrap();
    load_or_generate(&dir, &CertSource::Generated, ip, Some("omp.example")).unwrap();
    let remote = dir.join("remote");
    write_test_cert(&remote, &["other.example"]);
    let replaced_leaf = CertificateDer::from_pem_file(remote.join("cert.pem")).unwrap();
    let generated =
        load_or_generate(&dir, &CertSource::Generated, ip, Some("omp.example")).unwrap();
    assert_ne!(
        CertificateDer::from_pem_file(remote.join("cert.pem")).unwrap(),
        replaced_leaf
    );
    handshake_ok(&generated.config, "omp.example");
}
