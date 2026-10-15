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
//! Each same-process frame has a context of that world too, switched alike:
//! there the script only swallows the user's clicks, it draws and picks in
//! the top document only. Cross-site frames run in other processes, whose
//! targets the view does not attach: clicks there reach the page.
//!
//! Lock order: `switch` → `overlay_install` → `annotate`; `inner` → `annotate`.

mod capture;
mod pick;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

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
    /// The overlay world's contexts per session the overlay is (being)
    /// installed on.
    contexts: HashMap<String, Vec<i64>>,
    /// Session and overlay context of the last pick emitted: a capture is
    /// of that document or fails.
    picked: Option<(String, i64)>,
}

impl Annotate {
    /// Whether `on`, after recording a context of `session` (ignored for a
    /// session the overlay is not installed on).
    fn add_context(&mut self, session: &str, id: i64) -> bool {
        let Some(contexts) = self.contexts.get_mut(session) else {
            return false;
        };
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

    /// Start tracking `session`'s contexts; `false` when it already was.
    fn install(&mut self, session: &str) -> bool {
        if self.contexts.contains_key(session) {
            return false;
        }
        self.contexts.insert(session.to_owned(), Vec::new());
        true
    }

    fn forget(&mut self, session: &str) {
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
        if !lock(&self.annotate).install(session) {
            return Ok(());
        }
        // In order on the session, so only the last is waited for: `Runtime.
        // enable` first (it reports the world `runImmediately` makes), then
        // the binding the script calls. A session that refuses one refuses
        // the script too (it is gone).
        self.notify(Some(session), "Runtime.enable", json!({}));
        self.notify(
            Some(session),
            "Runtime.addBinding",
            json!({ "name": BINDING, "executionContextName": WORLD }),
        );
        let installed = self.call(
            Some(session),
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": OVERLAY, "worldName": WORLD, "runImmediately": true }),
        );
        installed.map(drop).map_err(|e| {
            lock(&self.annotate).forget(session);
            format!("annotate overlay: {e}")
        })
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
            // Flag and enables under the lock, like a release and the
            // reader's enables: a close or toggle taking it next is sent
            // after them, so the page ends with the final flag. A context
            // that is gone fails: the page navigated, and its successor is
            // switched by the reader.
            let last = {
                let mut annotate = lock(&self.annotate);
                if self.is_closed() {
                    return Err("view closed".into());
                }
                annotate.on = on;
                let mut last = None;
                if let (Some(session), Some(client)) = (session.as_deref(), self.client.get()) {
                    for &context in annotate.contexts.get(session).into_iter().flatten() {
                        client.notify(Some(session), "Runtime.evaluate", set_params(on, context));
                        last = Some(context);
                    }
                }
                drop(annotate);
                last
            };
            // One round trip after them, so the fresh frame below shows the
            // change.
            if let (Some(session), Some(context)) = (session.as_deref(), last) {
                let _ = self.call(
                    Some(session),
                    "Runtime.evaluate",
                    json!({ "expression": "0", "contextId": context }),
                );
            }
            // A close reports `false` itself; a toggle it overtook must not
            // report the flag afterwards.
            self.update_state(|s| {
                if !self.is_closed() {
                    s.annotate = on;
                }
            });
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
