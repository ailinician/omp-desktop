//! Owns the remote server: starts, stops and rebinds it from the settings,
//! watches the chosen interface, and builds the `Status` the webview shows.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde::Serialize;

mod core;
use core::{Core, HubEmit, Running, State};

use super::bridge::FanOut;
use super::devices::{unix_seconds, DeviceStore};
use super::emit::{RemoteEmit, Status};
use super::hub::{Hub, CLOSE_NOT_READY};
use super::net::{self, Iface};
use super::pairing::{Pairing, CODE_TTL};
use super::push::Pusher;
use super::settings::RemoteSettings;
use super::tracker::TrackerSink;
use super::{qr, tls};

pub const SETTINGS_FILE: &str = "remote.json";
const DEVICES_FILE: &str = "remote-devices.json";
const MONITOR_INTERVAL: Duration = Duration::from_secs(10);

/// Where the manager learns the host's interfaces; replaced in tests.
pub type IfaceSource = Arc<dyn Fn() -> Result<Vec<Iface>, String> + Send + Sync>;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairStart {
    pub url: String,
    pub svg: String,
    /// Unix seconds, like the device timestamps.
    pub expires_at: u64,
}

pub struct RemoteManager {
    core: Arc<Core>,
    fan_out: Option<Arc<FanOut>>,
}

/// Whether the running server already matches `new` (what is not a server
/// property, like the push options, is ignored).
fn same_server(old: &RemoteSettings, new: &RemoteSettings) -> bool {
    old.enabled == new.enabled
        && old.interface == new.interface
        && old.port == new.port
        && old.host_name == new.host_name
        && old.cert == new.cert
        && old.allow_http == new.allow_http
}

impl RemoteManager {
    pub fn new(config_dir: PathBuf, emit: Arc<dyn RemoteEmit>) -> Self {
        Self::with_source(
            config_dir,
            emit,
            Arc::new(net::interfaces),
            MONITOR_INTERVAL,
        )
    }

    pub fn with_source(
        dir: PathBuf,
        emit: Arc<dyn RemoteEmit>,
        source: IfaceSource,
        interval: Duration,
    ) -> Self {
        let devices = Arc::new(DeviceStore::load(dir.join(DEVICES_FILE)));
        let settings = RemoteSettings::load(&dir.join(SETTINGS_FILE));
        let core = Arc::new_cyclic(|core: &Weak<Core>| {
            let hub_emit = HubEmit {
                core: Weak::clone(core),
                emit: Arc::clone(&emit),
            };
            let hub = Arc::new(Hub::new(Arc::clone(&devices), Arc::new(hub_emit)));
            let pusher = Pusher::new(
                dir.clone(),
                Arc::clone(&devices),
                {
                    let hub = Arc::clone(&hub);
                    Arc::new(move |tab| hub.tab_title(tab))
                },
                Core::emit_status_fn(core),
            );
            Core {
                hub,
                pusher: Arc::new(pusher),
                dir,
                emit,
                source,
                interval,
                devices,
                pairing: Arc::new(Pairing::default()),
                serial: Mutex::new(()),
                emit_lock: Mutex::new(()),
                me: Weak::clone(core),
                state: Mutex::new(State {
                    settings,
                    run: None,
                    error: None,
                    monitor: None,
                }),
            }
        });
        let fan_out = match FanOut::new(Arc::clone(&core.hub)) {
            Ok(fan_out) => {
                fan_out.set_push(Some(Arc::clone(&core.pusher) as Arc<dyn TrackerSink>));
                Some(Arc::new(fan_out))
            }
            Err(error) => {
                eprintln!("[omp-desktop] remote tab updates are unavailable: {error}");
                None
            }
        };
        Self { core, fan_out }
    }

    pub fn hub(&self) -> Arc<Hub> {
        Arc::clone(&self.core.hub)
    }

    /// Enqueue-only tracker sink, stopped together with this manager.
    pub fn tracker_sink(&self) -> Option<Arc<FanOut>> {
        self.fan_out.as_ref().map(Arc::clone)
    }

    /// Send a test notification to every subscribed phone.
    pub fn push_test(&self) -> Result<(), String> {
        self.core.pusher.test()
    }

    /// Hand the push settings to the pusher and the hub (which tells phones
    /// the key to subscribe with).
    fn configure_push(&self, push: &super::settings::PushSettings) {
        let key = self.core.pusher.configure(push);
        self.core.hub.set_push_key(key);
    }

    #[cfg(test)]
    pub fn settings(&self) -> RemoteSettings {
        self.core.state().settings.clone() // owned snapshot
    }

    pub fn status(&self) -> Status {
        self.core.status_with(&self.core.hub.connected_devices())
    }

    /// Start with the persisted settings (the app is starting up).
    pub fn start_saved(&self) -> Status {
        let core = &self.core;
        {
            let _serial = core.serial();
            let settings = core.state().settings.clone(); // owned snapshot under serial
            if settings.enabled {
                let old = core.state().monitor.take();
                drop(old);
                core.rebind(&settings);
                core.state().monitor = core.spawn_monitor();
            }
            self.configure_push(&settings.push);
        }
        self.publish_status()
    }

    /// Persist `settings` and make the server match them.
    pub fn apply(&self, settings: RemoteSettings) -> Result<Status, String> {
        let settings = settings.normalized();
        let core = &self.core;
        {
            let _serial = core.serial();
            settings.save(&core.dir.join(SETTINGS_FILE))?;
            let (keep, old) = {
                let mut state = core.state();
                let cert_fresh = settings.allow_http
                    || !matches!(settings.cert, super::settings::CertSetting::Generated)
                    || state.run.as_ref().is_some_and(|run| {
                        tls::generated_cert_valid(&core.dir, run.ip, settings.host_name.as_deref())
                    });
                let keep =
                    state.run.is_some() && same_server(&state.settings, &settings) && cert_fresh;
                state.settings = settings.clone(); // the state keeps its own copy
                (keep, if keep { None } else { state.monitor.take() })
            };
            // Only e.g. the push options changed: sockets stay connected.
            if !keep {
                drop(old);
                core.rebind(&settings);
                if settings.enabled {
                    let monitor = core.spawn_monitor();
                    core.state().monitor = monitor;
                }
            }
            self.configure_push(&settings.push);
        }
        Ok(self.publish_status())
    }

    pub fn stop(&self) {
        if let Some(fan_out) = &self.fan_out {
            fan_out.stop();
        }
        let core = &self.core;
        let _serial = core.serial();
        let old = core.state().monitor.take();
        let run = core.state().run.take();
        drop((old, run));
        // Also without a server: connections still in flight on a dropped one
        // must not be admitted.
        core.hub.close_all(CLOSE_NOT_READY);
    }

    fn publish_status(&self) -> Status {
        self.core.emit_status()
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
            if !matches!(settings.cert, super::settings::CertSetting::Generated) {
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

impl Drop for RemoteManager {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod tests;
