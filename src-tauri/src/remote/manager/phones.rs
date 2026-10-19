//! Phone pairing, device management and certificate regeneration for
//! [`super::RemoteManager`].

use std::time::Instant;

use super::core::Running;
use super::{PairStart, RemoteManager};
use crate::remote::devices::unix_seconds;
use crate::remote::emit::Status;
use crate::remote::pairing::CODE_TTL;
use crate::remote::qr;
use crate::remote::settings::CertSetting;

impl RemoteManager {
    /// Send a test notification to every subscribed phone.
    pub fn push_test(&self) -> Result<(), String> {
        self.core.pusher.test()
    }

    pub fn pair_start(&self) -> Result<PairStart, String> {
        let base = self
            .core
            .state()
            .run
            .as_ref()
            .map(Running::base_url)
            .ok_or("remote access is not listening")?;
        let code = self.core.pairing.start(Instant::now())?;
        let url = format!("{base}/#pair={code}");
        Ok(PairStart {
            svg: qr::svg(&url)?,
            url,
            expires_at: unix_seconds() + CODE_TTL.as_secs(),
        })
    }

    pub fn pair_cancel(&self) {
        self.core.pairing.cancel();
    }

    pub fn device_rename(&self, id: &str, name: &str) -> Result<Status, String> {
        self.core.devices.rename(id, name)?;
        Ok(self.publish_status())
    }

    pub fn device_revoke(&self, id: &str) -> Result<Status, String> {
        if !self.core.hub.revoke(id) {
            return Err("remote device not found".to_owned());
        }
        Ok(self.publish_status())
    }

    /// Delete the generated certificate and serve with a fresh one.
    pub fn regenerate_cert(&self) -> Result<Status, String> {
        let core = &self.core;
        {
            let _serial = core.serial();
            let settings = core.state().settings.clone(); // owned for the rebind
            if !matches!(settings.cert, CertSetting::Generated) {
                return Err("an own certificate is in use; there is nothing to regenerate".into());
            }
            for file in ["cert.pem", "key.pem", "cert.json"] {
                match std::fs::remove_file(core.dir.join("remote").join(file)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(format!("remove {file}: {error}")),
                }
            }
            if settings.enabled {
                core.rebind(&settings);
            }
        }
        Ok(self.publish_status())
    }
}
