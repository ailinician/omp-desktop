//! The browser panel's face: the panel half of [`BrowserManager`] (the
//! per-session [`View`]) and the Tauri commands over it. All commands but
//! `browser_input` do their CDP round trips off the IPC thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Manager, State};

use super::annotate::CapturedImage;
use super::view::{
    BrowserEmit, Frame, InputEvent, Pick, Rect, Size, Status, TabInfo, TauriBrowserEmit, View,
    ViewState,
};
use super::{lock, BrowserManager, Entry, NO_BROWSER};

/// What an open that lost to a close says.
const PANEL_CLOSED: &str = "panel closed";

/// What an open that a newer open of the same session superseded says.
const REOPENED: &str = "panel reopened";

/// One open's sink: its messages reach the panel until a newer open of the
/// same session supersedes it. A restart reopens on the same webview
/// handler, so a superseded open's `running`, frames or closing `idle`
/// would otherwise land in the live panel.
struct OpenEmit {
    emit: Arc<dyn BrowserEmit>,
    superseded: AtomicBool,
}

impl OpenEmit {
    fn live(&self) -> bool {
        !self.superseded.load(Ordering::SeqCst)
    }
}

impl BrowserEmit for OpenEmit {
    fn frame(&self, sid: &str, f: &Frame) {
        if self.live() {
            self.emit.frame(sid, f);
        }
    }

    fn tabs(&self, sid: &str, tabs: &[TabInfo]) {
        if self.live() {
            self.emit.tabs(sid, tabs);
        }
    }

    fn state(&self, sid: &str, s: &ViewState) {
        if self.live() {
            self.emit.state(sid, s);
        }
    }

    fn pick(&self, sid: &str, p: &Pick) {
        if self.live() {
            self.emit.pick(sid, p);
        }
    }
}

/// The session's panel: its view while open, a generation that
/// [`close`](Self::close) and every open advance, and the page the agent last
/// worked in. An open captures the generation when it starts and installs
/// its view only if it is still the same when it is done, so a close or a
/// newer open that came in between wins.
#[derive(Default)]
pub(super) struct PanelSlot {
    view: Option<Arc<View>>,
    generation: u64,
    /// The sink of the newest open still in flight; a newer open silences it.
    opening: Option<Arc<OpenEmit>>,
    /// What the proxy last reported (`tap.rs`); a panel opened later starts
    /// there.
    last_agent_target: Option<String>,
}

impl PanelSlot {
    /// Empty the slot and make the opens still in flight stale.
    pub const fn close(&mut self) -> Option<Arc<View>> {
        self.generation += 1;
        self.view.take()
    }

    /// The agent worked in page `target` (reported from a proxy thread):
    /// remember it, and let the open view follow.
    pub fn agent_worked_in(&mut self, target: &str) {
        target.clone_into(self.last_agent_target.get_or_insert_with(String::new));
        if let Some(view) = &self.view {
            view.follow_agent(target);
        }
    }
}

pub(super) type ViewSlot = Mutex<PanelSlot>;

impl BrowserManager {
    fn entry(&self, session_id: &str) -> Result<Arc<Entry>, String> {
        self.state()
            .entries
            .get(session_id)
            .map(Arc::clone)
            .ok_or_else(|| NO_BROWSER.to_owned())
    }

    /// Open the session's panel: make sure its Chromium runs, connect to it
    /// and start the screencast. Replaces an earlier view of the session and
    /// supersedes an earlier open still in flight (the newest open wins).
    pub fn open_panel(
        &self,
        session_id: &str,
        width: u32,
        height: u32,
        dpr: f64,
        emit: Arc<dyn BrowserEmit>,
    ) -> Result<(), String> {
        self.open_panel_with(session_id, width, height, dpr, emit, |_| {})
    }

    /// [`open_panel`](Self::open_panel); `before_install` runs once the view
    /// is open, just before it is installed (a test holds the open there).
    pub(super) fn open_panel_with(
        &self,
        session_id: &str,
        width: u32,
        height: u32,
        dpr: f64,
        emit: Arc<dyn BrowserEmit>,
        before_install: impl FnOnce(&Arc<View>),
    ) -> Result<(), String> {
        // A bad size fails before Chromium is launched for it.
        let size = Size::new(width, height, dpr)?;
        let entry = self.entry(session_id)?;
        let sink = Arc::new(OpenEmit {
            emit,
            superseded: AtomicBool::new(false),
        });
        let (generation, old, last_agent) = {
            let mut panel = lock(&entry.view);
            panel.generation += 1;
            // The slot and this open both hold the sink.
            if let Some(earlier) = panel.opening.replace(Arc::clone(&sink)) {
                earlier.superseded.store(true, Ordering::SeqCst);
            }
            // Owned: the lock must not span the open.
            (
                panel.generation,
                panel.view.take(),
                panel.last_agent_target.clone(),
            )
        };
        if let Some(old) = old {
            old.close();
        }
        let emit = Arc::clone(&sink) as Arc<dyn BrowserEmit>;
        emit.state(session_id, &ViewState::new(Status::Starting));
        let started = self.chromium_port(session_id).and_then(|port| {
            let view = Arc::new(View::connect(session_id, port, Arc::clone(&emit))?);
            match view.open(size, last_agent.as_deref()) {
                Ok(()) => Ok(view),
                Err(e) => {
                    view.close();
                    Err(e)
                }
            }
        });
        let installed = started.and_then(|view| {
            before_install(&view);
            let mut panel = lock(&entry.view);
            // `Entry::close` marks the slot closed before it takes the view.
            let refused = if entry.slot.closed.load(Ordering::SeqCst) {
                Some("browser closed")
            } else if panel.generation != generation {
                Some(if sink.live() { PANEL_CLOSED } else { REOPENED })
            } else {
                None
            };
            if let Some(refused) = refused {
                drop(panel);
                view.close();
                return Err(refused.to_owned());
            }
            // Our generation since our start: no other view was installed.
            panel.view = Some(Arc::clone(&view)); // the slot and this scope both hold it
            if let Some(target) = &panel.last_agent_target {
                view.follow_agent(target); // a report that came in while the view opened
            }
            drop(panel);
            Ok(())
        });
        {
            let mut panel = lock(&entry.view);
            if panel
                .opening
                .as_ref()
                .is_some_and(|o| Arc::ptr_eq(o, &sink))
            {
                panel.opening = None;
            }
        }
        match &installed {
            // The panel was closed meanwhile: the closed view said `idle`.
            // A superseded open's sink is silent anyway.
            Err(e) if e != PANEL_CLOSED => {
                let mut state = ViewState::new(Status::Error);
                state.error = Some(e.clone()); // the state event and the caller each own it
                emit.state(session_id, &state);
            }
            _ => {}
        }
        installed
    }

    /// Close the session's panel view; Chromium keeps running. An open still
    /// in flight does not install its view afterwards.
    pub fn close_panel(&self, session_id: &str) -> Result<(), String> {
        let view = lock(&self.entry(session_id)?.view).close();
        if let Some(view) = view {
            view.close();
        }
        Ok(())
    }

    /// The session's open view.
    pub fn panel(&self, session_id: &str) -> Result<Arc<View>, String> {
        lock(&self.entry(session_id)?.view)
            .view
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| "panel not open".to_owned())
    }

    /// The page the proxy last reported for the session.
    #[cfg(test)]
    pub(super) fn last_agent_target_for_test(&self, session_id: &str) -> Option<String> {
        lock(&self.entry(session_id).ok()?.view)
            .last_agent_target
            .clone()
    }
}

/// Run `action` on the session's open view, off the IPC thread.
async fn with_view<T: Send + 'static>(
    app: AppHandle,
    session_id: String,
    action: impl FnOnce(&View) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    crate::off_ipc(move || {
        let view = app.state::<BrowserManager>().panel(&session_id)?;
        action(&view)
    })
    .await?
}

/// `events` is the panel's channel: every state, tabs, pick and frame
/// message of this open, in order, until the panel closes or reopens.
#[tauri::command]
pub async fn browser_open_panel(
    session_id: String,
    width: u32,
    height: u32,
    dpr: f64,
    events: Channel<InvokeResponseBody>,
    app: AppHandle,
) -> Result<(), String> {
    crate::off_ipc(move || {
        let emit = Arc::new(TauriBrowserEmit(events));
        app.state::<BrowserManager>()
            .open_panel(&session_id, width, height, dpr, emit)
    })
    .await?
}

#[tauri::command]
pub async fn browser_close_panel(session_id: String, app: AppHandle) -> Result<(), String> {
    crate::off_ipc(move || app.state::<BrowserManager>().close_panel(&session_id)).await?
}

#[tauri::command]
pub async fn browser_resize(
    session_id: String,
    width: u32,
    height: u32,
    dpr: f64,
    app: AppHandle,
) -> Result<(), String> {
    with_view(app, session_id, move |v| v.resize(width, height, dpr)).await
}

#[tauri::command]
pub async fn browser_navigate(
    session_id: String,
    url: String,
    app: AppHandle,
) -> Result<(), String> {
    with_view(app, session_id, move |v| v.navigate(&url)).await
}

#[tauri::command]
pub async fn browser_history(session_id: String, delta: i32, app: AppHandle) -> Result<(), String> {
    with_view(app, session_id, move |v| v.history(delta)).await
}

#[tauri::command]
pub async fn browser_tab_new(session_id: String, app: AppHandle) -> Result<(), String> {
    with_view(app, session_id, View::tab_new).await
}

#[tauri::command]
pub async fn browser_tab_close(
    session_id: String,
    target_id: String,
    app: AppHandle,
) -> Result<(), String> {
    with_view(app, session_id, move |v| v.tab_close(&target_id)).await
}

#[tauri::command]
pub async fn browser_tab_activate(
    session_id: String,
    target_id: String,
    app: AppHandle,
) -> Result<(), String> {
    with_view(app, session_id, move |v| v.tab_activate(&target_id)).await
}

/// Synchronous on purpose: it only queues the event, and invocations from
/// the frontend reach the queue in the order they were sent (a `spawn_blocking`
/// per event could reorder a press and its release).
#[tauri::command]
pub fn browser_input(
    session_id: String,
    event: InputEvent,
    browser: State<'_, BrowserManager>,
) -> Result<(), String> {
    browser.panel(&session_id)?.input(&event)
}

/// Synchronous and cheap: the webview painted or dropped frame `seq`, so
/// the panel's next frame may follow (see `view/frames.rs`).
#[tauri::command]
pub fn browser_frame_ack(
    session_id: String,
    seq: u64,
    browser: State<'_, BrowserManager>,
) -> Result<(), String> {
    browser.panel(&session_id)?.frame_ack(seq);
    Ok(())
}

#[tauri::command]
pub async fn browser_annotate(session_id: String, on: bool, app: AppHandle) -> Result<(), String> {
    with_view(app, session_id, move |v| v.annotate(on)).await
}

#[tauri::command]
pub async fn browser_capture(
    session_id: String,
    rect: Rect,
    app: AppHandle,
) -> Result<CapturedImage, String> {
    with_view(app, session_id, move |v| v.capture(&rect)).await
}

#[tauri::command]
pub async fn browser_scroll_to(
    session_id: String,
    rect: Rect,
    app: AppHandle,
) -> Result<(), String> {
    with_view(app, session_id, move |v| v.scroll_to(&rect)).await
}
