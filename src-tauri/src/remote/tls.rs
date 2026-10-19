use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyUsagePurpose};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer, ServerName};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::io::Write;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub enum CertSource {
    Generated,
    Files { cert: PathBuf, key: PathBuf },
}

#[derive(Debug)]
pub struct TlsMaterial {
    pub config: Arc<rustls::ServerConfig>,
    pub fingerprint: String,
}

pub fn load_or_generate(
    dir: &Path,
    source: &CertSource,
    ip: Ipv4Addr,
    host: Option<&str>,
) -> Result<TlsMaterial, String> {
    if let Some(host) = host {
        ServerName::try_from(host).map_err(|error| format!("invalid host name {host}: {error}"))?;
    }
    match source {
        CertSource::Files { cert, key } => {
            let (chain, key) = read_pair(cert, key)?;
            check_name(&chain[0], ip, host)?;
            material(chain, key)
        }
        CertSource::Generated => generated(dir, ip, host),
    }
}

#[derive(Deserialize)]
struct GeneratedCertificate {
    ip: Ipv4Addr,
    host: Option<String>,
    fingerprint: String,
    #[serde(rename = "notAfter")]
    not_after: u64,
}

fn generated(dir: &Path, ip: Ipv4Addr, host: Option<&str>) -> Result<TlsMaterial, String> {
    let remote = dir.join("remote");
    let cert_path = remote.join("cert.pem");
    let key_path = remote.join("key.pem");
    let metadata_path = remote.join("cert.json");
    let now = SystemTime::now();
    let renew_before = (now + Duration::from_hours(30 * 24))
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|error| format!("read TLS certificate time: {error}"))?
        .as_secs();
    let stored = std::fs::read(&metadata_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<GeneratedCertificate>(&bytes).ok())
        .filter(|stored| {
            stored.ip == ip && stored.host.as_deref() == host && stored.not_after > renew_before
        });
    if let Some(stored) = stored {
        if let Ok((chain, key)) = read_pair(&cert_path, &key_path) {
            if let Ok(material) = material(chain, key) {
                // Bind the recorded SAN set to this exact leaf, not a replaced PEM.
                if material.fingerprint == stored.fingerprint {
                    return Ok(material);
                }
            }
        }
    }
    let mut names = vec![ip.to_string()];
    if let Some(host) = host {
        names.push(host.to_owned());
    }
    let mut params = CertificateParams::new(names).map_err(|error| error.to_string())?;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.not_before = now.into();
    params.not_after = params.not_before + Duration::from_hours(2 * 365 * 24);
    let not_after = params.not_after.unix_timestamp();
    let key = rcgen::KeyPair::generate().map_err(|error| format!("generate TLS key: {error}"))?;
    let cert = params
        .self_signed(&key)
        .map_err(|error| format!("generate TLS certificate: {error}"))?;
    crate::private_fs::private_dirs(true)
        .create(&remote)
        .map_err(|error| format!("create {}: {error}", remote.display()))?;
    write_private(&key_path, key.serialize_pem().as_bytes())?;
    write_private(&cert_path, cert.pem().as_bytes())?;
    let material = material(
        vec![cert.der().to_owned()],
        PrivateKeyDer::Pkcs8(key.serialize_der().into()),
    )?;
    // Publish metadata last: incomplete writes cannot authorize reuse of a new pair.
    let metadata = serde_json::to_vec(&serde_json::json!({
        "ip": ip,
        "host": host,
        "fingerprint": material.fingerprint,
        "notAfter": not_after,
    }))
    .map_err(|error| format!("encode TLS certificate metadata: {error}"))?;
    write_private(&metadata_path, &metadata)?;
    Ok(material)
}

fn read_pair(
    cert_path: &Path,
    key_path: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), String> {
    let chain = CertificateDer::pem_file_iter(cert_path)
        .map_err(|error| format!("read {}: {error}", cert_path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read {}: {error}", cert_path.display()))?;
    if chain.is_empty() {
        return Err(format!("no certificates in {}", cert_path.display()));
    }
    let key = PrivateKeyDer::from_pem_file(key_path)
        .map_err(|error| format!("read {}: {error}", key_path.display()))?;
    Ok((chain, key))
}

fn check_name(leaf: &CertificateDer<'_>, ip: Ipv4Addr, host: Option<&str>) -> Result<(), String> {
    let name = host.map_or_else(|| ip.to_string(), str::to_owned);
    let server_name = ServerName::try_from(name.as_str())
        .map_err(|error| format!("invalid TLS name {name}: {error}"))?;
    webpki::EndEntityCert::try_from(leaf)
        .map_err(|error| format!("invalid TLS certificate: {error}"))?
        .verify_is_valid_for_subject_name(&server_name)
        .map_err(|error| format!("TLS certificate does not cover {name}: {error}"))
}

fn material(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<TlsMaterial, String> {
    let digest = Sha256::digest(&chain[0]);
    let mut fingerprint = String::with_capacity(95);
    for (index, byte) in digest.iter().enumerate() {
        if index != 0 {
            fingerprint.push(':');
        }
        write!(fingerprint, "{byte:02X}").map_err(|error| error.to_string())?;
    }
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| format!("TLS protocol configuration: {error}"))?
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .map_err(|error| format!("TLS certificate/key configuration: {error}"))?;
    Ok(TlsMaterial {
        config: Arc::new(config),
        fingerprint,
    })
}

/// Replace the destination with a fresh private file, never briefly exposing key bytes.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension(format!("{}.tmp", super::rand::random_b64(12)?));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|error| format!("write {}: {error}", path.display()))
}

#[cfg(test)]
#[path = "tls_tests.rs"]
mod tests;
