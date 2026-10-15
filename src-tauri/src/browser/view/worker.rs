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

/// Attempts at one screencast restart, `HEAL_GAP` apart (see `model.rs`).
const RESTART_TRIES: usize = 4;

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
                // A page in the middle of committing a navigation refuses
                // the restart for a moment; one that failed is tried again,
                // or the screencast could stay stopped with no frame left to
                // ask for another.
                for _ in 0..RESTART_TRIES {
                    let wait = lock(&core.inner).restart_wait();
                    thread::sleep(wait);
                    lock(&core.inner).restart_started();
                    if core.is_closed() || core.resize_shown_page().is_ok() {
                        break;
                    }
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
        _ if session.is_some_and(|s| lock(&core.inner).is_active_session(s)) => {
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
        // Asked at `frameNavigated`, Chromium may still be swapping in the
        // committed document ("Not attached to an active page"); once the
        // page stopped loading the history is settled, so it is asked again.
        "Page.frameStartedLoading" | "Page.frameStoppedLoading"
            if core.is_main_frame(params["frameId"].as_str()) =>
        {
            let loading = method == "Page.frameStartedLoading";
            core.update_state(|s| s.loading = loading);
            if !loading {
                let _ = core.refresh_history();
            }
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
        end_annotate(core);
        core.follow_to(&id);
    }
}

/// The agent worked in the page in the follow slot (see `proxy/`): show it,
/// or show it when it is listed or the user's hold ends.
fn on_follow(core: &Core) {
    #[cfg(test)]
    drop(lock(&core.follow_gate)); // a test holds it to keep the worker from taking the slot
    let Some(target) = core.follow.take() else {
        return;
    };
    end_annotate(core);
    core.follow_to(&target);
}

/// The user's hold ended with a follow kept back: show that page now.
fn on_hold_ended(core: &Core) {
    let held = lock(&core.inner).take_held_follow();
    if let Some(target) = held {
        end_annotate(core);
        core.follow_to(&target);
    }
}

/// The agent acting ends annotate mode, whenever its page is shown: the
/// user's picks would land on a page it changes.
fn end_annotate(core: &Core) {
    let annotating = lock(&core.annotate).on;
    if annotating {
        let _ = core.set_annotate(false);
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
        Some(None) => match core.create_page() {
            Ok(id) => {
                let _ = core.switch_to(&id);
            }
            Err(e) => core.update_state(|s| s.error = Some(format!("cannot open a page: {e}"))),
        },
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;

    use super::super::events::Event;
    use super::super::tests::{fake_core, sent_count};
    use super::*;
    use crate::browser::tests::{cdp_error, cdp_result};

    #[test]
    fn a_refused_screencast_restart_is_tried_again() {
        let mut refused = false;
        let (core, _states, sent) = fake_core(move |request| {
            if request["method"] == "Emulation.setDeviceMetricsOverride" && !refused {
                refused = true;
                return Some(cdp_error(request, "Not attached to an active page"));
            }
            None
        });
        lock(&core.inner).set_active("a", "sa");
        let (events, queue) = std::sync::mpsc::channel();
        events.send(Event::Restart).unwrap();
        events.send(Event::Stop).unwrap();
        run_worker(&core, &queue);
        assert_eq!(
            sent_count(&sent, "Page.startScreencast"),
            1,
            "started on the second try"
        );
    }

    #[test]
    fn the_history_is_asked_again_once_the_page_stopped_loading() {
        // Refused for as long as the commit lasts, longer than `Core::call`
        // sends a refused command again.
        let committing = Arc::new(AtomicBool::new(false));
        let refusing = Arc::clone(&committing);
        let (core, _states, _sent) = fake_core(move |request| {
            if request["method"] != "Page.getNavigationHistory" {
                return None;
            }
            if refusing.load(Ordering::SeqCst) {
                return Some(cdp_error(request, "Not attached to an active page"));
            }
            Some(cdp_result(
                request,
                json!({ "currentIndex": 1, "entries": [
                    { "id": 1, "url": "about:blank" },
                    { "id": 2, "url": "data:text/html,a" },
                ] }),
            ))
        });
        assert_eq!(core.switch_to("a"), Ok(()));
        core.update_state(|s| s.can_back = false);
        committing.store(true, Ordering::SeqCst);
        let committed = json!({ "frame": { "id": "a", "url": "data:text/html,a" } });
        on_page_event(&core, "Page.frameNavigated", &committed);
        assert!(!lock(&core.inner).state.can_back, "asked while committing");
        committing.store(false, Ordering::SeqCst);
        on_page_event(
            &core,
            "Page.frameStoppedLoading",
            &json!({ "frameId": "a" }),
        );
        assert!(lock(&core.inner).state.can_back);
    }

    #[test]
    fn a_held_follow_that_changes_the_page_ends_annotate_mode() {
        let (core, _states, _sent) = fake_core(|_| None);
        assert_eq!(core.switch_to("a"), Ok(()));
        let hold_ends = {
            let mut inner = lock(&core.inner);
            inner.upsert_page(&json!({ "targetId": "b", "url": "about:blank" }));
            inner.following = true;
            inner.follow_hold = Duration::from_millis(50);
            inner.user_activated = Some(Instant::now());
            assert!(
                !inner.request_follow("b"),
                "the user's choice holds the view"
            );
            inner.follow_deadline().unwrap()
        };
        lock(&core.annotate).on = true; // turned on during the hold
        thread::sleep(hold_ends.saturating_duration_since(Instant::now()));
        on_hold_ended(&core);
        assert!(lock(&core.inner).is_shown("b"), "the agent's page is shown");
        assert!(!lock(&core.annotate).on, "annotate mode ended with it");
    }

    #[test]
    fn a_page_that_cannot_replace_the_last_closed_one_is_told() {
        let (core, states, _sent) = fake_core(|request| {
            (request["method"] == "Target.createTarget").then(|| cdp_error(request, "nope"))
        });
        assert_eq!(core.switch_to("a"), Ok(()));
        on_target_destroyed(&core, Some("a"));
        assert_eq!(
            states.last_error().as_deref(),
            Some("cannot open a page: nope")
        );
    }
}
