//! The view's worker thread: reacts to target and page events with the CDP
//! calls the reader thread must not make, and shows the agent's page once a
//! hold that kept it back ends.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::Instant;

use serde_json::Value;

use super::super::cdp::DISCONNECTED;
use super::super::lock;
use super::core::Core;
use super::emit::Status;
use super::events::Event;

pub(super) fn run_worker(core: &Core, events: &Receiver<Event>) {
    loop {
        // A follow kept back by the user's hold is shown when the hold ends.
        let deadline = lock(&core.inner).follow_deadline();
        let event = match deadline {
            Some(at) => match events.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => {
                    on_hold_ended(core);
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match events.recv() {
                Ok(event) => event,
                Err(_) => break,
            },
        };
        match event {
            Event::Stop => break,
            Event::Follow => on_follow(core),
            Event::Barrier(done) => {
                let _ = done.send(());
            }
            Event::Restart => {
                let wait = lock(&core.inner).restart_wait();
                thread::sleep(wait);
                let size = {
                    let mut inner = lock(&core.inner);
                    inner.restart_started();
                    inner.size
                };
                if let Ok(session) = core.active_session() {
                    let _ = core.resize_to(&session, size);
                }
            }
            Event::Cdp {
                session,
                method,
                params,
            } => handle(core, session.as_deref(), &method, &params),
        }
    }
}

fn handle(core: &Core, session: Option<&str>, method: &str, params: &Value) {
    match method {
        "Target.targetCreated" | "Target.targetInfoChanged" => {
            on_target_event(core, method, params);
        }
        "Target.targetDestroyed" => on_target_destroyed(core, params["targetId"].as_str()),
        DISCONNECTED => {
            if !core.is_closed() {
                core.update_state(|s| {
                    s.status = Status::Stopped;
                    s.error = Some("browser connection lost".into());
                });
            }
        }
        _ if session.is_some_and(|s| core.active_session().is_ok_and(|a| a == s)) => {
            on_page_event(core, method, params);
        }
        _ => {}
    }
}

fn on_page_event(core: &Core, method: &str, params: &Value) {
    match method {
        "Page.frameNavigated" => {
            let frame = &params["frame"];
            if frame.get("parentId").is_none() {
                // A failed load commits an error page that names the URL.
                let failed = frame["unreachableUrl"].as_str();
                let url = failed.or_else(|| frame["url"].as_str());
                core.update_state(|s| {
                    if let Some(url) = url {
                        url.clone_into(&mut s.url);
                    }
                    s.error = failed.map(|url| format!("could not load {url}"));
                });
                let _ = core.refresh_history();
            }
        }
        // Subframes (ads, embeds) load on their own; only the page's counts.
        "Page.frameStartedLoading" | "Page.frameStoppedLoading"
            if core.is_main_frame(params["frameId"].as_str()) =>
        {
            let loading = method == "Page.frameStartedLoading";
            core.update_state(|s| s.loading = loading);
        }
        // The history changed at `frameNavigated`; the title may have now.
        "Page.loadEventFired" => {
            core.update_state(|s| s.loading = false);
            let _ = core.refresh_title();
        }
        "Page.navigatedWithinDocument" => {
            let _ = core.refresh_history();
            let _ = core.refresh_title();
        }
        _ => {}
    }
}

/// A target event: the model keeps the pages current, and names the page the
/// agent was waiting for when it has just appeared.
fn on_target_event(core: &Core, method: &str, params: &Value) {
    let mut inner = lock(&core.inner);
    let follow = inner.apply_target_event(method, params);
    core.publish_tabs(&mut inner);
    drop(inner);
    if let Some(id) = follow {
        core.follow_to(&id);
    }
}

/// The agent worked in the page in the follow slot (see `proxy/`): show it,
/// or show it when it is listed or the user's hold ends. The agent acting
/// ends annotate mode: the user's picks would land on a page it changes.
fn on_follow(core: &Core) {
    #[cfg(test)]
    drop(lock(&core.follow_gate)); // a test holds it to keep the worker from taking the slot
    let Some(target) = core.follow.take() else {
        return;
    };
    let annotating = lock(&core.annotate).on;
    if annotating {
        let _ = core.set_annotate(false);
    }
    core.follow_to(&target);
}

/// The user's hold ended with a follow kept back: show that page now.
fn on_hold_ended(core: &Core) {
    let held = lock(&core.inner).take_held_follow();
    if let Some(target) = held {
        core.follow_to(&target);
    }
}

/// A closed page: drop it, and show the newest remaining one (a blank one
/// when none is left) if it was the active page.
fn on_target_destroyed(core: &Core, target_id: Option<&str>) {
    let Some(target_id) = target_id else {
        return;
    };
    let next = {
        let mut inner = lock(&core.inner);
        let removed = inner.remove_page(target_id);
        let was_active = inner
            .active
            .as_ref()
            .is_some_and(|a| a.target_id == target_id);
        if was_active {
            if let Some(gone) = inner.active.take() {
                core.release_overlay(&gone.session); // its session is gone with it
            }
        }
        core.publish_tabs(&mut inner);
        (removed && was_active).then(|| inner.pages.last().map(|p| p.target_id.clone()))
        // owned: the lock must not span the switch
    };
    match next {
        Some(Some(id)) => {
            let _ = core.switch_to(&id);
        }
        Some(None) => {
            if let Ok(id) = core.create_page() {
                let _ = core.switch_to(&id);
            }
        }
        None => {}
    }
}
