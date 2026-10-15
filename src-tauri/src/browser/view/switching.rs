//! Which page the view shows: the user's tab choices, the agent's follows
//! and the switch between page sessions.

use std::time::Instant;

use serde_json::json;

use super::super::lock;
use super::core::Core;

impl Core {
    /// Attach to `target_id` and give back the page session.
    fn attach(&self, target_id: &str) -> Result<String, String> {
        self.call(
            None,
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )
        .and_then(|attached| {
            attached["sessionId"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "attachToTarget returned no session".to_owned())
        })
    }

    /// `Ok` when `target_id` is one of the listed pages.
    pub(super) fn require_tab(&self, target_id: &str) -> Result<(), String> {
        if lock(&self.inner).is_listed(target_id) {
            Ok(())
        } else {
            Err("no such tab".into())
        }
    }

    pub(super) fn switch_to(&self, target_id: &str) -> Result<(), String> {
        let _switching = lock(&self.switch);
        self.switch_locked(target_id)
    }

    /// The agent worked in `target_id`: show it, unless the user's recent
    /// choice holds the view (then it is kept for when the hold ends).
    /// Decided under the switch lock, so a tab the user picks meanwhile is
    /// not overridden.
    pub(super) fn follow_to(&self, target_id: &str) {
        let _switching = lock(&self.switch);
        if lock(&self.inner).request_follow(target_id) {
            let _ = self.switch_locked(target_id);
        }
    }

    /// Show `target_id`: attach to it first, so a page that cannot be
    /// attached leaves the shown one as it is; then stop the old page's
    /// screencast, detach it and start the new one. The `tabs` event
    /// marking it active precedes its frames. A failure is also told in the
    /// state's `error`, which the next switch that works clears; until a
    /// switch to a page finished, picking it again retries the switch.
    fn switch_locked(&self, target_id: &str) -> Result<(), String> {
        let size = {
            let inner = lock(&self.inner);
            if inner.is_shown(target_id) {
                return Ok(());
            }
            inner.size
        };
        let shown = self.attach(target_id).and_then(|session| {
            let (old, generation) = {
                let order = lock(&self.emit_order);
                let mut inner = lock(&self.inner);
                let old = inner.active.as_ref().map(|a| a.session.clone()); // owned: used after the swap
                let generation = inner.set_active(target_id, &session);
                self.publish_tabs(&mut inner);
                drop(inner);
                drop(order);
                (old, generation)
            };
            if let Some(old) = old {
                self.release_overlay(&old); // the old page must not stay in annotate mode
                self.notify(Some(&old), "Page.stopScreencast", json!({}));
                self.notify(None, "Target.detachFromTarget", json!({ "sessionId": old }));
            }
            self.notify(
                None,
                "Target.activateTarget",
                json!({ "targetId": target_id }),
            );
            self.call(Some(&session), "Page.enable", json!({}))?;
            // Best effort: turning annotate on installs it again and says why not.
            let _ = self.install_overlay(&session);
            self.apply_viewport(&session, size)?;
            lock(&self.inner).mark_shown(generation);
            Ok(())
        });
        if let Err(e) = &shown {
            self.update_state(|s| s.error = Some(format!("cannot show the page: {e}")));
            return shown;
        }
        self.update_state(|s| {
            s.loading = false;
            s.error = None;
            s.url.clear();
        });
        let _ = self.refresh_title();
        self.refresh_history()
    }

    pub(super) fn stamp_user_activation(&self) {
        lock(&self.inner).user_activated = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{fake_core, sent_count};
    use crate::browser::tests::cdp_error;

    #[test]
    fn a_switch_that_failed_is_retried_when_the_page_is_picked_again() {
        let mut failed = false;
        let (core, states, sent) = fake_core(move |request| {
            if request["method"] == "Page.enable" && !failed {
                failed = true;
                return Some(cdp_error(request, "boom"));
            }
            None
        });
        assert_eq!(core.switch_to("a"), Err("boom".to_owned()));
        assert_eq!(
            states.last_error().as_deref(),
            Some("cannot show the page: boom")
        );
        assert_eq!(core.switch_to("a"), Ok(()), "picked again");
        assert_eq!(
            sent_count(&sent, "Page.enable"),
            2,
            "the switch was retried"
        );
        assert_eq!(
            states.last_error(),
            None,
            "the switch that worked cleared the error"
        );
        assert_eq!(core.switch_to("a"), Ok(()));
        assert_eq!(
            sent_count(&sent, "Page.enable"),
            2,
            "a page shown is not switched to again"
        );
    }
}
