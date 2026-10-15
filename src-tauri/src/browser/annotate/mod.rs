//! Annotate mode: the overlay script running in an isolated world of the
//! shown page, the picks it reports, and area capture.
//!
//! The script (`overlay.js`) is installed on every page session the view
//! attaches to. Chromium creates its world as an execution context named
//! [`WORLD`]; the view turns the overlay on and off by evaluating in those
//! contexts, and a context created later (a new document) is switched on by
//! the event handler itself while annotate is on. The binding is exposed in
//! that world only, so a page script cannot forge a pick; and only a click
//! the panel sent makes one, not the agent's.
//!
//! Lock order: `switch` → `overlay_install` → `annotate`; `inner` → `annotate`.

mod capture;
mod pick;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use super::lock;
use super::view::Core;

pub use capture::CapturedImage;

const OVERLAY: &str = include_str!("overlay.js");
const WORLD: &str = "omp-annotate";
const BINDING: &str = "__ompAnnotate";

/// The events [`Core::on_overlay_event`] takes, on the reader thread.
pub(in crate::browser) const EVENTS: [&str; 4] = [
    "Runtime.executionContextCreated",
    "Runtime.executionContextDestroyed",
    "Runtime.executionContextsCleared",
    "Runtime.bindingCalled",
];

/// Annotate state of one view. Locked briefly, never across a blocking CDP
/// call: the reader thread takes it too. Notifications stay under the lock
/// so a release or toggle cannot be followed by a stale enable.
#[derive(Default)]
pub(in crate::browser) struct Annotate {
    pub(in crate::browser) on: bool,
    /// Page sessions the overlay is (being) installed on.
    installed: HashSet<String>,
    /// The overlay world's contexts per installed session.
    contexts: HashMap<String, Vec<i64>>,
    /// Session and overlay context of the last pick emitted: a capture is
    /// of that document or fails.
    picked: Option<(String, i64)>,
}

impl Annotate {
    /// Whether `on`, after recording a context of `session` (ignored for a
    /// session the overlay is not installed on).
    fn add_context(&mut self, session: &str, id: i64) -> bool {
        if !self.installed.contains(session) {
            return false;
        }
        let contexts = self.contexts.entry(session.to_owned()).or_default();
        if !contexts.contains(&id) {
            contexts.push(id);
        }
        self.on
    }

    fn owns(&self, session: &str, id: i64) -> bool {
        self.contexts.get(session).is_some_and(|c| c.contains(&id))
    }

    fn contexts_of(&self, session: &str) -> Vec<i64> {
        self.contexts.get(session).cloned().unwrap_or_default() // owned: used after the lock
    }

    fn drop_context(&mut self, session: &str, id: Option<i64>) {
        if let Some(contexts) = self.contexts.get_mut(session) {
            contexts.retain(|c| id.is_some_and(|id| *c != id));
        }
        if self
            .picked
            .as_ref()
            .is_some_and(|(s, c)| s == session && id.is_none_or(|id| *c == id))
        {
            self.picked = None;
        }
    }

    /// The overlay context the last pick of `session` came from, if it is
    /// still there.
    fn picked_context(&self, session: &str) -> Option<i64> {
        self.picked
            .as_ref()
            .filter(|(s, c)| s == session && self.owns(s, *c))
            .map(|(_, c)| *c)
    }

    fn forget(&mut self, session: &str) {
        self.installed.remove(session);
        self.drop_context(session, None);
        self.contexts.remove(session);
    }
}

fn set_params(on: bool, context: i64) -> Value {
    json!({
        "expression": format!("window.__ompAnnotateSet && window.__ompAnnotateSet({on})"),
        "contextId": context,
    })
}

impl Core {
    /// Install the overlay on page session `session`. Idempotent. Not under
    /// the annotate lock, whose holders must not wait on the reader thread.
    pub(in crate::browser) fn install_overlay(&self, session: &str) -> Result<(), String> {
        let _installing = lock(&self.overlay_install);
        if !lock(&self.annotate).installed.insert(session.to_owned()) {
            return Ok(());
        }
        // `Runtime.enable` first: it reports the world `runImmediately` makes.
        let steps = [
            ("Runtime.enable", json!({})),
            (
                "Runtime.addBinding",
                json!({ "name": BINDING, "executionContextName": WORLD }),
            ),
            (
                "Page.addScriptToEvaluateOnNewDocument",
                json!({ "source": OVERLAY, "worldName": WORLD, "runImmediately": true }),
            ),
        ];
        for (method, params) in steps {
            if let Err(e) = self.call(Some(session), method, params) {
                lock(&self.annotate).forget(session);
                return Err(format!("annotate overlay: {e}"));
            }
        }
        Ok(())
    }

    /// Annotate on or off for the shown page, and for every page shown
    /// later. Turning it on installs the overlay on the shown page and fails
    /// when that does; turning it off clears the flag first and only fails
    /// for a closed view. Takes `switch`: call it holding no view lock.
    pub(in crate::browser) fn set_annotate(&self, on: bool) -> Result<(), String> {
        {
            // Target switches install the new world's contexts using this
            // flag: the session, flag and context updates stay in order.
            let _switching = lock(&self.switch);
            if self.is_closed() {
                return Err("view closed".into());
            }
            let session = if on {
                let session = self.active_session()?;
                self.install_overlay(&session)?;
                Some(session)
            } else {
                self.active_session().ok()
            };
            let contexts = {
                let mut annotate = lock(&self.annotate);
                if self.is_closed() {
                    return Err("view closed".into());
                }
                annotate.on = on;
                let contexts = session
                    .as_deref()
                    .map_or_else(Vec::new, |s| annotate.contexts_of(s));
                drop(annotate);
                contexts
            };
            // A context that appears from now on sees the flag. One that is
            // gone fails: the page navigated, and its successor is switched.
            if let Some(session) = session.as_deref() {
                for context in contexts {
                    let _ = self.call(Some(session), "Runtime.evaluate", set_params(on, context));
                }
            }
            self.update_state(|s| s.annotate = on);
        }
        // The overlay's change shows now, not with the page's next repaint.
        self.refresh_frame();
        Ok(())
    }

    /// Switch the overlay of `session` off and forget it (the session is
    /// about to be detached); the annotate flag stays.
    pub(in crate::browser) fn release_overlay(&self, session: &str) {
        let mut annotate = lock(&self.annotate);
        let contexts = annotate.contexts_of(session);
        annotate.forget(session);
        if let Some(client) = self.client.get() {
            for context in contexts {
                client.notify(
                    Some(session),
                    "Runtime.evaluate",
                    set_params(false, context),
                );
            }
        }
        drop(annotate); // Keep release notifications ordered with late contexts.
    }
}
