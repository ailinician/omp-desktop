//! Owns the remote server: starts, stops and rebinds it from the settings,
//! watches the chosen interface, and builds the `Status` the webview shows.

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::bridge::FanOut;
use super::devices::{unix_seconds, DeviceStore};
use super::emit::{Action, DeviceStatus, RemoteEmit, Status};
use super::hub::{Hub, CLOSE_NOT_READY};
use super::net::{self, Iface};
use super::pairing::{Pairing, CODE_TTL};
use super::push::Pusher;
use super::server::{Listen, Server, Shared};
use super::settings::RemoteSettings;
use super::tracker::TrackerSink;
use super::{http::REQUEST_DEADLINE, qr, tls, ws};

pub const SETTINGS_FILE: &str = "remote.json";
const DEVICES_FILE: &str = "remote-devices.json";
const MONITOR_INTERVAL: Duration = Duration::from_secs(10);

/// Where the manager learns the host's interfaces; replaced in tests.
pub type IfaceSource = Arc<dyn Fn() -> Result<Vec<Iface>, String> + Send + Sync>;

struct Running {
    _server: Server,
    ip: Ipv4Addr,
    port: u16,
    host: Option<String>,
    plain: bool,
    fingerprint: Option<String>,
}

impl Running {
    fn base_url(&self) -> String {
        let scheme = if self.plain { "http" } else { "https" };
        let host = self.host.clone().unwrap_or_else(|| self.ip.to_string()); // owned for format
        format!("{scheme}://{host}:{}", self.port)
    }
}

/// Dropping the sender wakes the monitor thread, which then ends.
struct Monitor {
    stop: Arc<AtomicBool>,
    _wake: Sender<()>,
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

struct State {
    settings: RemoteSettings,
    run: Option<Running>,
    error: Option<String>,
    monitor: Option<Monitor>,
}

/// Forwards the hub's events and turns every connection change into a status.
struct HubEmit {
    core: Weak<Core>,
    emit: Arc<dyn RemoteEmit>,
}

impl RemoteEmit for HubEmit {
    fn watch(&self, tab_ids: &[String]) {
        self.emit.watch(tab_ids);
    }

    fn connections(&self, _device_ids: &[String]) {
        if let Some(core) = self.core.upgrade() {
            core.emit_status();
        }
    }

    fn action(&self, action: &Action) {
        self.emit.action(action);
    }

    fn status(&self, status: &Status) {
        self.emit.status(status);
    }
}

struct Core {
    dir: PathBuf,
    emit: Arc<dyn RemoteEmit>,
    source: IfaceSource,
    interval: Duration,
    devices: Arc<DeviceStore>,
    pairing: Arc<Pairing>,
    hub: Arc<Hub>,
    pusher: Arc<Pusher>,
    /// Serialises apply / rebind / stop; `state` is only ever held briefly.
    serial: Mutex<()>,
    /// Orders status emits: each takes the latest snapshot under this lock,
    /// so an older status never reaches the webview after a newer one.
    emit_lock: Mutex<()>,
    me: Weak<Self>,
    state: Mutex<State>,
}

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

impl Core {
    fn serial(&self) -> MutexGuard<'_, ()> {
        self.serial.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Emit the latest status. The snapshot is taken under `emit_lock`, so
    /// concurrent emits reach the webview in snapshot order.
    fn emit_status(&self) -> Status {
        let _order = self
            .emit_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let status = self.status_with(&self.hub.connected_devices());
        self.emit.status(&status);
        status
    }

    fn emit_status_fn(core: &Weak<Self>) -> Arc<dyn Fn() + Send + Sync> {
        let core = Weak::clone(core);
        Arc::new(move || {
            if let Some(core) = core.upgrade() {
                core.emit_status();
            }
        })
    }

    fn status_with(&self, connected: &[String]) -> Status {
        let devices = self
            .devices
            .list()
            .into_iter()
            .map(|device| DeviceStatus {
                connected: connected.contains(&device.id),
                push_subscribed: !device.push.is_empty(),
                id: device.id,
                name: device.name,
                paired_at: device.paired_at,
                last_seen: device.last_seen,
            })
            .collect();
        let state = self.state();
        let run = state.run.as_ref();
        Status {
            enabled: state.settings.enabled,
            listening: run.is_some(),
            url: run.map(Running::base_url),
            ip: run.map(|run| run.ip.to_string()),
            error: state.error.clone(), // owned snapshot
            fingerprint: run.and_then(|run| run.fingerprint.clone()), // owned snapshot
            devices,
            plain_http: state.settings.allow_http,
            push_error: self.pusher.last_error(),
            settings: state.settings.clone(), // owned snapshot
        }
    }

    fn start(&self, settings: &RemoteSettings) -> Result<Option<Running>, String> {
        // After the `close_all` of the rebind/stop that precedes every start.
        let generation = self.hub.generation();
        if !settings.enabled {
            return Ok(None);
        }
        let name = settings
            .interface
            .as_deref()
            .ok_or("no network interface selected")?;
        let ip = net::ipv4_of(name, &(self.source)()?)
            .ok_or_else(|| format!("interface {name} has no IPv4 address"))?;
        let host = settings.host_name.as_deref();
        let (tls, fingerprint) = if settings.allow_http {
            (None, None)
        } else {
            let material = tls::load_or_generate(&self.dir, &settings.cert.source(), ip, host)?;
            (Some(material.config), Some(material.fingerprint))
        };
        let server = Server::start(
            Listen {
                ip,
                port: settings.port,
                host_name: host.map(str::to_owned),
                tls,
                request_deadline: REQUEST_DEADLINE,
            },
            Shared {
                pairing: Arc::clone(&self.pairing),
                devices: Arc::clone(&self.devices),
                is_current: {
                    let hub = Arc::clone(&self.hub);
                    Arc::new(move || hub.generation() == generation)
                },
                on_paired: Self::emit_status_fn(&self.me),
            },
            ws::upgrade_fn(Arc::clone(&self.hub), Arc::clone(&self.devices), generation),
        )?;
        Ok(Some(Running {
            port: server.local_addr().port(),
            _server: server,
            ip,
            host: host.map(str::to_owned),
            plain: settings.allow_http,
            fingerprint,
        }))
    }

    /// Stop the server (and its phone sockets), then start it as `settings`
    /// say. A failure is recorded as the status error, never a fallback bind.
    fn rebind(&self, settings: &RemoteSettings) {
        let old = self.state().run.take();
        drop(old); // joins the accept thread
                   // Also without a server: connections still in flight on a dropped one
                   // must not be admitted.
        self.hub.close_all(CLOSE_NOT_READY);
        let (run, error) = match self.start(settings) {
            Ok(run) => (run, None),
            Err(error) => (None, Some(error)),
        };
        let mut state = self.state();
        state.run = run;
        state.error = error;
    }

    fn snapshot(&self) -> (Option<Ipv4Addr>, Option<String>) {
        let state = self.state();
        (
            state.run.as_ref().map(|run| run.ip),
            state.error.clone(), // owned snapshot
        )
    }

    /// One monitor tick: follow the interface's address, never another one.
    fn reconcile(&self, stop: &AtomicBool) {
        let _serial = self.serial();
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let (settings, running) = {
            let state = self.state();
            (
                state.settings.clone(), // owned for the lookup
                state.run.as_ref().map(|run| run.ip),
            )
        };
        let listed = (self.source)();
        if listed.is_err() && running.is_some() {
            return;
        }
        let wanted = settings
            .interface
            .as_deref()
            .zip(listed.ok())
            .and_then(|(name, list)| net::ipv4_of(name, &list));
        if running.is_some() && running == wanted {
            return;
        }
        let before = self.snapshot();
        self.rebind(&settings);
        if self.snapshot() != before {
            self.emit_status();
        }
    }

    fn spawn_monitor(self: &Arc<Self>) -> Option<Monitor> {
        let stop = Arc::new(AtomicBool::new(false));
        let (wake, asleep) = mpsc::channel::<()>();
        let core = Arc::downgrade(self);
        let flag = Arc::clone(&stop);
        let interval = self.interval;
        let spawned = std::thread::Builder::new()
            .name("remote-monitor".to_owned())
            .spawn(move || {
                while asleep.recv_timeout(interval) == Err(RecvTimeoutError::Timeout) {
                    let Some(core) = core.upgrade() else { break };
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    core.reconcile(&flag);
                }
            });
        match spawned {
            Ok(_) => Some(Monitor { stop, _wake: wake }),
            Err(error) => {
                eprintln!("[omp-desktop] could not start the remote interface monitor: {error}");
                None
            }
        }
    }
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
    fn configure_push(&self) {
        let push = self.settings().push;
        let key = self.core.pusher.configure(&push);
        self.core.hub.set_push_key(key);
    }

    pub fn settings(&self) -> RemoteSettings {
        self.core.state().settings.clone() // owned snapshot
    }

    pub fn status(&self) -> Status {
        self.core.status_with(&self.core.hub.connected_devices())
    }

    /// Start with the persisted settings (the app is starting up).
    pub fn start_saved(&self) -> Status {
        let settings = self.settings();
        if settings.enabled {
            self.apply(settings).unwrap_or_else(|_| self.status())
        } else {
            self.configure_push();
            self.status()
        }
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
                let keep = state.run.is_some() && same_server(&state.settings, &settings);
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
        }
        self.configure_push();
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
