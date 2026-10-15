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
    /// state's `error`.
    fn switch_locked(&self, target_id: &str) -> Result<(), String> {
        let size = {
            let inner = lock(&self.inner);
            if inner
                .active
                .as_ref()
                .is_some_and(|a| a.target_id == target_id)
            {
                return Ok(());
            }
            inner.size
        };
        let shown = self.attach(target_id).and_then(|session| {
            let old = {
                let order = lock(&self.emit_order);
                let mut inner = lock(&self.inner);
                let old = inner.active.as_ref().map(|a| a.session.clone()); // owned: used after the swap
                inner.set_active(target_id, &session);
                self.publish_tabs(&mut inner);
                drop(inner);
                drop(order);
                old
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
            self.apply_viewport(&session, size)
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
