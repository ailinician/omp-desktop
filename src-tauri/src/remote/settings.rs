//! Remote access settings, persisted by Rust in `remote.json` (mode 0600).

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::tls::{write_private, CertSource};
use crate::json_store;

pub const DEFAULT_PORT: u16 = 7470;

/// Where the TLS certificate comes from.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CertSetting {
    #[default]
    Generated,
    Files {
        cert: String,
        key: String,
    },
}

impl CertSetting {
    pub fn source(&self) -> CertSource {
        match self {
            Self::Generated => CertSource::Generated,
            Self::Files { cert, key } => CertSource::Files {
                cert: cert.into(),
                key: key.into(),
            },
        }
    }
}

/// Web Push triggers; the behaviour lives in the push module.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct PushSettings {
    pub enabled: bool,
    pub questions: bool,
    pub finished: bool,
    pub contact: String,
}

impl Default for PushSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            questions: true,
            finished: true,
            contact: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct RemoteSettings {
    pub enabled: bool,
    pub interface: Option<String>,
    pub port: u16,
    pub host_name: Option<String>,
    pub cert: CertSetting,
    pub allow_http: bool,
    pub push: PushSettings,
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            interface: None,
            port: DEFAULT_PORT,
            host_name: None,
            cert: CertSetting::default(),
            allow_http: false,
            push: PushSettings::default(),
        }
    }
}

impl RemoteSettings {
    /// A blank host name or interface means "not set".
    pub fn normalized(mut self) -> Self {
        let trimmed = |value: Option<String>| {
            value
                .map(|text| text.trim().to_owned())
                .filter(|text| !text.is_empty())
        };
        self.interface = trimmed(self.interface);
        self.host_name = trimmed(self.host_name);
        self
    }

    /// Unreadable or missing file: everything off.
    pub fn load(path: &Path) -> Self {
        json_store::read_or_default::<Self>(path).normalized()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        write_private(path, &bytes)
    }
}
