//! The view's threads: the connection's event handler (reader thread), the
//! frame emitter (`frames.rs`) and the input sender; the worker is in
//! `worker.rs`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread;

use serde_json::Value;

use super::super::annotate;
use super::super::cdp::{EventHandler, DISCONNECTED};
use super::super::lock;
use super::core::Core;
use super::emit::Frame;
use super::frames::chromium_ack;
use super::worker::run_worker;

/// What the worker thread is told.
pub(super) enum Event {
    Cdp {
        session: Option<String>,
        method: String,
        params: Value,
    },
    /// The agent worked in a page: take it from [`FollowSlot`]. At most one
    /// of these is queued at a time.
    Follow,
    /// Answered once everything queued before it has been handled.
    Barrier(Sender<()>),
    /// A frame had the wrong size: restart the screencast.
    Restart,
    Stop,
}

/// A gesture queued for the page that was shown when it was made: it is
/// dropped, not redirected, when another page is shown by the time it is
/// sent. `release` marks a mouse button release.
pub(super) struct InputItem {
    pub session: String,
    pub generation: u64,
    pub method: &'static str,
    pub params: Value,
    pub release: bool,
}

/// Events the worker acts on; the handler drops all others.
const FORWARDED: [&str; 8] = [
    "Target.targetCreated",
    "Target.targetInfoChanged",
    "Target.targetDestroyed",
    "Page.frameNavigated",
    "Page.frameStartedLoading",
    "Page.frameStoppedLoading",
    "Page.loadEventFired",
    "Page.navigatedWithinDocument",
];

/// The page the agent worked in last, waiting for the worker. Reports
/// overwrite each other: a client streaming intents costs one slot and one
/// queued wake-up, and the worker switches once, to the newest page, not
/// through every stale one.
#[derive(Default)]
pub(super) struct FollowSlot {
    target: Mutex<Option<String>>,
    /// An `Event::Follow` is queued and the worker has not taken the slot
    /// since.
    woken: AtomicBool,
}

impl FollowSlot {
    /// Record `target` as the newest; `true` when the worker must be woken
    /// (no wake-up is queued yet).
    pub fn put(&self, target: &str) -> bool {
        *lock(&self.target) = Some(target.to_owned());
        !self.woken.swap(true, Ordering::SeqCst)
    }

    /// The newest page, if any. Clears the wake-up mark first: a report that
    /// lands after the clear queues a new wake-up, one before it is read
    /// here.
    pub fn take(&self) -> Option<String> {
        self.woken.store(false, Ordering::SeqCst);
        lock(&self.target).take()
    }
}

/// The handler for the connection's reader thread. Frames are handed to
/// the emitter right here; the rest goes to the worker.
pub(super) fn handler(core: Weak<Core>, worker: Sender<Event>) -> EventHandler {
    Box::new(move |session, method, params| {
        let Some(core) = core.upgrade() else {
            return;
        };
        if method == "Page.screencastFrame" {
            if on_frame(&core, session, params) {
                let _ = worker.send(Event::Restart);
            }
        } else if annotate::EVENTS.contains(&method) {
            core.on_overlay_event(session, method, params);
        } else if method == DISCONNECTED || FORWARDED.contains(&method) {
            let _ = worker.send(Event::Cdp {
                session: session.map(str::to_owned),
                method: method.to_owned(),
                params: params.clone(), // the handler only borrows it
            });
        }
    })
}

/// Queue a frame of the shown page for the emitter; its ack to Chromium
/// waits for the webview (see `frames.rs`). `true` when the frame is of the
/// wrong size and the screencast should restart.
fn on_frame(core: &Core, session: Option<&str>, params: &Value) -> bool {
    let Some(session) = session else {
        return false;
    };
    let ack = chromium_ack(session, params);
    let Some(data) = params.get("data").and_then(Value::as_str) else {
        core.ack_chromium(ack); // not queued: acknowledge now, or Chromium stops sending
        return false;
    };
    let mut inner = lock(&core.inner);
    let Some(generation) = inner
        .active
        .as_ref()
        .filter(|a| a.session == session)
        .map(|a| a.generation)
    else {
        drop(inner);
        core.ack_chromium(ack);
        return false; // a page we no longer show
    };
    // The metadata's device size is the CSS size of what the image shows.
    let metadata = &params["metadata"];
    let css = |key: &str, fallback: u32| {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let css = metadata[key]
            .as_f64()
            .map_or(fallback, |v| v.round() as u32); // a screen size in DIP
        css
    };
    let css_w = css("deviceWidth", inner.size.w);
    let css_h = css("deviceHeight", inner.size.h);
    // The event is borrowed; the base64 stays as Chromium sent it.
    let frame = Frame::new(data.to_owned(), css_w, css_h);
    let proportioned = frame.has_css_proportions();
    let restart = inner.frame_needs_restart(css_w, css_h, proportioned);
    drop(inner);
    // An image that is not what its metadata says would map clicks wrong.
    if proportioned {
        core.frames.put(generation, frame, ack);
    } else {
        core.ack_chromium(ack);
    }
    restart
}

/// Start the worker, frame emitter and input sender threads.
pub(super) fn spawn(
    core: &Arc<Core>,
    events: Receiver<Event>,
    input: Receiver<InputItem>,
) -> Result<(), String> {
    let spawn = |name: &str, run: Box<dyn FnOnce() + Send>| {
        thread::Builder::new()
            .name(name.into())
            .spawn(run)
            .map(drop)
            .map_err(|e| format!("{name} thread: {e}"))
    };
    let worker = Arc::clone(core);
    spawn(
        "browser-events",
        Box::new(move || run_worker(&worker, &events)),
    )?;
    let emitter = Arc::clone(core);
    spawn("browser-frames", Box::new(move || emitter.run_emitter()))?;
    let sender = Arc::clone(core);
    spawn(
        "browser-input",
        Box::new(move || run_input(&sender, &input)),
    )
}

fn run_input(core: &Core, queue: &Receiver<InputItem>) {
    let mut failing = false;
    for item in queue {
        if core.is_closed() {
            break;
        }
        #[cfg(test)]
        drop(lock(&core.input_gate)); // a test holds it to keep the queue from draining
        match core.send_input(item) {
            Ok(()) => failing = false,
            Err(e) if !failing => {
                failing = true; // one line per streak, not per mouse move
                eprintln!("[omp-desktop] browser input: {e}");
            }
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_reports_overwrite_each_other_and_wake_once() {
        let slot = FollowSlot::default();
        assert!(slot.put("a"), "the first report wakes the worker");
        for n in 0..1000 {
            assert!(!slot.put(&format!("t{n}")), "a wake-up is already queued");
        }
        assert_eq!(slot.take().as_deref(), Some("t999"), "only the newest");
        assert_eq!(slot.take(), None);
        assert!(slot.put("b"), "after the worker took it, wake again");
    }
}
