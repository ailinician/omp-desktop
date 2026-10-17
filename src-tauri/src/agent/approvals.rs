//! Tool-approval prompts one omp process forwarded to the human and that
//! are still waiting for an answer, so a standing grant is bound to the
//! prompt it came from (`approval_rules_grant` → [`AgentBridge::grant_approval`]).
//!
//! The frontend names a prompt by session id and request id only; the tool
//! and the project come from here — what this process actually asked, under
//! the folder it was started in. A prompt shown in the wrong tab, one from a
//! process since restarted under the same id, or one already settled matches
//! nothing and the grant is refused instead of landing on another session's
//! or project's scope. Lives on the session's map entry, so it ends with the
//! process.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::Value;

use super::AgentBridge;
use crate::approval::{self, RuleBook, RuleScope};

/// Open prompts kept per process. A turn with parallel gated tool calls
/// raises one prompt per call before any is answered; this is far beyond
/// that, and bounds a process whose prompts are never answered.
const MAX_PENDING: usize = 64;

/// Longest request id recorded. omp's ids are short; a longer one is not
/// kept, so its prompt can still be answered but not granted.
const MAX_ID_BYTES: usize = 256;

pub(super) struct PendingApprovals {
    /// The process's project folder (`None` = started without one, which
    /// only a session-scope grant accepts).
    project_root: Option<PathBuf>,
    /// `(request id, tool)`, oldest first.
    prompts: VecDeque<(String, String)>,
}

impl PendingApprovals {
    pub(super) const fn new(project_root: Option<PathBuf>) -> Self {
        Self {
            project_root,
            prompts: VecDeque::new(),
        }
    }

    /// Track a forwarded approval prompt, or forget one omp cancelled
    /// (`method: "cancel"`, `targetId`). Other frames leave it as it is.
    fn observe(&mut self, frame: &Value) {
        if let Some(tool) = approval::approval_tool_name(frame) {
            let Some(id) = approval::approval_request_id(frame) else {
                return;
            };
            if id.len() > MAX_ID_BYTES {
                return;
            }
            self.remove(id);
            if self.prompts.len() == MAX_PENDING {
                self.prompts.pop_front();
            }
            self.prompts.push_back((id.to_owned(), tool.to_owned()));
        } else if frame.get("method").and_then(Value::as_str) == Some("cancel") {
            if let Some(id) = frame.get("targetId").and_then(Value::as_str) {
                self.remove(id);
            }
        }
    }

    /// The tool of open prompt `id`.
    pub(super) fn tool(&self, id: &str) -> Option<&str> {
        self.prompts
            .iter()
            .find(|(pending, _)| pending == id)
            .map(|(_, tool)| tool.as_str())
    }

    fn remove(&mut self, id: &str) {
        if let Some(at) = self.prompts.iter().position(|(pending, _)| pending == id) {
            self.prompts.remove(at);
        }
    }
}

/// The stdout reader's per-line hook for a frame it forwards to the
/// frontend (a rule-covered prompt it answered itself is never recorded).
/// Locks only for an `extension_ui_request`; a poisoned lock records nothing.
pub(super) fn observe(pending: &Mutex<PendingApprovals>, frame_type: Option<&str>, frame: &Value) {
    if frame_type != Some("extension_ui_request") {
        return;
    }
    if let Ok(mut pending) = pending.lock() {
        pending.observe(frame);
    }
}

/// The request id an `extension_ui_response` command answers, `None` for
/// any other command. `AgentBridge::send` settles that prompt once the
/// answer reached omp: approved or denied, it is no longer one to grant.
pub(super) fn answered_id(command: &Value) -> Option<&str> {
    if command.get("type").and_then(Value::as_str) != Some("extension_ui_response") {
        return None;
    }
    command.get("id").and_then(Value::as_str)
}

/// Forget prompt `id` (answered); a poisoned lock leaves it.
pub(super) fn settle(pending: &Mutex<PendingApprovals>, id: &str) {
    if let Ok(mut pending) = pending.lock() {
        pending.remove(id);
    }
}

impl AgentBridge {
    /// Grant standing approval for the open prompt `request_id` of
    /// `session_id`'s live process: that prompt's tool, under that
    /// process's folder for `RuleScope::Project` — never anything the
    /// caller names. Refused, and the prompt kept, when the process has no
    /// such open prompt or a project grant has no folder; consumed by a
    /// grant that goes ahead (a failed rules-file write then leaves it
    /// consumed — the user answered the prompt in the same click).
    ///
    /// A session grant is made under the `sessions` lock, so it cannot land
    /// after the process was reaped and its grants cleared
    /// (`reap_and_clear_grants` clears after the removal). A project grant
    /// writes its file after the locks are released, so this blocks: call
    /// it off the IPC thread.
    pub fn grant_approval(
        &self,
        rule_book: &RuleBook,
        session_id: &str,
        request_id: &str,
        scope: RuleScope,
    ) -> Result<(), String> {
        let project = self
            .with_entry(session_id, |inner| {
                let mut pending = inner
                    .approvals
                    .lock()
                    .map_err(|_| "approvals lock poisoned".to_string())?;
                let tool = pending.tool(request_id).ok_or_else(|| {
                    format!("session '{session_id}' has no open approval prompt '{request_id}'")
                })?;
                let project = match scope {
                    RuleScope::Session => {
                        rule_book.grant(session_id, None, tool, RuleScope::Session)?;
                        None
                    }
                    RuleScope::Project => {
                        let root = pending.project_root.as_deref().ok_or_else(|| {
                            "project-scoped grant requires a project root".to_string()
                        })?;
                        // Copied out: the rules file is written after both
                        // locks are released.
                        Some((tool.to_owned(), root.to_path_buf()))
                    }
                };
                pending.remove(request_id);
                drop(pending);
                Ok::<_, String>(project)
            })?
            .ok_or_else(|| format!("session '{session_id}' not found"))??;
        match project {
            Some((tool, root)) => {
                rule_book.grant(session_id, Some(&root), &tool, RuleScope::Project)
            }
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::inner::BridgeInner;
    use crate::agent::tests::idle_inner;
    use crate::test_support::TempDir;
    use serde_json::json;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn prompt(id: &str, tool: &str) -> Value {
        json!({
            "type": "extension_ui_request", "id": id, "method": "select",
            "title": format!("Allow tool: {tool}\nCommand: ls"), "options": ["Approve", "Deny"],
        })
    }

    fn pending() -> PendingApprovals {
        PendingApprovals::new(Some(PathBuf::from("/proj")))
    }

    #[test]
    fn a_forwarded_approval_prompt_is_kept_with_its_tool() {
        let mut p = pending();
        p.observe(&json!({
            "type": "extension_ui_request", "id": "r2", "method": "select",
            "title": "Pick one", "options": ["a", "b"],
        }));
        p.observe(&prompt("r1", "bash"));
        assert_eq!(p.tool("r1"), Some("bash"));
        assert_eq!(p.tool("r2"), None, "not an approval prompt");
        assert_eq!(p.tool("other"), None);
    }

    #[test]
    fn omp_cancel_forgets_the_prompt() {
        let mut p = pending();
        p.observe(&prompt("r1", "bash"));
        p.observe(&prompt("r2", "edit"));
        p.observe(&json!({"type": "extension_ui_request", "id": "c", "method": "cancel", "targetId": "r1"}));
        assert_eq!(p.tool("r1"), None);
        assert_eq!(p.tool("r2"), Some("edit"));
    }

    #[test]
    fn a_reused_id_keeps_only_the_latest_prompts_tool() {
        let mut p = pending();
        p.observe(&prompt("r1", "bash"));
        p.observe(&prompt("r1", "edit"));
        assert_eq!(p.tool("r1"), Some("edit"));
        p.remove("r1");
        assert_eq!(p.tool("r1"), None);
    }

    #[test]
    fn the_oldest_prompt_goes_once_the_cap_is_reached() {
        let mut p = pending();
        for i in 0..=MAX_PENDING {
            p.observe(&prompt(&format!("r{i}"), "bash"));
        }
        assert_eq!(p.tool("r0"), None);
        assert!(p.tool("r1").is_some());
        assert!(p.tool(&format!("r{MAX_PENDING}")).is_some());
    }

    #[test]
    fn an_oversized_id_is_not_recorded() {
        let mut p = pending();
        let id = "x".repeat(MAX_ID_BYTES + 1);
        p.observe(&prompt(&id, "bash"));
        assert_eq!(p.tool(&id), None);
    }

    #[test]
    fn only_an_extension_ui_response_answers_a_prompt() {
        assert_eq!(
            answered_id(&json!({"type": "extension_ui_response", "id": "r1", "value": "Deny"})),
            Some("r1")
        );
        assert_eq!(answered_id(&json!({"type": "prompt", "id": "r1"})), None);
        assert_eq!(answered_id(&json!({"type": "extension_ui_response"})), None);
    }

    /// `sess-a` runs in `proj-a` with prompts `req-1`/`req-2` (bash) open;
    /// `sess-b` is another tab with none.
    fn bridge_with_prompts(proj_a: &Path) -> AgentBridge {
        let bridge = AgentBridge::new();
        let open = Mutex::new(PendingApprovals::new(Some(proj_a.to_path_buf())));
        observe(
            &open,
            Some("extension_ui_request"),
            &prompt("req-1", "bash"),
        );
        observe(
            &open,
            Some("extension_ui_request"),
            &prompt("req-2", "bash"),
        );
        let mut sessions = bridge.sessions.lock().unwrap();
        sessions.insert(
            "sess-a".into(),
            BridgeInner {
                approvals: Arc::new(open),
                ..idle_inner(Arc::new(AtomicBool::new(true)))
            },
        );
        sessions.insert("sess-b".into(), idle_inner(Arc::new(AtomicBool::new(true))));
        drop(sessions);
        bridge
    }

    #[test]
    fn a_grant_lands_on_the_asking_process_scope_once() {
        let dir = TempDir::new("approvals", "grant-scope");
        let (proj_a, proj_b) = (dir.path().join("proj-a"), dir.path().join("proj-b"));
        std::fs::create_dir_all(&proj_a).unwrap();
        std::fs::create_dir_all(&proj_b).unwrap();
        let book = RuleBook::new(dir.path().join("rules"));
        let bridge = bridge_with_prompts(&proj_a);

        // The other tab names the same request id: no prompt of its own.
        let err = bridge
            .grant_approval(&book, "sess-b", "req-1", RuleScope::Project)
            .unwrap_err();
        assert!(err.contains("no open approval prompt"), "{err}");
        assert!(!book.is_granted("sess-b", Some(&proj_b), "bash"));

        bridge
            .grant_approval(&book, "sess-a", "req-1", RuleScope::Project)
            .unwrap();
        assert!(book.is_granted("any-session", Some(&proj_a), "bash"));
        assert!(!book.is_granted("sess-b", Some(&proj_b), "bash"));
        let err = bridge
            .grant_approval(&book, "sess-a", "req-1", RuleScope::Session)
            .unwrap_err();
        assert!(err.contains("no open approval prompt"), "{err}");

        bridge
            .grant_approval(&book, "sess-a", "req-2", RuleScope::Session)
            .unwrap();
        assert!(book.is_granted("sess-a", None, "bash"));
        assert!(!book.is_granted("sess-b", None, "bash"));
    }

    #[test]
    fn an_answered_prompt_or_a_respawned_process_grants_nothing() {
        let dir = TempDir::new("approvals", "grant-settled");
        let book = RuleBook::new(dir.path().join("rules"));
        let bridge = bridge_with_prompts(dir.path());
        let approvals = bridge
            .with_entry("sess-a", |inner| Arc::clone(&inner.approvals))
            .unwrap()
            .unwrap();
        settle(&approvals, "req-1");
        let err = bridge
            .grant_approval(&book, "sess-a", "req-1", RuleScope::Session)
            .unwrap_err();
        assert!(err.contains("no open approval prompt"), "{err}");

        // A respawn under the same id: the old process's open prompt is gone.
        bridge
            .sessions
            .lock()
            .unwrap()
            .insert("sess-a".into(), idle_inner(Arc::new(AtomicBool::new(true))));
        let err = bridge
            .grant_approval(&book, "sess-a", "req-2", RuleScope::Session)
            .unwrap_err();
        assert!(err.contains("no open approval prompt"), "{err}");
        assert!(!book.is_granted("sess-a", None, "bash"));

        bridge.sessions.lock().unwrap().remove("sess-a");
        let err = bridge
            .grant_approval(&book, "sess-a", "req-2", RuleScope::Session)
            .unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn a_project_grant_without_a_folder_is_refused_and_keeps_the_prompt() {
        let dir = TempDir::new("approvals", "grant-no-root");
        let book = RuleBook::new(dir.path().join("rules"));
        let bridge = AgentBridge::new();
        let open = Mutex::new(PendingApprovals::new(None));
        observe(
            &open,
            Some("extension_ui_request"),
            &prompt("req-1", "bash"),
        );
        bridge.sessions.lock().unwrap().insert(
            "sess-a".into(),
            BridgeInner {
                approvals: Arc::new(open),
                ..idle_inner(Arc::new(AtomicBool::new(true)))
            },
        );
        let err = bridge
            .grant_approval(&book, "sess-a", "req-1", RuleScope::Project)
            .unwrap_err();
        assert!(err.contains("requires a project root"), "{err}");
        bridge
            .grant_approval(&book, "sess-a", "req-1", RuleScope::Session)
            .unwrap();
        assert!(book.is_granted("sess-a", None, "bash"));
    }
}
