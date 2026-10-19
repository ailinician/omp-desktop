//! Helpers the test suites of several modules share.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Poll `check` every 10 ms until it returns `Some`, for at most `timeout`.
pub fn eventually<T>(timeout: Duration, mut check: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(found) = check() {
            return Some(found);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Poll `condition` until it holds (`true`) or `timeout` passes (`false`).
pub fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    eventually(timeout, || condition().then_some(())).is_some()
}

/// `script` run by the platform shell: `cmd /C` on Windows, `sh -c` on unix.
pub fn shell(script: &str) -> std::process::Command {
    let (program, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let mut cmd = std::process::Command::new(program);
    cmd.args([flag, script]);
    cmd
}

/// An executable stand-in at `path` holding `content` (unix: mode 0755),
/// its folder created first.
pub fn executable(path: &Path, content: &str) -> PathBuf {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(path, content).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path.to_path_buf()
}

/// A fresh, empty scratch directory `omp-<scope>-<name>-<pid>` in the temp
/// dir. `scope` keeps the suites apart: their tests share one process.
pub fn tempdir(scope: &str, name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omp-{scope}-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A [`tempdir`] removed with its content when dropped (also when the test
/// fails).
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(scope: &str, name: &str) -> Self {
        Self(tempdir(scope, name))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A TLS client that trusts only the certificate in `cert_pem`, the one the
/// remote server generated for the test.
pub fn tls_client_config(cert_pem: &Path) -> Arc<rustls::ClientConfig> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    let pem = std::fs::read(cert_pem).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(&pem).unwrap())
        .unwrap();
    Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth(),
    )
}
