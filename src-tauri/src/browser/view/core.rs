//! The state and CDP plumbing a [`View`](super::View) and its threads
//! share: the model, CDP calls, state events, viewport and input. Which page
//! is shown is in `switching.rs`, frames in `frames.rs`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::super::annotate::Annotate;
use super::super::cdp::CdpClient;
use super::super::lock;
use super::emit::{BrowserEmit, Pick, ViewState};
use super::events::{FollowSlot, InputItem};
use super::frames::FrameFlow;
use super::model::{Inner, Size};

pub(in crate::browser) struct Core {
    pub(super) sid: String,
    pub(super) emit: Arc<dyn BrowserEmit>,
    pub(in crate::browser) inner: Mutex<Inner>,
    pub(super) frames: FrameFlow,
    pub(super) follow: FollowSlot,
    /// Serialises target switches and annotate toggles; never taken by the
    /// reader thread, whose notifications do not block on CDP responses.
    pub(in crate::browser) switch: Mutex<()>,
    /// Held across a frame's emit and an activation's `tabs` event (taken
    /// before `inner`), so frames stay ordered after the event that marks
    /// their page active.
    pub(super) emit_order: Mutex<()>,
    pub(in crate::browser) client: OnceLock<CdpClient>,
    closed: AtomicBool,
    /// When the panel last released a mouse button on the page.
    released: Mutex<Option<Instant>>,
    /// Annotate mode, see `annotate/`.
    pub(in crate::browser) annotate: Mutex<Annotate>,
    /// Serialises overlay installs (taken before `annotate`, never by the
    /// reader thread).
    pub(in crate::browser) overlay_install: Mutex<()>,
    /// Held by a test to keep the input sender from sending.
    #[cfg(test)]
    pub(super) input_gate: Mutex<()>,
    /// Held by a test to keep the worker from handling a follow.
    #[cfg(test)]
    pub(super) follow_gate: Mutex<()>,
}

impl Core {
    pub(in crate::browser) fn new(sid: &str, emit: Arc<dyn BrowserEmit>) -> Self {
        Self {
            sid: sid.to_owned(),
            emit,
            inner: Mutex::new(Inner::new()),
            frames: FrameFlow::default(),
            follow: FollowSlot::default(),
            switch: Mutex::new(()),
            emit_order: Mutex::new(()),
            client: OnceLock::new(),
            closed: AtomicBool::new(false),
            released: Mutex::new(None),
            annotate: Mutex::new(Annotate::default()),
            overlay_install: Mutex::new(()),
            #[cfg(test)]
            input_gate: Mutex::new(()),
            #[cfg(test)]
            follow_gate: Mutex::new(()),
        }
    }

    pub(in crate::browser) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Mark the view closed; `false` when it already was.
    pub(in crate::browser) fn mark_closed(&self) -> bool {
        let mut annotate = lock(&self.annotate);
        let was_open = !self.closed.swap(true, Ordering::SeqCst);
        annotate.on = false;
        was_open
    }

    pub(in crate::browser) fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        self.client
            .get()
            .ok_or("cdp not connected")?
            .call(session, method, params)
    }

    /// Send a command whose reply nobody reads; CDP keeps each session's
    /// commands in order, so a later `call` still sees its effect.
    pub(in crate::browser) fn notify(&self, session: Option<&str>, method: &str, params: Value) {
        if let Some(client) = self.client.get() {
            client.notify(session, method, params);
        }
    }

    pub(in crate::browser) fn active_session(&self) -> Result<String, String> {
        self.activation().map(|(session, _)| session)
    }

    pub(in crate::browser) fn call_active(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let session = self.active_session()?;
        self.call(Some(&session), method, params)
    }

    /// A new blank page, listed at once.
    pub(super) fn create_page(&self) -> Result<String, String> {
        let created = self.call(None, "Target.createTarget", json!({ "url": "about:blank" }))?;
        let target = created["targetId"]
            .as_str()
            .ok_or("createTarget returned no target")?
            .to_owned();
        lock(&self.inner).upsert_page(&json!({ "targetId": target, "url": "about:blank" }));
        Ok(target)
    }

    /// Emit `tabs` when the list differs from the last one. Emitting under
    /// the lock keeps events in the order of the changes.
    pub(super) fn publish_tabs(&self, inner: &mut Inner) {
        if let Some(tabs) = inner.tabs_if_changed() {
            self.emit.tabs(&self.sid, tabs);
        }
    }

    /// Tell the panel about a pick, unless the view is closed.
    pub(in crate::browser) fn emit_pick(&self, pick: &Pick) {
        if !self.is_closed() {
            self.emit.pick(&self.sid, pick);
        }
    }

    pub(in crate::browser) fn update_state(&self, change: impl FnOnce(&mut ViewState)) {
        let mut inner = lock(&self.inner);
        let mut next = inner.state.clone(); // compared with the previous state below
        change(&mut next);
        if next != inner.state {
            self.emit.state(&self.sid, &next);
            inner.state = next;
        }
    }

    /// `url` and back/forward availability from the navigation history.
    pub(super) fn refresh_history(&self) -> Result<(), String> {
        let history = self.call_active("Page.getNavigationHistory", json!({}))?;
        let entries = history["entries"].as_array().map_or(&[][..], Vec::as_slice);
        let current = history["currentIndex"]
            .as_u64()
            .and_then(|i| usize::try_from(i).ok())
            .unwrap_or(0);
        let url = entries
            .get(current)
            .and_then(|e| e["url"].as_str())
            .map(str::to_owned);
        self.update_state(|s| {
            s.can_back = current > 0;
            s.can_forward = current + 1 < entries.len();
            if let Some(url) = url {
                s.url = url;
            }
        });
        Ok(())
    }

    /// The shown page's title as Chromium has it now: target events carry
    /// the title of the moment a navigation commits, often still the URL.
    pub(super) fn refresh_title(&self) -> Result<(), String> {
        let active = lock(&self.inner)
            .active
            .as_ref()
            .map(|a| a.target_id.clone()); // owned: the lock must not span the CDP call
        let Some(target_id) = active else {
            return Ok(());
        };
        let info = self.call(
            None,
            "Target.getTargetInfo",
            json!({ "targetId": target_id }),
        )?;
        let mut inner = lock(&self.inner);
        if inner.is_listed(&target_id) {
            inner.upsert_page(&info["targetInfo"]);
            self.publish_tabs(&mut inner);
        }
        drop(inner);
        Ok(())
    }

    /// Make the panel `size`, in effect for new frames of `session`.
    pub(super) fn resize_to(&self, session: &str, size: Size) -> Result<(), String> {
        let _switching = lock(&self.switch);
        self.apply_viewport(session, size)
    }

    /// Size `session`'s viewport to `size` and (re)start its screencast.
    pub(super) fn apply_viewport(&self, session: &str, size: Size) -> Result<(), String> {
        self.notify(Some(session), "Page.stopScreencast", json!({}));
        self.call(
            Some(session),
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": size.w, "height": size.h, "deviceScaleFactor": size.dpr, "mobile": false }),
        )?;
        // The page shows as focused (carets, `:focus`) though headless
        // Chromium has no window focus; not every build supports it.
        self.notify(
            Some(session),
            "Emulation.setFocusEmulationEnabled",
            json!({ "enabled": true }),
        );
        self.start_screencast(session, size)
    }

    /// Restart `session`'s screencast without touching its viewport: the
    /// start sends a fresh frame.
    pub(super) fn restart_screencast(&self, session: &str, size: Size) -> Result<(), String> {
        self.notify(Some(session), "Page.stopScreencast", json!({}));
        self.start_screencast(session, size)
    }

    fn start_screencast(&self, session: &str, size: Size) -> Result<(), String> {
        self.frames.clear();
        self.call(
            Some(session),
            "Page.startScreencast",
            json!({
                "format": "jpeg", "quality": 70,
                "maxWidth": size.px(size.w), "maxHeight": size.px(size.h),
                "everyNthFrame": 1,
            }),
        )?;
        Ok(())
    }

    /// The page session the next input event is made for, with its
    /// activation.
    pub(super) fn activation(&self) -> Result<(String, u64), String> {
        lock(&self.inner)
            .active
            .as_ref()
            .map(|a| (a.session.clone(), a.generation)) // owned: used after the lock
            .ok_or_else(|| "no page open".to_owned())
    }

    /// Send a queued gesture to the page session it was made for. One of an
    /// earlier activation is dropped, not sent to the page shown now. Not
    /// waited for: CDP keeps the order, and a round trip per event would cap
    /// input at one per reply.
    pub(super) fn send_input(&self, item: InputItem) -> Result<(), String> {
        let current = lock(&self.inner)
            .active
            .as_ref()
            .is_some_and(|a| a.generation == item.generation && a.session == item.session);
        if !current {
            return Ok(());
        }
        if item.release {
            self.note_panel_release(); // before the page can react to it
        }
        self.client.get().ok_or("cdp not connected")?.post(
            Some(&item.session),
            item.method,
            item.params,
        )
    }

    /// Whether `frame_id` is the shown page's main frame (a page target's
    /// main frame has the target's id).
    pub(super) fn is_main_frame(&self, frame_id: Option<&str>) -> bool {
        let inner = lock(&self.inner);
        frame_id.is_some_and(|id| inner.active.as_ref().is_some_and(|a| a.target_id == id))
    }

    /// The panel released a mouse button on the page.
    pub(in crate::browser) fn note_panel_release(&self) {
        *lock(&self.released) = Some(Instant::now());
    }

    /// Whether the panel released a mouse button on the page in the last
    /// `within`. Cheap: the reader thread asks.
    pub(in crate::browser) fn panel_released_within(&self, within: Duration) -> bool {
        lock(&self.released).is_some_and(|at| at.elapsed() <= within)
    }
}

#[cfg(test)]
mod tests {
    use super::super::emit::{Frame, TabInfo};
    use super::*;

    /// Records the base64 of every frame emitted.
    #[derive(Default)]
    struct Frames(Mutex<Vec<String>>);

    impl BrowserEmit for Frames {
        fn frame(&self, _: &str, f: &Frame) {
            lock(&self.0).push(f.b64.clone()); // the log owns what it records
        }
        fn tabs(&self, _: &str, _: &[TabInfo]) {}
        fn state(&self, _: &str, _: &ViewState) {}
        fn pick(&self, _: &str, _: &Pick) {}
    }

    fn core() -> (Core, Arc<Frames>) {
        let seen = Arc::new(Frames::default());
        let emit: Arc<dyn BrowserEmit> = Arc::clone(&seen) as Arc<dyn BrowserEmit>;
        (Core::new("s1", emit), seen)
    }

    #[test]
    fn a_frame_of_an_earlier_activation_is_not_emitted() {
        let (core, seen) = core();
        let first = lock(&core.inner).set_active("a", "sa");
        assert!(core.emit_frame(first, &Frame::new("on-a".into(), 1, 1)));
        let second = lock(&core.inner).set_active("b", "sb");
        assert!(!core.emit_frame(first, &Frame::new("late-a".into(), 1, 1)));
        assert!(core.emit_frame(second, &Frame::new("on-b".into(), 1, 1)));
        assert_eq!(*lock(&seen.0), ["on-a", "on-b"]);
    }

    #[test]
    fn emitting_a_frame_does_not_hold_the_model() {
        /// Takes the model lock from another thread while emitting.
        struct Probe(OnceLock<std::sync::Weak<Core>>);
        impl BrowserEmit for Probe {
            fn frame(&self, _: &str, _: &Frame) {
                let core = self.0.get().and_then(std::sync::Weak::upgrade).unwrap();
                let free = std::thread::spawn(move || core.activation().is_ok());
                assert!(free.join().unwrap(), "input can be queued meanwhile");
            }
            fn tabs(&self, _: &str, _: &[TabInfo]) {}
            fn state(&self, _: &str, _: &ViewState) {}
            fn pick(&self, _: &str, _: &Pick) {}
        }
        let probe = Arc::new(Probe(OnceLock::new()));
        let core = Arc::new(Core::new("s1", Arc::clone(&probe) as Arc<dyn BrowserEmit>));
        probe.0.set(Arc::downgrade(&core)).unwrap();
        let generation = lock(&core.inner).set_active("a", "sa");
        assert!(core.emit_frame(generation, &Frame::new("f".into(), 1, 1)));
    }

    #[test]
    fn annotate_toggle_waits_for_target_switch_serialisation() {
        let (core, _) = core();
        let core = Arc::new(core);
        lock(&core.inner).set_active("a", "sa");
        let switching = lock(&core.switch);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let toggled = Arc::clone(&core);
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx.send(toggled.set_annotate(true)).unwrap();
        });
        started_rx.recv().unwrap();
        let early = done_rx.recv_timeout(std::time::Duration::from_millis(100));
        lock(&core.inner).set_active("b", "sb");
        drop(switching);
        worker.join().unwrap();
        assert!(
            matches!(early, Err(std::sync::mpsc::RecvTimeoutError::Timeout)),
            "the toggle must not finish while a target switch owns the lock: {early:?}"
        );
        assert_eq!(
            done_rx.recv().unwrap(),
            Err("annotate overlay: cdp not connected".to_owned()),
            "after the switch the toggle attempts the current page"
        );
    }

    fn item(session: &str, generation: u64, release: bool) -> InputItem {
        InputItem {
            session: session.to_owned(),
            generation,
            method: "Input.insertText",
            params: json!({ "text": "x" }),
            release,
        }
    }

    #[test]
    fn input_of_an_earlier_activation_is_dropped_not_sent() {
        let (core, _seen) = core();
        let first = lock(&core.inner).set_active("a", "sa");
        let second = lock(&core.inner).set_active("b", "sb");
        assert_eq!(core.send_input(item("sa", first, false)), Ok(()), "dropped");
        assert_eq!(
            core.send_input(item("sb", second, false)),
            Err("cdp not connected".to_owned()),
            "the current page's input is sent (there is no connection here)"
        );
    }

    #[test]
    fn a_mouse_release_sent_to_the_page_is_stamped() {
        let (core, _seen) = core();
        let within = Duration::from_secs(60);
        assert!(!core.panel_released_within(within), "never released");
        let first = lock(&core.inner).set_active("a", "sa");
        let second = lock(&core.inner).set_active("b", "sb");
        let _ = core.send_input(item("sa", first, true));
        assert!(!core.panel_released_within(within), "a dropped release");
        let _ = core.send_input(item("sb", second, false));
        assert!(!core.panel_released_within(within), "not a release");
        let _ = core.send_input(item("sb", second, true));
        assert!(core.panel_released_within(within));
    }

    #[test]
    fn only_listed_pages_can_be_shown_by_the_user() {
        let (core, _seen) = core();
        lock(&core.inner).upsert_page(&json!({ "targetId": "a", "url": "u" }));
        assert_eq!(core.require_tab("a"), Ok(()));
        assert_eq!(core.require_tab("worker-1"), Err("no such tab".to_owned()));
    }
}
