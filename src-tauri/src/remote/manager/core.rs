//! Internal state, server start/rebind lifecycle, status assembly and
//! interface-monitoring loop for [`super::RemoteManager`].

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use super::IfaceSource;
use crate::remote::devices::DeviceStore;
use crate::remote::emit::{Action, DeviceStatus, RemoteEmit, Status};
use crate::remote::http::REQUEST_DEADLINE;
use crate::remote::hub::{Hub, CLOSE_NOT_READY};
use crate::remote::net;
use crate::remote::pairing::Pairing;
use crate::remote::push::Pusher;
use crate::remote::server::{Listen, Server, Shared};
use crate::remote::settings::{CertSetting, RemoteSettings};
use crate::remote::{tls, ws};

pub(super) struct Running {
    _server: Server,
    pub ip: Ipv4Addr,
    port: u16,
    host: Option<String>,
    plain: bool,
    pub fingerprint: Option<String>,
}

impl Running {
    pub fn base_url(&self) -> String {
        let scheme = if self.plain { "http" } else { "https" };
        let host = self.host.clone().unwrap_or_else(|| self.ip.to_string()); // owned for format
        format!("{scheme}://{host}:{}", self.port)
    }
}

/// Dropping the sender wakes the monitor thread, which then ends.
pub(super) struct Monitor {
    stop: Arc<AtomicBool>,
    _wake: Sender<()>,
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

pub(super) struct State {
    pub settings: RemoteSettings,
    pub run: Option<Running>,
    pub error: Option<String>,
    pub monitor: Option<Monitor>,
}

/// Forwards the hub's events and turns every connection change into a status.
pub(super) struct HubEmit {
    pub core: Weak<Core>,
    pub emit: Arc<dyn RemoteEmit>,
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

pub(super) struct Core {
    pub dir: PathBuf,
    pub emit: Arc<dyn RemoteEmit>,
    pub source: IfaceSource,
    pub interval: Duration,
    pub devices: Arc<DeviceStore>,
    pub pairing: Arc<Pairing>,
    pub hub: Arc<Hub>,
    pub pusher: Arc<Pusher>,
    /// Serialises apply / rebind / stop; `state` is only ever held briefly.
    pub serial: Mutex<()>,
    /// Orders status emits: each takes the latest snapshot under this lock,
    /// so an older status never reaches the webview after a newer one.
    pub emit_lock: Mutex<()>,
    pub me: Weak<Self>,
    pub state: Mutex<State>,
}

impl Core {
    pub fn serial(&self) -> MutexGuard<'_, ()> {
        self.serial.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Emit the latest status. The snapshot is taken under `emit_lock`, so
    /// concurrent emits reach the webview in snapshot order.
    pub fn emit_status(&self) -> Status {
        let _order = self
            .emit_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let status = self.status_with(&self.hub.connected_devices());
        self.emit.status(&status);
        status
    }

    pub fn emit_status_fn(core: &Weak<Self>) -> Arc<dyn Fn() + Send + Sync> {
        let core = Weak::clone(core);
        Arc::new(move || {
            if let Some(core) = core.upgrade() {
                core.emit_status();
            }
        })
    }

    pub fn status_with(&self, connected: &[String]) -> Status {
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
    pub fn rebind(&self, settings: &RemoteSettings) {
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

    fn snapshot(&self) -> (Option<Ipv4Addr>, Option<String>, Option<String>) {
        let state = self.state();
        (
            state.run.as_ref().map(|run| run.ip),
            state.run.as_ref().and_then(|run| run.fingerprint.clone()),
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
        let cert_fresh = settings.allow_http
            || !matches!(settings.cert, CertSetting::Generated)
            || wanted.is_some_and(|ip| {
                tls::generated_cert_valid(&self.dir, ip, settings.host_name.as_deref())
            });
        if running.is_some() && running == wanted && cert_fresh {
            return;
        }
        let before = self.snapshot();
        self.rebind(&settings);
        if self.snapshot() != before {
            self.emit_status();
        }
    }

    pub fn spawn_monitor(self: &Arc<Self>) -> Option<Monitor> {
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
