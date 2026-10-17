//! The panel's view of a tab's Chromium: one browser-level CDP client on
//! the tab's broker (`Target.attachToTarget { flatten: true }`, so one
//! connection serves every page session), the page targets, and the
//! screencast of the active one.
//!
//! Threads: the connection's reader runs [`events::handler`] (frames and
//! quick bookkeeping only), a worker thread does everything that needs a
//! CDP call in answer to an event, one thread emits frames (flow-controlled
//! by the webview's acks, see `frames.rs`) and one sends input in order.
//! Locks: `Core::inner` is never held across a CDP call or a frame emit;
//! `Core::switch` serialises target switches and viewport changes.

mod core;
mod emit;
mod events;
mod frames;
mod input;
mod model;
mod switching;
mod worker;

#[cfg(test)]
mod tests;

use std::sync::mpsc::{self, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use self::events::{Event, InputItem};
use super::annotate::CapturedImage;
use super::broker::Broker;
use super::cdp::CdpClient;
use super::lock;

pub(in crate::browser) use self::core::Core;
pub(in crate::browser) use emit::{
    BrowserEmit, Frame, Pick, Rect, Status, TabInfo, TauriBrowserEmit, ViewState,
};
pub use input::InputEvent;
pub(in crate::browser) use model::Size;

const MAX_URL_LEN: usize = 8192;
/// Input events not yet sent to Chromium (sending does not wait
/// for the page); beyond this the panel's input is refused ("browser is
/// busy").
const INPUT_QUEUE: usize = 512;

/// What the panel may load: the web, inline documents and a blank page
/// (what the address field's `normalizeUrl` makes); never `javascript:`,
/// `file:`, `chrome:` or other schemes that would act on the browser.
fn checked_url(url: &str) -> Result<&str, String> {
    let url = url.trim();
    if url.is_empty() || url.len() > MAX_URL_LEN || url.chars().any(char::is_control) {
        return Err("invalid url".into());
    }
    let starts = |prefix: &str| {
        url.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    };
    if ["http://", "https://", "data:"].into_iter().any(starts)
        || url.eq_ignore_ascii_case("about:blank")
    {
        Ok(url)
    } else {
        Err("unsupported url scheme".into())
    }
}

/// The panel's session on one tab's Chromium.
pub struct View {
    core: Arc<Core>,
    events: Sender<Event>,
    input: SyncSender<InputItem>,
}

impl View {
    /// Connect a browser-level client to the tab's Chromium through its
    /// broker (not through the agents' proxy).
    pub(in crate::browser) fn connect(
        sid: &str,
        broker: &Arc<Broker>,
        emit: Arc<dyn BrowserEmit>,
    ) -> Result<Self, String> {
        let core = Arc::new(Core::new(sid, emit));
        let (events, queue) = mpsc::channel();
        let handler = events::handler(Arc::downgrade(&core), events.clone()); // the view keeps one sender for `Barrier` and `Stop`
        let client = CdpClient::connect(broker.connect_browser()?, handler)?;
        if core.client.set(client).is_err() {
            return Err("cdp client already set".into());
        }
        let (input, input_queue) = mpsc::sync_channel(INPUT_QUEUE);
        events::spawn(&core, queue, input_queue)?;
        Ok(Self {
            core,
            events,
            input,
        })
    }

    /// Discover the pages, show the one the agent last worked in if it is
    /// still there (`start_on`) or else the newest (a blank one when there is
    /// none), and start following the agent.
    pub(in crate::browser) fn open(
        &self,
        size: Size,
        start_on: Option<&str>,
    ) -> Result<(), String> {
        let core = &self.core;
        lock(&core.inner).size = size;
        core.call(
            None,
            "Target.setDiscoverTargets",
            json!({ "discover": true }),
        )?;
        self.barrier()?;
        let targets = core.call(None, "Target.getTargets", json!({}))?;
        let first = {
            let mut inner = lock(&core.inner);
            for info in targets["targetInfos"].as_array().into_iter().flatten() {
                if info["type"] == "page" {
                    inner.upsert_page(info);
                }
            }
            // Owned: the lock must not span the switch.
            let first = start_on
                .filter(|t| inner.is_listed(t))
                .map(str::to_owned)
                .or_else(|| inner.pages.last().map(|p| p.target_id.clone()));
            drop(inner);
            first
        };
        let target = match first {
            Some(target) => target,
            None => self.core.create_page()?,
        };
        core.update_state(|s| s.status = Status::Running);
        core.switch_to(&target)?;
        lock(&core.inner).following = true;
        Ok(())
    }

    /// The agent worked in page `target`: the view switches to it unless the
    /// user acted a moment ago (then when the hold ends), or when it appears
    /// if it is not listed yet. Callers are proxy threads: this only records
    /// the page in the follow slot (a newer report replaces an older one the
    /// worker has not taken) and wakes the worker if no wake-up is queued.
    pub fn follow_agent(&self, target: &str) {
        if self.core.follow.put(target) {
            let _ = self.events.send(Event::Follow);
        }
    }

    /// Wait until the worker has handled everything received so far.
    fn barrier(&self) -> Result<(), String> {
        let (done, wait) = mpsc::channel();
        self.events
            .send(Event::Barrier(done))
            .map_err(|_| "browser view closed")?;
        wait.recv_timeout(Duration::from_secs(10))
            .map_err(|_| "browser view did not answer".to_owned())
    }

    /// The webview painted or dropped frame `seq`: the next one may follow.
    pub fn frame_ack(&self, seq: u64) {
        self.core.frame_ack(seq);
    }

    /// Turn annotate mode on or off (for this view's pages, shown now and
    /// later); a closed view has it off.
    pub fn annotate(&self, on: bool) -> Result<(), String> {
        self.core.set_annotate(on)
    }

    /// A PNG of `doc_rect`, the pick's document rect (CSS pixels) on the
    /// page shown.
    pub fn capture(&self, doc_rect: &Rect) -> Result<CapturedImage, String> {
        self.core.capture(doc_rect)
    }

    /// Scroll the shown page so that the absolute document `rect` is visible.
    pub fn scroll_to(&self, rect: &Rect) -> Result<(), String> {
        self.core.scroll_to(rect)
    }

    /// Stop the screencast and drop the connection; Chromium keeps running.
    /// Idempotent.
    pub fn close(&self) {
        let core = &self.core;
        if !core.mark_closed() {
            return;
        }
        // An error of the view's pages no longer applies to a closed view.
        core.update_state(|s| {
            s.status = Status::Idle;
            s.annotate = false;
            s.error = None;
        });
        if let (Some(client), Ok(session)) = (core.client.get(), core.active_session()) {
            core.release_overlay(&session); // the page keeps running: no overlay left in it
                                            // No waiting: the connection ends right after, which detaches the
                                            // session anyway.
            client.notify(Some(&session), "Page.stopScreencast", json!({}));
            client.notify(
                None,
                "Target.detachFromTarget",
                json!({ "sessionId": session }),
            );
        }
        let _ = self.events.send(Event::Stop);
        core.frames.close();
        if let Some(client) = core.client.get() {
            client.close();
        }
    }

    pub fn resize(&self, width: u32, height: u32, dpr: f64) -> Result<(), String> {
        lock(&self.core.inner).size = Size::new(width, height, dpr)?;
        self.core.resize_shown_page()
    }

    /// Load `url` in the shown page. Returns once sent: `Page.navigate`
    /// answers only when the navigation commits, which a slow server can
    /// delay past any timeout; progress and failure arrive as the state's
    /// `loading` and `error`.
    pub fn navigate(&self, url: &str) -> Result<(), String> {
        let url = checked_url(url)?;
        let session = self.core.active_session()?;
        let client = self.core.client.get().ok_or("cdp not connected")?;
        self.core.stamp_user_activation(); // the user chose this page
        client.post(Some(&session), "Page.navigate", json!({ "url": url }))
    }

    /// Go `delta` entries back (negative) or forward; out of range does
    /// nothing.
    pub fn history(&self, delta: i32) -> Result<(), String> {
        self.core.stamp_user_activation();
        let history = self
            .core
            .call_active("Page.getNavigationHistory", json!({}))?;
        let current = history["currentIndex"].as_i64().unwrap_or(0);
        let entries = history["entries"].as_array().map_or(&[][..], Vec::as_slice);
        let target = usize::try_from(current + i64::from(delta)).ok();
        let Some(entry) = target.and_then(|i| entries.get(i)) else {
            return Ok(());
        };
        self.core
            .call_active(
                "Page.navigateToHistoryEntry",
                json!({ "entryId": entry["id"] }),
            )
            .map(drop)
    }

    /// Open a blank tab and show it.
    pub fn tab_new(&self) -> Result<(), String> {
        self.core.stamp_user_activation(); // the new page is the user's choice, not the agent's
        let target = self.core.create_page()?;
        self.core.switch_to(&target)
    }

    /// Close one of the listed tabs; the last one is replaced by a blank
    /// page.
    pub fn tab_close(&self, target_id: &str) -> Result<(), String> {
        self.core.require_tab(target_id)?;
        self.core.stamp_user_activation();
        let only = lock(&self.core.inner).pages.len() <= 1;
        if only {
            self.core.create_page()?;
        }
        self.core
            .call(None, "Target.closeTarget", json!({ "targetId": target_id }))
            .map(drop)
    }

    /// Show one of the listed tabs.
    pub fn tab_activate(&self, target_id: &str) -> Result<(), String> {
        self.core.require_tab(target_id)?;
        self.core.stamp_user_activation();
        self.core.switch_to(target_id)
    }

    /// Queue an input event for the page shown now; events are sent in
    /// order, and one still queued when another page is shown is dropped,
    /// not sent to that page. Acting on the page (anything but moving the
    /// pointer) holds the view there, as picking a tab does.
    pub fn input(&self, event: &InputEvent) -> Result<(), String> {
        let (method, params) = event.to_command()?;
        if event.is_deliberate() {
            self.core.stamp_user_activation();
        }
        let (session, generation) = self.core.activation()?;
        let item = InputItem {
            session,
            generation,
            method,
            params,
            release: event.is_release(),
        };
        self.input.try_send(item).map_err(|e| match e {
            TrySendError::Full(_) => "browser is busy".to_owned(),
            TrySendError::Disconnected(_) => "browser view closed".to_owned(),
        })
    }

    /// `Runtime.evaluate` of `expression` in the active page.
    #[cfg(test)]
    pub(in crate::browser) fn evaluate_for_test(
        &self,
        expression: &str,
    ) -> Result<serde_json::Value, String> {
        let result = self.core.call_active(
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true }),
        )?;
        Ok(result["result"]["value"].clone())
    }
}

impl Drop for View {
    fn drop(&mut self) {
        self.close();
    }
}
