//! Pure scheduler for rail widget plugin runs: decides which plugin runs when,
//! for which tab, and what happens to its result. No threads, no IO, no clock:
//! the host feeds [`Event`]s with the current epoch ms and executes the
//! returned [`Action`]s (spawn, kill, emit). Every rule lives here so unit
//! tests can pin it; `docs/widgets.md` describes the behaviour for users.
//!
//! Model in short: one run per [`RunKey`] (tab session or [`GLOBAL_KEY`] ×
//! plugin) with at most one coalesced follow-up; at most
//! [`MAX_CONCURRENT_RUNS`] processes, the rest wait in FIFO order; nothing
//! starts unless the plugin is enabled, approved, shown, the rail is visible
//! and the plugin matches the session's project. Runnable sets of the active
//! session are `u64` bit masks indexed like the config's plugin list.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use super::config::{PluginEntry, Scope, Trigger, MAX_PLUGINS};

/// Pseudo-session that keys the one shared result of a `global` plugin.
pub const GLOBAL_KEY: &str = "__global__";
pub const MAX_CONCURRENT_RUNS: usize = 4;
/// Plugins past this index are never scheduled: runnable sets are `u64` masks.
const MAX_SCHEDULED: usize = 64;
const _: () = assert!(MAX_PLUGINS <= MAX_SCHEDULED);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunKey {
    /// Tab session id, or [`GLOBAL_KEY`].
    pub scope_key: String,
    pub plugin_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionCtx {
    pub project: String,
    pub branch: String,
    pub remote: String,
}

/// What the user sees: the active tab, rail and window visibility, and the
/// plugins whose card is switched off.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct View {
    pub session: Option<String>,
    pub rail_visible: bool,
    pub window_visible: bool,
    pub hidden: HashSet<String>,
}

/// Result of one run: the plugin's JSON stdout, or an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub result: Result<String, String>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The current config; `approved` holds the ids whose current fingerprint
    /// is approved. The entries are shared with the host's config view.
    Config {
        plugins: Vec<Arc<PluginEntry>>,
        approved: HashSet<String>,
    },
    View(View),
    /// A new session, or fresh context (branch/remote) for a known one.
    SessionOpened {
        session: String,
        ctx: SessionCtx,
    },
    SessionClosed {
        session: String,
    },
    TurnEnd {
        session: String,
    },
    /// Sent after a `SessionOpened` that refreshed the session's context.
    Branch {
        session: String,
        branch: String,
    },
    Manual {
        plugin_id: String,
    },
    Finished {
        key: RunKey,
        seq: u64,
        outcome: Outcome,
    },
    /// The host's clock reached [`Scheduler::next_deadline`].
    Tick,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Spawn the plugin with `session`'s context; report back with `seq`.
    Start {
        key: RunKey,
        seq: u64,
        session: String,
        trigger: Trigger,
    },
    Kill {
        key: RunKey,
    },
    /// The run state of `key` changed.
    Changed {
        key: RunKey,
    },
    /// The run state of `key` was dropped.
    Removed {
        key: RunKey,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    /// Queued for a concurrency slot.
    Waiting,
    Running,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub at: u64,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunState {
    pub phase: Phase,
    /// Identifies the current run; unique across the scheduler, so a result
    /// of a killed run can never be taken for a later run of the same key.
    pub seq: u64,
    /// Trigger of the run in flight, or of the last finished run.
    pub trigger: Option<Trigger>,
    /// Coalesced follow-up (at most one).
    pub queued: Option<Trigger>,
    /// Last finished run; kept while a new one runs so the card can show it dimmed.
    pub last: Option<Finished>,
    pub last_ok_at: Option<u64>,
}

impl RunState {
    const fn idle() -> Self {
        Self {
            phase: Phase::Idle,
            seq: 0,
            trigger: None,
            queued: None,
            last: None,
            last_ok_at: None,
        }
    }
}

#[derive(Debug, Default)]
struct Session {
    ctx: SessionCtx,
    /// Triggers that fired while the session was not live, per plugin id;
    /// flushed (one run per plugin) when it becomes live.
    dirty: HashMap<String, Trigger>,
    /// Last branch seen; a `Branch` event to the same branch is a no-op.
    last_branch: Option<String>,
}

#[derive(Debug, Default)]
struct Config {
    plugins: Vec<Arc<PluginEntry>>,
    /// Plugin id → index into `plugins` (first entry wins on duplicates).
    index: HashMap<String, usize>,
    approved: HashSet<String>,
}

impl Config {
    fn get(&self, id: &str) -> Option<(usize, &PluginEntry)> {
        let &i = self.index.get(id)?;
        self.plugins.get(i).map(|p| (i, &**p))
    }
}

#[derive(Debug, Default)]
struct Table {
    runs: HashMap<RunKey, RunState>,
    /// FIFO of `Waiting` keys; entries whose state is gone are skipped.
    waiting: VecDeque<RunKey>,
    running: usize,
    last_seq: u64,
}

impl Table {
    /// Starts a run of `key` with `session`'s context, coalesces it into the
    /// one in flight, or queues it for a slot.
    fn start(&mut self, key: RunKey, trigger: Trigger, session: &str, actions: &mut Vec<Action>) {
        if !self.runs.contains_key(&key) {
            // First run of this key: the map needs its own copy.
            self.runs.insert(key.clone(), RunState::idle());
        }
        let Some(st) = self.runs.get_mut(&key) else {
            return;
        };
        if st.phase != Phase::Idle {
            // An interval tick never replaces a pending turn-end/branch/manual refresh.
            // Nothing the rail shows changed: no `Changed`.
            st.queued = match st.queued {
                Some(q) if trigger == Trigger::Interval && q != Trigger::Interval => Some(q),
                _ => Some(trigger),
            };
            return;
        }
        self.last_seq += 1;
        st.seq = self.last_seq;
        st.trigger = Some(trigger);
        st.queued = None;
        // Two actions name the key: one copy.
        actions.push(Action::Changed { key: key.clone() });
        if self.running < MAX_CONCURRENT_RUNS {
            st.phase = Phase::Running;
            self.running += 1;
            actions.push(Action::Start {
                key,
                seq: self.last_seq,
                session: session.to_owned(),
                trigger,
            });
        } else {
            st.phase = Phase::Waiting;
            self.waiting.push_back(key);
        }
    }
}

/// Read-only gate checks, borrowed field by field so the run table can be
/// mutated at the same time.
struct Gate<'a> {
    cfg: &'a Config,
    view: &'a View,
    sessions: &'a HashMap<String, Session>,
}

impl<'a> Gate<'a> {
    const fn new(cfg: &'a Config, view: &'a View, sessions: &'a HashMap<String, Session>) -> Self {
        Self {
            cfg,
            view,
            sessions,
        }
    }

    /// enabled ∧ approved ∧ shown ∧ rail visible ∧ session known ∧ match.
    fn runnable(&self, p: &PluginEntry, session: &str) -> bool {
        p.enabled
            && self.view.rail_visible
            && self.cfg.approved.contains(&p.id)
            && !self.view.hidden.contains(&p.id)
            && self
                .sessions
                .get(session)
                .is_some_and(|s| p.matches(&s.ctx.project, &s.ctx.remote))
    }

    fn mask(&self, session: &str) -> u64 {
        self.cfg
            .plugins
            .iter()
            .take(MAX_SCHEDULED)
            .enumerate()
            .filter(|(_, p)| self.runnable(p, session))
            .fold(0, |m, (i, _)| m | bit(i))
    }

    /// The session a run of `key` with `trigger` may start in right now, or
    /// `None`: tab keys use their own (still open) session, global keys the
    /// active one; an interval run also needs a visible window and, for a
    /// tab plugin, its tab to be active.
    fn allowed(&self, key: &RunKey, trigger: Trigger, active: Option<&'a str>) -> Option<&'a str> {
        let (_, p) = self.cfg.get(&key.plugin_id)?;
        let global = key.scope_key == GLOBAL_KEY;
        if global != (p.scope == Scope::Global) {
            return None;
        }
        let session = if global {
            active?
        } else {
            self.sessions.get_key_value(&key.scope_key)?.0.as_str()
        };
        if !self.runnable(p, session) {
            return None;
        }
        if trigger == Trigger::Interval
            && !(self.view.window_visible && (global || active == Some(session)))
        {
            return None;
        }
        Some(session)
    }
}

const fn bit(i: usize) -> u64 {
    if i < MAX_SCHEDULED {
        1 << i
    } else {
        0
    }
}

/// Indices of the set bits, lowest first.
fn bits(mut mask: u64) -> impl Iterator<Item = usize> {
    std::iter::from_fn(move || {
        if mask == 0 {
            return None;
        }
        let i = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        Some(i)
    })
}

fn key_for(p: &PluginEntry, session: &str) -> RunKey {
    let scope_key = match p.scope {
        Scope::Tab => session,
        Scope::Global => GLOBAL_KEY,
    };
    RunKey {
        scope_key: scope_key.to_owned(),
        plugin_id: p.id.clone(),
    }
}

/// Whether an interval plugin is due: never finished (and not in flight), or
/// its last run finished at least `interval` ago. A run in flight is fresh.
fn due_now(run: Option<&RunState>, interval: u64, now: u64) -> bool {
    match run {
        None => true,
        Some(r) if r.phase != Phase::Idle => false,
        Some(r) => r
            .last
            .as_ref()
            .is_none_or(|f| now.saturating_sub(f.at) >= interval),
    }
}

/// Re-arms plugin `i`'s interval timer one interval from `now`; a no-op for a
/// plugin without one.
fn rearm(timers: &mut [Option<u64>], i: usize, p: &PluginEntry, now: u64) {
    if let (Some(due), Some(iv)) = (timers.get_mut(i), p.interval_ms) {
        *due = Some(now.saturating_add(iv));
    }
}

/// Whether a plugin gets an interval timer: tab timers only exist while a
/// tab is active.
fn timer_wanted(p: &PluginEntry, approved: bool, has_active: bool) -> bool {
    p.interval_ms.is_some() && p.enabled && approved && (p.scope == Scope::Global || has_active)
}

#[derive(Debug, Default)]
pub struct Scheduler {
    cfg: Config,
    /// As last reported; `session` may name a session not opened yet.
    view: View,
    /// The effective active session: `view.session` once it is known.
    active: Option<String>,
    sessions: HashMap<String, Session>,
    table: Table,
    /// Next interval deadline per plugin index (aligned with `cfg.plugins`).
    timers: Vec<Option<u64>>,
    /// Plugins runnable for `active` as of the last refresh.
    active_mask: u64,
    /// `window_visible && rail_visible` as of the last refresh.
    live: bool,
}

impl Scheduler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle(&mut self, ev: Event, now_ms: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        self.clamp_timers(now_ms);
        match ev {
            Event::Config { plugins, approved } => {
                self.set_config(plugins, approved, now_ms, &mut actions);
            }
            Event::View(view) => {
                self.view = view;
                self.refresh(now_ms, &mut actions);
            }
            Event::SessionOpened { session, ctx } => {
                if let Some(s) = self.sessions.get_mut(&session) {
                    s.ctx = ctx;
                } else {
                    // The first branch seen is only recorded.
                    let last_branch = (!ctx.branch.is_empty()).then(|| ctx.branch.clone());
                    let s = Session {
                        ctx,
                        dirty: HashMap::new(),
                        last_branch,
                    };
                    self.sessions.insert(session, s);
                }
                self.refresh(now_ms, &mut actions);
            }
            Event::SessionClosed { session } => self.close(&session, now_ms, &mut actions),
            Event::TurnEnd { session } => self.fire(&session, Trigger::TurnEnd, &mut actions),
            Event::Branch { session, branch } => {
                let Some(s) = self.sessions.get_mut(&session) else {
                    return actions;
                };
                let changed = s.last_branch.as_ref().is_some_and(|b| *b != branch);
                s.last_branch = Some(branch);
                if changed {
                    self.fire(&session, Trigger::Branch, &mut actions);
                }
            }
            Event::Manual { plugin_id } => self.manual(&plugin_id, &mut actions),
            Event::Finished { key, seq, outcome } => {
                self.finished(key, seq, outcome, now_ms, &mut actions);
            }
            Event::Tick => self.tick(now_ms, &mut actions),
        }
        actions
    }

    /// Earliest interval deadline; the host sends [`Event::Tick`] once its clock reaches it.
    pub fn next_deadline(&self) -> Option<u64> {
        self.timers.iter().flatten().copied().min()
    }

    pub fn run(&self, key: &RunKey) -> Option<&RunState> {
        self.table.runs.get(key)
    }

    pub fn session_ctx(&self, session: &str) -> Option<&SessionCtx> {
        self.sessions.get(session).map(|s| &s.ctx)
    }

    /// The active session and, in config order, the plugins that apply to it
    /// (enabled ∧ approved ∧ match), regardless of rail/window visibility and
    /// hidden cards. Empty without an active session.
    pub fn active_applicable(&self) -> (Option<&str>, Vec<&str>) {
        let active = self.active.as_deref();
        let Some(ctx) = active.and_then(|s| self.sessions.get(s)).map(|s| &s.ctx) else {
            return (active, Vec::new());
        };
        let ids = self
            .cfg
            .plugins
            .iter()
            .filter(|p| {
                p.enabled
                    && self.cfg.approved.contains(&p.id)
                    && p.matches(&ctx.project, &ctx.remote)
            })
            .map(|p| p.id.as_str())
            .collect();
        (active, ids)
    }

    /// A clock that jumped back must not push a timer further away than one interval.
    fn clamp_timers(&mut self, now: u64) {
        for (due, p) in self.timers.iter_mut().zip(&self.cfg.plugins) {
            if let (Some(d), Some(iv)) = (due.as_mut(), p.interval_ms) {
                *d = (*d).min(now.saturating_add(iv));
            }
        }
    }

    /// Re-derives the active session and its runnable set; activates plugins
    /// that became runnable and catches up when the view became live.
    fn refresh(&mut self, now: u64, actions: &mut Vec<Action>) {
        let active = self
            .view
            .session
            .as_ref()
            .filter(|s| self.sessions.contains_key(s.as_str()));
        let switched = active != self.active.as_ref();
        if switched {
            // Once per tab switch.
            self.active = active.cloned();
        }
        let gate = Gate::new(&self.cfg, &self.view, &self.sessions);
        let mask = self.active.as_deref().map_or(0, |s| gate.mask(s));
        let newly = if switched {
            let has_active = self.active.is_some();
            for (due, p) in self.timers.iter_mut().zip(&self.cfg.plugins) {
                if p.scope == Scope::Tab {
                    *due = p
                        .interval_ms
                        .filter(|_| timer_wanted(p, self.cfg.approved.contains(&p.id), has_active))
                        .map(|iv| now.saturating_add(iv));
                }
            }
            mask
        } else {
            mask & !self.active_mask
        };
        self.active_mask = mask;
        self.activate(newly, now, actions);
        let live = self.view.window_visible && self.view.rail_visible;
        if live && !self.live {
            self.flush_dirty(actions);
            self.catch_up(now, actions);
        }
        self.live = live;
    }

    /// Runs the plugins in `newly` for the active session: tab plugins with
    /// `activate`, or else their dirty trigger; global plugins with
    /// `activate` only without a fresh result.
    fn activate(&mut self, newly: u64, now: u64, actions: &mut Vec<Action>) {
        let Some(session) = self.active.as_deref() else {
            return;
        };
        let mut sess = self.sessions.get_mut(session);
        for i in bits(newly) {
            let Some(p) = self.cfg.plugins.get(i) else {
                continue;
            };
            let key = key_for(p, session);
            let trigger = match p.scope {
                Scope::Tab => {
                    let dirty = sess.as_mut().and_then(|s| s.dirty.remove(&p.id));
                    rearm(&mut self.timers, i, p, now);
                    if p.wants(Trigger::Activate) {
                        Some(Trigger::Activate)
                    } else {
                        dirty
                    }
                }
                Scope::Global => {
                    let due = p
                        .interval_ms
                        .is_none_or(|iv| due_now(self.table.runs.get(&key), iv, now));
                    (p.wants(Trigger::Activate) && due).then_some(Trigger::Activate)
                }
            };
            let Some(trigger) = trigger else {
                continue;
            };
            if p.scope == Scope::Global {
                rearm(&mut self.timers, i, p, now);
            }
            self.table.start(key, trigger, session, actions);
        }
    }

    /// Runs what fired while the active session was not live.
    fn flush_dirty(&mut self, actions: &mut Vec<Action>) {
        let Some(session) = self.active.as_deref() else {
            return;
        };
        let Some(sess) = self.sessions.get_mut(session) else {
            return;
        };
        if sess.dirty.is_empty() {
            return;
        }
        for i in bits(self.active_mask) {
            let Some(p) = self.cfg.plugins.get(i) else {
                continue;
            };
            if p.scope != Scope::Tab {
                continue;
            }
            if let Some(trigger) = sess.dirty.remove(&p.id) {
                self.table
                    .start(key_for(p, session), trigger, session, actions);
            }
        }
    }

    /// Each runnable interval plugin that is due runs once and is re-armed.
    fn catch_up(&mut self, now: u64, actions: &mut Vec<Action>) {
        let Some(session) = self.active.as_deref() else {
            return;
        };
        for i in bits(self.active_mask) {
            let Some(p) = self.cfg.plugins.get(i) else {
                continue;
            };
            let Some(iv) = p.interval_ms else {
                continue;
            };
            let key = key_for(p, session);
            if !due_now(self.table.runs.get(&key), iv, now) {
                continue;
            }
            rearm(&mut self.timers, i, p, now);
            self.table.start(key, Trigger::Interval, session, actions);
        }
    }

    /// `turn_end`/`branch` of `session`: runs now when it is the live active
    /// session, otherwise remembered for its next activation.
    fn fire(&mut self, session: &str, trigger: Trigger, actions: &mut Vec<Action>) {
        if self.live && self.active.as_deref() == Some(session) {
            for i in bits(self.active_mask) {
                let Some(p) = self.cfg.plugins.get(i) else {
                    continue;
                };
                if p.wants(trigger) {
                    self.table
                        .start(key_for(p, session), trigger, session, actions);
                }
            }
        } else if let Some(sess) = self.sessions.get_mut(session) {
            for p in &self.cfg.plugins {
                if !(p.enabled && p.scope == Scope::Tab && p.wants(trigger)) {
                    continue;
                }
                if let Some(t) = sess.dirty.get_mut(&p.id) {
                    *t = trigger;
                } else {
                    sess.dirty.insert(p.id.clone(), trigger);
                }
            }
        }
    }

    fn manual(&mut self, plugin_id: &str, actions: &mut Vec<Action>) {
        let Some(session) = self.active.as_deref() else {
            return;
        };
        let Some((i, p)) = self.cfg.get(plugin_id) else {
            return;
        };
        if self.active_mask & bit(i) != 0 {
            self.table
                .start(key_for(p, session), Trigger::Manual, session, actions);
        }
    }

    fn tick(&mut self, now: u64, actions: &mut Vec<Action>) {
        let session = if self.live {
            self.active.as_deref()
        } else {
            None
        };
        for (i, (due, p)) in self.timers.iter_mut().zip(&self.cfg.plugins).enumerate() {
            let (Some(d), Some(iv)) = (*due, p.interval_ms) else {
                continue;
            };
            if d > now {
                continue;
            }
            *due = Some(now.saturating_add(iv));
            if let Some(session) = session {
                if self.active_mask & bit(i) != 0 {
                    self.table
                        .start(key_for(p, session), Trigger::Interval, session, actions);
                }
            }
        }
    }

    fn finished(
        &mut self,
        key: RunKey,
        seq: u64,
        outcome: Outcome,
        now: u64,
        actions: &mut Vec<Action>,
    ) {
        let Some(st) = self.table.runs.get_mut(&key) else {
            return;
        };
        if st.seq != seq || st.phase != Phase::Running {
            return;
        }
        st.phase = Phase::Idle;
        if outcome.result.is_ok() {
            st.last_ok_at = Some(now);
        }
        st.last = Some(Finished { at: now, outcome });
        let queued = st.queued.take();
        self.table.running = self.table.running.saturating_sub(1);
        // The key is still needed for the follow-up.
        actions.push(Action::Changed { key: key.clone() });
        self.promote(actions);
        let Some(trigger) = queued else {
            return;
        };
        let gate = Gate::new(&self.cfg, &self.view, &self.sessions);
        if let Some(session) = gate.allowed(&key, trigger, self.active.as_deref()) {
            self.table.start(key, trigger, session, actions);
        }
    }

    /// Fills free slots from the waiting FIFO; a waiting run that may no
    /// longer start (tab closed, hidden, unapproved, …) goes back to idle.
    fn promote(&mut self, actions: &mut Vec<Action>) {
        let gate = Gate::new(&self.cfg, &self.view, &self.sessions);
        let active = self.active.as_deref();
        while self.table.running < MAX_CONCURRENT_RUNS {
            let Some(key) = self.table.waiting.pop_front() else {
                break;
            };
            let Some(st) = self.table.runs.get_mut(&key) else {
                continue;
            };
            if st.phase != Phase::Waiting {
                continue;
            }
            let trigger = st.trigger.unwrap_or(Trigger::Manual);
            if let Some(session) = gate.allowed(&key, trigger, active) {
                st.phase = Phase::Running;
                self.table.running += 1;
                let seq = st.seq;
                // Two actions name the key: one copy.
                actions.push(Action::Changed { key: key.clone() });
                actions.push(Action::Start {
                    key,
                    seq,
                    session: session.to_owned(),
                    trigger,
                });
            } else {
                st.phase = Phase::Idle;
                st.queued = None;
                actions.push(Action::Changed { key });
            }
        }
    }

    /// Drops every run state whose key matches `pred`; runs in flight or
    /// waiting are killed first.
    fn purge(&mut self, pred: impl Fn(&RunKey) -> bool, actions: &mut Vec<Action>) {
        let mut had_waiting = false;
        for (key, st) in self.table.runs.extract_if(|k, _| pred(k)) {
            match st.phase {
                Phase::Idle => {}
                Phase::Waiting => {
                    had_waiting = true;
                    actions.push(Action::Kill { key: key.clone() });
                }
                Phase::Running => {
                    self.table.running = self.table.running.saturating_sub(1);
                    actions.push(Action::Kill { key: key.clone() });
                }
            }
            actions.push(Action::Removed { key });
        }
        if had_waiting {
            let runs = &self.table.runs;
            self.table.waiting.retain(|k| runs.contains_key(k));
        }
    }

    fn close(&mut self, session: &str, now: u64, actions: &mut Vec<Action>) {
        if self.sessions.remove(session).is_none() {
            return;
        }
        if self.view.session.as_deref() == Some(session) {
            self.view.session = None;
        }
        self.purge(|k| k.scope_key == session, actions);
        self.refresh(now, actions);
        self.promote(actions);
    }

    fn set_config(
        &mut self,
        plugins: Vec<Arc<PluginEntry>>,
        approved: HashSet<String>,
        now: u64,
        actions: &mut Vec<Action>,
    ) {
        let mut index = HashMap::with_capacity(plugins.len());
        for (i, p) in plugins.iter().enumerate() {
            index.entry(p.id.clone()).or_insert(i);
        }
        // Removed, unapproved, or scope changed: the old results are void.
        self.purge(
            |k| {
                index.get(&k.plugin_id).is_none_or(|&i| {
                    !approved.contains(&k.plugin_id)
                        || plugins.get(i).is_none_or(|p| {
                            (p.scope == Scope::Global) != (k.scope_key == GLOBAL_KEY)
                        })
                })
            },
            actions,
        );
        // Carry timers and the runnable set over by id.
        let has_active = self.active.is_some();
        let mut timers = Vec::with_capacity(plugins.len());
        let mut mask = 0;
        for (i, p) in plugins.iter().enumerate() {
            let now_approved = approved.contains(&p.id);
            let old = self.cfg.get(&p.id);
            let kept_due = old.and_then(|(oi, op)| {
                let same = op.interval_ms == p.interval_ms
                    && op.match_path == p.match_path
                    && op.match_remote == p.match_remote
                    && op.scope == p.scope
                    && op.enabled == p.enabled
                    && self.cfg.approved.contains(&p.id) == now_approved;
                if same {
                    self.timers.get(oi).copied().flatten()
                } else {
                    None
                }
            });
            let due = p
                .interval_ms
                .filter(|_| timer_wanted(p, now_approved, has_active))
                .map(|iv| kept_due.unwrap_or_else(|| now.saturating_add(iv)));
            timers.push(due);
            // A scope change starts over: it lost its result above.
            if let Some((oi, op)) = old {
                if op.scope == p.scope && self.active_mask & bit(oi) != 0 {
                    mask |= bit(i);
                }
            }
        }
        for s in self.sessions.values_mut() {
            s.dirty.retain(|id, _| index.contains_key(id));
        }
        self.cfg = Config {
            plugins,
            index,
            approved,
        };
        self.timers = timers;
        self.active_mask = mask;
        self.promote(actions);
        self.refresh(now, actions);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Trigger::{Activate, Branch, Interval, Manual, TurnEnd};

    const MIN: u64 = 60_000;

    fn plugin(
        id: &str,
        scope: Scope,
        triggers: &[Trigger],
        interval_ms: Option<u64>,
    ) -> PluginEntry {
        PluginEntry {
            id: id.into(),
            title: id.into(),
            command: vec!["x".into()],
            match_path: None,
            match_remote: None,
            triggers: triggers.to_vec(),
            timeout_ms: 1_000,
            interval_ms,
            enabled: true,
            scope,
        }
    }

    fn tab(id: &str) -> PluginEntry {
        plugin(id, Scope::Tab, &[Activate, TurnEnd, Branch], None)
    }

    fn global(id: &str) -> PluginEntry {
        plugin(id, Scope::Global, &[Activate, TurnEnd, Branch], None)
    }

    fn key(scope_key: &str, plugin_id: &str) -> RunKey {
        RunKey {
            scope_key: scope_key.into(),
            plugin_id: plugin_id.into(),
        }
    }

    fn ok() -> Outcome {
        Outcome {
            result: Ok("{}".into()),
            duration_ms: 3,
        }
    }

    fn err(msg: &str) -> Outcome {
        Outcome {
            result: Err(msg.into()),
            duration_ms: 3,
        }
    }

    /// `(scope_key, plugin_id, session, trigger)` of every `Start`.
    fn starts(actions: &[Action]) -> Vec<(&str, &str, &str, Trigger)> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Start {
                    key,
                    session,
                    trigger,
                    ..
                } => Some((
                    key.scope_key.as_str(),
                    key.plugin_id.as_str(),
                    session.as_str(),
                    *trigger,
                )),
                _ => None,
            })
            .collect()
    }

    fn kills(actions: &[Action]) -> Vec<(&str, &str)> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Kill { key } => Some((key.scope_key.as_str(), key.plugin_id.as_str())),
                _ => None,
            })
            .collect()
    }

    fn removed(actions: &[Action]) -> Vec<(&str, &str)> {
        let mut out: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::Removed { key } => Some((key.scope_key.as_str(), key.plugin_id.as_str())),
                _ => None,
            })
            .collect();
        out.sort_unstable();
        out
    }

    fn approved_all(plugins: &[PluginEntry]) -> HashSet<String> {
        plugins.iter().map(|p| p.id.clone()).collect()
    }

    /// `plugins` as the host hands them over: shared entries.
    fn shared(plugins: Vec<PluginEntry>) -> Vec<Arc<PluginEntry>> {
        plugins.into_iter().map(Arc::new).collect()
    }

    /// A scheduler with a clock: rail and window visible, every plugin approved.
    struct H {
        s: Scheduler,
        now: u64,
        view: View,
    }

    impl H {
        fn new(plugins: Vec<PluginEntry>) -> Self {
            let mut h = Self {
                s: Scheduler::new(),
                now: 1_000_000,
                view: View {
                    session: None,
                    rail_visible: true,
                    window_visible: true,
                    hidden: HashSet::new(),
                },
            };
            h.config(plugins);
            h.push_view();
            h
        }

        fn ev(&mut self, ev: Event) -> Vec<Action> {
            self.s.handle(ev, self.now)
        }

        fn config(&mut self, plugins: Vec<PluginEntry>) -> Vec<Action> {
            let approved = approved_all(&plugins);
            self.ev(Event::Config {
                plugins: shared(plugins),
                approved,
            })
        }

        fn push_view(&mut self) -> Vec<Action> {
            // `H` keeps its view for the next tweak; the event owns a copy.
            let v = self.view.clone();
            self.ev(Event::View(v))
        }

        fn open(&mut self, session: &str, project: &str) -> Vec<Action> {
            self.open_ctx(session, project, "", "main")
        }

        fn open_ctx(
            &mut self,
            session: &str,
            project: &str,
            remote: &str,
            branch: &str,
        ) -> Vec<Action> {
            self.ev(Event::SessionOpened {
                session: session.into(),
                ctx: SessionCtx {
                    project: project.into(),
                    branch: branch.into(),
                    remote: remote.into(),
                },
            })
        }

        fn activate(&mut self, session: Option<&str>) -> Vec<Action> {
            self.view.session = session.map(String::from);
            self.push_view()
        }

        fn seq(&self, k: &RunKey) -> u64 {
            self.s.run(k).expect("run state").seq
        }

        fn phase(&self, k: &RunKey) -> Phase {
            self.s.run(k).expect("run state").phase
        }

        /// Finishes the current run of `k`.
        fn finish(&mut self, k: &RunKey, outcome: Outcome) -> Vec<Action> {
            let seq = self.seq(k);
            self.finish_seq(k, seq, outcome)
        }

        fn finish_seq(&mut self, k: &RunKey, seq: u64, outcome: Outcome) -> Vec<Action> {
            self.ev(Event::Finished {
                // The event owns its key; callers keep theirs for asserts.
                key: k.clone(),
                seq,
                outcome,
            })
        }

        fn turn_end(&mut self, session: &str) -> Vec<Action> {
            self.ev(Event::TurnEnd {
                session: session.into(),
            })
        }

        fn branch(&mut self, session: &str, branch: &str) -> Vec<Action> {
            self.ev(Event::Branch {
                session: session.into(),
                branch: branch.into(),
            })
        }

        fn manual(&mut self, id: &str) -> Vec<Action> {
            self.ev(Event::Manual {
                plugin_id: id.into(),
            })
        }

        /// Advances the clock to the next deadline and ticks.
        fn tick_to_deadline(&mut self) -> Vec<Action> {
            self.now = self.s.next_deadline().expect("a timer");
            self.ev(Event::Tick)
        }

        /// Opens and activates `s1` (project `/p`) and finishes the activation runs.
        fn settled(plugins: Vec<PluginEntry>) -> Self {
            let mut h = Self::new(plugins);
            h.open("s1", "/p");
            let a = h.activate(Some("s1"));
            let keys: Vec<RunKey> = a
                .iter()
                .filter_map(|a| match a {
                    // Copied out: `finish` below needs `h` mutably.
                    Action::Start { key, .. } => Some(key.clone()),
                    _ => None,
                })
                .collect();
            for k in &keys {
                h.finish(k, ok());
            }
            h
        }
    }

    // ── start / finish / coalescing ───────────────────────────────────────

    #[test]
    fn activation_starts_a_run_with_a_fresh_seq() {
        let mut h = H::new(vec![tab("git")]);
        h.open("s1", "/p");
        let a = h.activate(Some("s1"));
        assert_eq!(starts(&a), vec![("s1", "git", "s1", Activate)]);
        let st = h.s.run(&key("s1", "git")).expect("state");
        assert_eq!(st.phase, Phase::Running);
        assert_eq!(st.trigger, Some(Activate));
        assert_eq!(st.seq, 1);
        assert!(a.contains(&Action::Changed {
            key: key("s1", "git")
        }));
    }

    #[test]
    fn triggers_while_running_coalesce_and_latest_wins() {
        let mut h = H::new(vec![tab("git")]);
        h.open("s1", "/p");
        h.activate(Some("s1"));
        assert_eq!(starts(&h.turn_end("s1")), vec![]);
        assert_eq!(starts(&h.manual("git")), vec![]);
        let st = h.s.run(&key("s1", "git")).expect("state");
        assert_eq!(st.queued, Some(Manual), "latest trigger wins");
        assert_eq!(st.seq, 1, "no second run");
    }

    #[test]
    fn finish_runs_the_queued_follow_up_once() {
        let mut h = H::new(vec![tab("git")]);
        h.open("s1", "/p");
        h.activate(Some("s1"));
        h.branch("s1", "dev");
        let a = h.finish(&key("s1", "git"), ok());
        assert_eq!(starts(&a), vec![("s1", "git", "s1", Branch)]);
        let st = h.s.run(&key("s1", "git")).expect("state");
        assert_eq!(st.queued, None);
        assert_eq!(st.phase, Phase::Running);
        assert_eq!(st.last.as_ref().map(|f| &f.outcome), Some(&ok()));
        let a = h.finish(&key("s1", "git"), ok());
        assert_eq!(starts(&a), vec![], "no second follow-up");
    }

    #[test]
    fn interval_never_overwrites_a_queued_non_interval_trigger() {
        let mut h = H::new(vec![plugin(
            "t",
            Scope::Tab,
            &[Activate, TurnEnd],
            Some(MIN),
        )]);
        h.open("s1", "/p");
        h.activate(Some("s1"));
        h.turn_end("s1");
        h.tick_to_deadline();
        let k = key("s1", "t");
        assert_eq!(h.s.run(&k).expect("state").queued, Some(TurnEnd));
        h.manual("t");
        assert_eq!(
            h.s.run(&k).expect("state").queued,
            Some(Manual),
            "non-interval still replaces"
        );
    }

    #[test]
    fn interval_queues_when_nothing_else_is_queued() {
        let mut h = H::new(vec![plugin("t", Scope::Tab, &[Activate], Some(MIN))]);
        h.open("s1", "/p");
        h.activate(Some("s1"));
        h.tick_to_deadline();
        assert_eq!(
            h.s.run(&key("s1", "t")).expect("state").queued,
            Some(Interval)
        );
    }

    #[test]
    fn stale_or_unknown_results_are_ignored() {
        let mut h = H::settled(vec![tab("git")]);
        let k = key("s1", "git");
        let old = h.seq(&k);
        h.manual("git");
        let fresh = h.seq(&k);
        assert_ne!(old, fresh);
        let a = h.finish_seq(&k, old, err("old"));
        assert_eq!(a, vec![]);
        assert_eq!(h.phase(&k), Phase::Running, "old seq must not overwrite");
        let a = h.ev(Event::Finished {
            key: key("s9", "git"),
            seq: fresh,
            outcome: ok(),
        });
        assert_eq!(a, vec![], "unknown key");
        h.finish(&k, ok());
        assert_eq!(h.phase(&k), Phase::Idle);
        let a = h.finish_seq(&k, fresh, err("dup"));
        assert_eq!(a, vec![], "an idle run takes no result");
        assert_eq!(
            h.s.run(&k)
                .and_then(|r| r.last.as_ref())
                .map(|f| &f.outcome),
            Some(&ok())
        );
    }

    #[test]
    fn a_late_result_lands_in_its_own_session() {
        let mut h = H::new(vec![tab("git")]);
        h.open("s1", "/p");
        h.open("s2", "/q");
        h.activate(Some("s1"));
        h.activate(Some("s2"));
        assert_eq!(h.phase(&key("s2", "git")), Phase::Running);
        h.finish(&key("s1", "git"), err("boom"));
        let s1 = h.s.run(&key("s1", "git")).expect("s1");
        assert_eq!(s1.last.as_ref().map(|f| &f.outcome), Some(&err("boom")));
        assert_eq!(h.phase(&key("s2", "git")), Phase::Running);
    }

    #[test]
    fn errors_keep_the_last_ok_time_and_the_previous_result_stays_while_running() {
        let mut h = H::new(vec![tab("git")]);
        h.open("s1", "/p");
        h.activate(Some("s1"));
        let k = key("s1", "git");
        let t1 = h.now;
        h.finish(&k, ok());
        h.now += 1_000;
        h.manual("git");
        let st = h.s.run(&k).expect("state");
        assert_eq!(
            st.last.as_ref().map(|f| f.at),
            Some(t1),
            "kept while running"
        );
        h.finish(&k, err("x"));
        let st = h.s.run(&k).expect("state");
        assert_eq!(st.last_ok_at, Some(t1));
        assert_eq!(st.last.as_ref().map(|f| f.at), Some(t1 + 1_000));
        assert_eq!(st.trigger, Some(Manual), "trigger of the last finished run");
    }

    // ── sessions ──────────────────────────────────────────────────────────

    #[test]
    fn session_close_kills_and_removes_its_runs_only() {
        let mut h = H::new(vec![tab("t"), global("g")]);
        h.open("s1", "/p");
        h.open("s2", "/q");
        h.activate(Some("s2"));
        h.finish(&key("s2", "t"), ok());
        h.finish(&key(GLOBAL_KEY, "g"), ok());
        h.activate(Some("s1"));
        // s1: t running, g fresh (no interval → also runs).
        let a = h.ev(Event::SessionClosed {
            session: "s1".into(),
        });
        assert_eq!(kills(&a), vec![("s1", "t")]);
        assert_eq!(removed(&a), vec![("s1", "t")]);
        assert!(h.s.run(&key("s1", "t")).is_none());
        assert!(h.s.run(&key("s2", "t")).is_some());
        assert!(h.s.run(&key(GLOBAL_KEY, "g")).is_some(), "global survives");
        assert_eq!(h.s.session_ctx("s1"), None);
        assert_eq!(h.s.active_applicable(), (None, vec![]));
    }

    #[test]
    fn closing_the_active_session_stops_everything_until_another_is_active() {
        let mut h = H::settled(vec![plugin(
            "t",
            Scope::Tab,
            &[Activate, TurnEnd],
            Some(MIN),
        )]);
        h.ev(Event::SessionClosed {
            session: "s1".into(),
        });
        assert_eq!(
            h.s.next_deadline(),
            None,
            "tab timers die with the active tab"
        );
        assert_eq!(starts(&h.manual("t")), vec![]);
        assert_eq!(starts(&h.turn_end("s1")), vec![]);
        // A view naming the closed session waits for it to be opened again.
        assert_eq!(starts(&h.activate(Some("s1"))), vec![]);
        assert_eq!(
            starts(&h.open("s1", "/p")),
            vec![("s1", "t", "s1", Activate)],
            "a re-opened id is a fresh session"
        );
    }

    #[test]
    fn follow_up_is_suppressed_for_a_closed_tab() {
        let mut h = H::new(vec![tab("t")]);
        h.open("s1", "/p");
        h.open("s2", "/q");
        h.activate(Some("s1"));
        h.turn_end("s1");
        let seq = h.seq(&key("s1", "t"));
        h.ev(Event::SessionClosed {
            session: "s1".into(),
        });
        let a = h.ev(Event::Finished {
            key: key("s1", "t"),
            seq,
            outcome: ok(),
        });
        assert_eq!(a, vec![]);
    }

    #[test]
    fn unknown_sessions_are_ignored() {
        let mut h = H::new(vec![tab("t")]);
        assert_eq!(h.turn_end("nope"), vec![]);
        assert_eq!(h.branch("nope", "x"), vec![]);
        assert_eq!(
            h.ev(Event::SessionClosed {
                session: "nope".into()
            }),
            vec![]
        );
        assert_eq!(
            starts(&h.activate(Some("nope"))),
            vec![],
            "view on an unknown session"
        );
        assert_eq!(h.manual("t"), vec![]);
        assert_eq!(h.s.table.runs.len(), 0);
    }

    #[test]
    fn a_view_sent_before_session_opened_activates_once_it_opens() {
        let mut h = H::new(vec![tab("t")]);
        assert_eq!(starts(&h.activate(Some("s1"))), vec![]);
        assert_eq!(
            starts(&h.open("s1", "/p")),
            vec![("s1", "t", "s1", Activate)]
        );
    }

    // ── activation ────────────────────────────────────────────────────────

    #[test]
    fn switching_tabs_activates_the_new_tab() {
        let mut h = H::settled(vec![tab("a"), plugin("b", Scope::Tab, &[TurnEnd], None)]);
        h.open("s2", "/q");
        let a = h.activate(Some("s2"));
        assert_eq!(
            starts(&a),
            vec![("s2", "a", "s2", Activate)],
            "b does not want activate"
        );
        h.finish(&key("s2", "a"), ok());
        let a = h.activate(Some("s1"));
        assert_eq!(
            starts(&a),
            vec![("s1", "a", "s1", Activate)],
            "back again re-activates"
        );
    }

    #[test]
    fn same_view_again_starts_nothing() {
        let mut h = H::settled(vec![tab("a")]);
        assert_eq!(h.push_view(), vec![]);
        assert_eq!(
            h.open("s1", "/p"),
            vec![],
            "ctx refresh without match change"
        );
    }

    #[test]
    fn global_plugins_run_on_activation_only_without_a_fresh_result() {
        let mut h = H::new(vec![
            plugin("gi", Scope::Global, &[Activate], Some(MIN)),
            plugin("gn", Scope::Global, &[Activate], None),
            plugin("gx", Scope::Global, &[TurnEnd], Some(MIN)),
        ]);
        h.open("s1", "/p");
        h.open("s2", "/q");
        let a = h.activate(Some("s1"));
        assert_eq!(
            starts(&a),
            vec![
                (GLOBAL_KEY, "gi", "s1", Activate),
                (GLOBAL_KEY, "gn", "s1", Activate)
            ],
            "gx does not want activate"
        );
        // First run still in flight: a tab switch skips gi, gn coalesces.
        let a = h.activate(Some("s2"));
        assert_eq!(starts(&a), vec![]);
        assert_eq!(h.s.run(&key(GLOBAL_KEY, "gi")).expect("gi").queued, None);
        assert_eq!(
            h.s.run(&key(GLOBAL_KEY, "gn")).expect("gn").queued,
            Some(Activate)
        );
        h.finish(&key(GLOBAL_KEY, "gi"), ok());
        let a = h.finish(&key(GLOBAL_KEY, "gn"), ok());
        assert_eq!(
            starts(&a),
            vec![(GLOBAL_KEY, "gn", "s2", Activate)],
            "follow-up uses the active tab"
        );
        h.finish(&key(GLOBAL_KEY, "gn"), ok());
        h.now += MIN - 1;
        let a = h.activate(Some("s1"));
        assert_eq!(
            starts(&a),
            vec![(GLOBAL_KEY, "gn", "s1", Activate)],
            "fresh gi skipped; no interval = always"
        );
        h.finish(&key(GLOBAL_KEY, "gn"), ok());
        h.now += 1;
        let a = h.activate(Some("s2"));
        assert_eq!(
            starts(&a),
            vec![
                (GLOBAL_KEY, "gi", "s2", Activate),
                (GLOBAL_KEY, "gn", "s2", Activate)
            ],
            "stale gi runs"
        );
    }

    #[test]
    fn a_global_result_is_shared_across_tabs() {
        let mut h = H::new(vec![plugin("g", Scope::Global, &[Activate], Some(MIN))]);
        h.open("s1", "/p");
        h.open("s2", "/q");
        h.activate(Some("s1"));
        h.finish(&key(GLOBAL_KEY, "g"), ok());
        h.activate(Some("s2"));
        let st = h.s.run(&key(GLOBAL_KEY, "g")).expect("shared");
        assert_eq!(st.last.as_ref().map(|f| f.at), Some(h.now));
        assert_eq!(h.s.table.runs.len(), 1);
        assert_eq!(
            starts(&h.manual("g")),
            vec![(GLOBAL_KEY, "g", "s2", Manual)],
            "refresh from another tab"
        );
    }

    #[test]
    fn hiding_or_showing_one_plugin_runs_only_that_plugin() {
        let mut h = H::settled(vec![tab("a"), tab("b"), global("g")]);
        h.view.hidden.insert("b".into());
        assert_eq!(h.push_view(), vec![], "hiding starts nothing");
        assert_eq!(starts(&h.manual("b")), vec![], "hidden = no runs");
        h.view.hidden.clear();
        assert_eq!(starts(&h.push_view()), vec![("s1", "b", "s1", Activate)]);
    }

    #[test]
    fn approving_one_plugin_runs_only_that_plugin() {
        let mut h = H::new(Vec::new());
        h.ev(Event::Config {
            plugins: shared(vec![tab("a"), tab("b")]),
            approved: HashSet::from(["a".to_string()]),
        });
        h.open("s1", "/p");
        assert_eq!(
            starts(&h.activate(Some("s1"))),
            vec![("s1", "a", "s1", Activate)]
        );
        h.finish(&key("s1", "a"), ok());
        assert_eq!(starts(&h.manual("b")), vec![], "pending approval = no runs");
        let a = h.config(vec![tab("a"), tab("b")]);
        assert_eq!(starts(&a), vec![("s1", "b", "s1", Activate)]);
    }

    #[test]
    fn hidden_rail_runs_nothing_and_showing_it_activates() {
        let mut h = H::new(vec![tab("a"), plugin("b", Scope::Tab, &[TurnEnd], None)]);
        h.view.rail_visible = false;
        h.push_view();
        h.open("s1", "/p");
        assert_eq!(starts(&h.activate(Some("s1"))), vec![]);
        assert_eq!(starts(&h.turn_end("s1")), vec![]);
        assert_eq!(starts(&h.manual("a")), vec![]);
        h.view.rail_visible = true;
        let a = h.push_view();
        assert_eq!(
            starts(&a),
            vec![("s1", "a", "s1", Activate), ("s1", "b", "s1", TurnEnd)],
            "a activates; b replays the turn end it missed"
        );
    }

    #[test]
    fn match_is_enforced_per_session_and_rechecked_on_ctx_change() {
        let mut p = tab("w");
        p.match_path = Some("/work".into());
        let mut r = tab("r");
        r.match_remote = Some("gitlab".into());
        let mut h = H::new(vec![p, r]);
        h.open_ctx("s1", "/home/x", "github.com/x", "main");
        assert_eq!(starts(&h.activate(Some("s1"))), vec![]);
        assert_eq!(starts(&h.manual("w")), vec![], "manual needs a match too");
        let a = h.open_ctx("s1", "/home/x", "gitlab.com/x", "main");
        assert_eq!(
            starts(&a),
            vec![("s1", "r", "s1", Activate)],
            "remote now matches"
        );
        h.open_ctx("s2", "/work/y", "", "main");
        assert_eq!(
            starts(&h.activate(Some("s2"))),
            vec![("s2", "w", "s2", Activate)]
        );
    }

    #[test]
    fn disabled_plugins_never_run() {
        let mut p = tab("d");
        p.enabled = false;
        p.interval_ms = Some(MIN);
        let mut h = H::new(vec![p]);
        h.open("s1", "/p");
        assert_eq!(starts(&h.activate(Some("s1"))), vec![]);
        assert_eq!(starts(&h.manual("d")), vec![]);
        assert_eq!(h.s.next_deadline(), None, "no timer");
    }

    // ── turn_end / branch / dirty ─────────────────────────────────────────

    #[test]
    fn turn_end_runs_wanting_plugins_of_the_active_tab() {
        let mut h = H::settled(vec![
            tab("a"),
            plugin("b", Scope::Tab, &[Activate], None),
            global("g"),
        ]);
        let a = h.turn_end("s1");
        assert_eq!(
            starts(&a),
            vec![("s1", "a", "s1", TurnEnd), (GLOBAL_KEY, "g", "s1", TurnEnd)]
        );
    }

    #[test]
    fn background_triggers_are_flushed_once_on_activation() {
        let mut h = H::settled(vec![
            plugin("a", Scope::Tab, &[Activate, TurnEnd], None),
            plugin("b", Scope::Tab, &[TurnEnd, Branch], None),
            plugin("c", Scope::Tab, &[Branch], None),
            plugin("g", Scope::Global, &[TurnEnd], None),
        ]);
        h.open("s2", "/q");
        assert_eq!(
            starts(&h.activate(Some("s2"))),
            vec![("s2", "a", "s2", Activate)]
        );
        h.finish(&key("s2", "a"), ok());
        // Background s1: nothing runs now.
        assert_eq!(starts(&h.turn_end("s1")), vec![]);
        assert_eq!(starts(&h.turn_end("s1")), vec![]);
        h.open_ctx("s1", "/p", "", "dev");
        assert_eq!(starts(&h.branch("s1", "dev")), vec![]);
        let a = h.activate(Some("s1"));
        assert_eq!(
            starts(&a),
            vec![
                ("s1", "a", "s1", Activate),
                ("s1", "b", "s1", Branch),
                ("s1", "c", "s1", Branch)
            ],
            "one run per plugin; latest dirty trigger; globals are not replayed"
        );
        for id in ["a", "b", "c"] {
            h.finish(&key("s1", id), ok());
        }
        h.activate(Some("s2"));
        let a = h.activate(Some("s1"));
        assert_eq!(
            starts(&a),
            vec![("s1", "a", "s1", Activate)],
            "flushed only once"
        );
    }

    #[test]
    fn hidden_window_defers_turn_end_until_visible() {
        let mut h = H::settled(vec![plugin("b", Scope::Tab, &[TurnEnd], None)]);
        h.view.window_visible = false;
        h.push_view();
        assert_eq!(starts(&h.turn_end("s1")), vec![]);
        h.view.window_visible = true;
        assert_eq!(starts(&h.push_view()), vec![("s1", "b", "s1", TurnEnd)]);
        assert_eq!(starts(&h.push_view()), vec![], "once");
    }

    #[test]
    fn branch_is_deduplicated_and_the_first_one_only_recorded() {
        let mut h = H::new(vec![tab("a")]);
        h.open_ctx("s1", "/p", "", "");
        h.activate(Some("s1"));
        h.finish(&key("s1", "a"), ok());
        assert_eq!(
            starts(&h.branch("s1", "main")),
            vec![],
            "first seen only records"
        );
        assert_eq!(starts(&h.branch("s1", "main")), vec![], "same branch");
        assert_eq!(
            starts(&h.branch("s1", "dev")),
            vec![("s1", "a", "s1", Branch)]
        );
        h.finish(&key("s1", "a"), ok());
        assert_eq!(starts(&h.branch("s1", "dev")), vec![]);
    }

    #[test]
    fn the_branch_a_session_opened_on_counts_as_seen() {
        let mut h = H::settled(vec![tab("a")]);
        assert_eq!(starts(&h.branch("s1", "main")), vec![], "opened on main");
        h.open_ctx("s1", "/p", "", "dev");
        assert_eq!(
            starts(&h.branch("s1", "dev")),
            vec![("s1", "a", "s1", Branch)]
        );
    }

    // ── manual ────────────────────────────────────────────────────────────

    #[test]
    fn manual_ignores_the_trigger_list_but_needs_an_active_tab() {
        let mut h = H::new(vec![plugin("m", Scope::Tab, &[], None)]);
        h.open("s1", "/p");
        assert_eq!(starts(&h.manual("m")), vec![], "no active tab");
        assert_eq!(starts(&h.activate(Some("s1"))), vec![]);
        assert_eq!(starts(&h.manual("m")), vec![("s1", "m", "s1", Manual)]);
        assert_eq!(starts(&h.manual("nope")), vec![]);
    }

    // ── intervals ─────────────────────────────────────────────────────────

    #[test]
    fn interval_ticks_run_and_re_arm() {
        let mut h = H::settled(vec![plugin("t", Scope::Tab, &[Activate], Some(MIN))]);
        let start = h.now;
        assert_eq!(h.s.next_deadline(), Some(start + MIN));
        assert_eq!(h.ev(Event::Tick), vec![], "not due yet");
        let a = h.tick_to_deadline();
        assert_eq!(starts(&a), vec![("s1", "t", "s1", Interval)]);
        assert_eq!(h.s.next_deadline(), Some(start + 2 * MIN));
    }

    #[test]
    fn hidden_window_pauses_ticks_and_catches_up_once_when_visible() {
        let mut h = H::settled(vec![
            plugin("t", Scope::Tab, &[Activate], Some(MIN)),
            plugin("f", Scope::Tab, &[Activate], Some(10 * MIN)),
        ]);
        h.view.window_visible = false;
        h.push_view();
        let a = h.tick_to_deadline();
        assert_eq!(starts(&a), vec![], "paused while hidden");
        assert_eq!(h.s.next_deadline(), Some(h.now + MIN), "re-armed anyway");
        h.now += 1;
        h.view.window_visible = true;
        let a = h.push_view();
        assert_eq!(
            starts(&a),
            vec![("s1", "t", "s1", Interval)],
            "due one catches up; f is fresh"
        );
        assert_eq!(h.s.next_deadline(), Some(h.now + MIN), "timer restarted");
        assert_eq!(starts(&h.push_view()), vec![]);
    }

    #[test]
    fn catch_up_treats_a_run_in_flight_as_fresh() {
        let mut h = H::new(vec![plugin("t", Scope::Tab, &[Activate], Some(MIN))]);
        h.open("s1", "/p");
        h.activate(Some("s1"));
        h.view.window_visible = false;
        h.push_view();
        h.now += 10 * MIN;
        h.view.window_visible = true;
        assert_eq!(starts(&h.push_view()), vec![]);
        assert_eq!(h.s.run(&key("s1", "t")).expect("t").queued, None);
    }

    #[test]
    fn tab_timers_follow_the_active_tab_and_restart_on_switch() {
        let mut h = H::settled(vec![plugin("t", Scope::Tab, &[], Some(MIN))]);
        h.open("s2", "/q");
        h.now += MIN / 2;
        assert_eq!(
            starts(&h.activate(Some("s2"))),
            vec![],
            "no activate trigger"
        );
        assert_eq!(h.s.next_deadline(), Some(h.now + MIN), "re-armed on switch");
        let a = h.tick_to_deadline();
        assert_eq!(
            starts(&a),
            vec![("s2", "t", "s2", Interval)],
            "runs for the active tab only"
        );
    }

    #[test]
    fn global_timer_survives_tab_switches_and_rechecks_match() {
        let mut g = plugin("g", Scope::Global, &[], Some(MIN));
        g.match_path = Some("/work".into());
        let mut h = H::new(vec![g]);
        h.open("s1", "/work/a");
        h.open("s2", "/home/b");
        h.activate(Some("s1"));
        let due = h.s.next_deadline();
        h.now += MIN / 2;
        h.activate(Some("s2"));
        assert_eq!(h.s.next_deadline(), due, "not re-armed by a tab switch");
        assert_eq!(
            starts(&h.tick_to_deadline()),
            vec![],
            "active tab does not match"
        );
        h.activate(Some("s1"));
        assert_eq!(
            starts(&h.tick_to_deadline()),
            vec![(GLOBAL_KEY, "g", "s1", Interval)]
        );
    }

    #[test]
    fn global_follow_up_rechecks_match_against_the_active_tab() {
        let mut g = global("g");
        g.match_path = Some("/proj/a".into());
        let mut h = H::new(vec![g]);
        h.open("s1", "/proj/a/sub");
        h.open("s2", "/proj/b");
        h.activate(Some("s1"));
        h.turn_end("s1");
        h.activate(Some("s2"));
        let a = h.finish(&key(GLOBAL_KEY, "g"), ok());
        assert_eq!(starts(&a), vec![], "active project no longer matches");
        assert_eq!(h.s.run(&key(GLOBAL_KEY, "g")).expect("g").queued, None);
    }

    #[test]
    fn interval_follow_ups_need_a_visible_window_and_for_tabs_the_active_tab() {
        let plugins = || {
            vec![
                plugin("t", Scope::Tab, &[Activate, TurnEnd], Some(MIN)),
                plugin("g", Scope::Global, &[Activate], Some(MIN)),
            ]
        };
        // Tab plugin, its tab no longer active.
        let mut h = H::new(plugins());
        h.open("s1", "/p");
        h.open("s2", "/q");
        h.activate(Some("s1"));
        h.tick_to_deadline();
        h.activate(Some("s2"));
        let a = h.finish(&key("s1", "t"), ok());
        assert_eq!(
            starts(&a),
            vec![],
            "tab interval follow-up needs its tab active"
        );
        let a = h.finish(&key(GLOBAL_KEY, "g"), ok());
        assert_eq!(
            starts(&a),
            vec![(GLOBAL_KEY, "g", "s2", Interval)],
            "global ignores the tab"
        );
        // Hidden window.
        let mut h = H::new(plugins());
        h.open("s1", "/p");
        h.activate(Some("s1"));
        h.tick_to_deadline();
        h.view.window_visible = false;
        h.push_view();
        assert_eq!(starts(&h.finish(&key("s1", "t"), ok())), vec![]);
        assert_eq!(starts(&h.finish(&key(GLOBAL_KEY, "g"), ok())), vec![]);
    }

    #[test]
    fn non_interval_follow_ups_run_for_background_tabs() {
        let mut h = H::new(vec![tab("t")]);
        h.open("s1", "/p");
        h.open("s2", "/q");
        h.activate(Some("s1"));
        h.turn_end("s1");
        h.activate(Some("s2"));
        let a = h.finish(&key("s1", "t"), ok());
        assert_eq!(starts(&a), vec![("s1", "t", "s1", TurnEnd)]);
    }

    #[test]
    fn non_interval_follow_ups_respect_the_rail() {
        let mut h = H::new(vec![tab("t")]);
        h.open("s1", "/p");
        h.activate(Some("s1"));
        h.turn_end("s1");
        h.view.rail_visible = false;
        h.push_view();
        assert_eq!(starts(&h.finish(&key("s1", "t"), ok())), vec![]);
    }

    #[test]
    fn clock_going_backwards_neither_panics_nor_stalls_timers() {
        let mut h = H::settled(vec![
            plugin("t", Scope::Tab, &[Activate], Some(MIN)),
            plugin("g", Scope::Global, &[Activate], Some(MIN)),
        ]);
        h.now = 5;
        assert_eq!(h.ev(Event::Tick), vec![]);
        assert_eq!(
            h.s.next_deadline(),
            Some(5 + MIN),
            "clamped to one interval"
        );
        h.view.window_visible = false;
        h.push_view();
        h.view.window_visible = true;
        assert_eq!(
            starts(&h.push_view()),
            vec![],
            "a future finish time is fresh"
        );
        h.now = 0;
        h.finish(&key("s1", "t"), ok());
        assert_eq!(starts(&h.tick_to_deadline()).len(), 2);
    }

    // ── concurrency ───────────────────────────────────────────────────────

    #[test]
    fn concurrency_is_capped_with_fifo_promotion() {
        let ids = ["a", "b", "c", "d", "e", "f"];
        let mut h = H::new(ids.iter().map(|id| tab(id)).collect());
        h.open("s1", "/p");
        let a = h.activate(Some("s1"));
        assert_eq!(starts(&a).len(), MAX_CONCURRENT_RUNS);
        assert_eq!(h.phase(&key("s1", "e")), Phase::Waiting);
        assert_eq!(h.phase(&key("s1", "f")), Phase::Waiting);
        // A trigger on a waiting run coalesces.
        h.manual("e");
        assert_eq!(h.s.run(&key("s1", "e")).expect("e").queued, Some(Manual));
        let a = h.finish(&key("s1", "c"), ok());
        assert_eq!(
            starts(&a),
            vec![("s1", "e", "s1", Activate)],
            "first in, first out"
        );
        let a = h.finish(&key("s1", "a"), ok());
        assert_eq!(starts(&a), vec![("s1", "f", "s1", Activate)]);
        let a = h.finish(&key("s1", "e"), ok());
        assert_eq!(starts(&a), vec![("s1", "e", "s1", Manual)], "its follow-up");
    }

    #[test]
    fn a_waiting_run_that_became_hidden_is_dropped_on_promotion() {
        let ids = ["a", "b", "c", "d", "e"];
        let mut h = H::new(ids.iter().map(|id| tab(id)).collect());
        h.open("s1", "/p");
        h.activate(Some("s1"));
        h.view.hidden.insert("e".into());
        h.push_view();
        let a = h.finish(&key("s1", "a"), ok());
        assert_eq!(starts(&a), vec![]);
        assert_eq!(h.phase(&key("s1", "e")), Phase::Idle);
    }

    #[test]
    fn closing_a_session_kills_waiting_runs_and_frees_slots() {
        let ids = ["a", "b", "c", "d", "e"];
        let mut h = H::new(ids.iter().map(|id| tab(id)).collect());
        h.open("s1", "/p");
        h.open("s2", "/q");
        h.activate(Some("s1"));
        let a = h.activate(Some("s2"));
        assert_eq!(starts(&a), vec![], "all slots taken");
        let a = h.ev(Event::SessionClosed {
            session: "s1".into(),
        });
        assert_eq!(kills(&a).len(), 5, "four running, one waiting");
        assert_eq!(removed(&a).len(), 5);
        assert_eq!(
            starts(&a).len(),
            MAX_CONCURRENT_RUNS,
            "s2's waiting runs promoted"
        );
        assert!(starts(&a).iter().all(|s| s.0 == "s2"));
    }

    // ── config reload ─────────────────────────────────────────────────────

    #[test]
    fn removed_or_unapproved_plugins_are_killed_and_removed() {
        let mut h = H::new(vec![tab("a"), tab("b"), global("g")]);
        h.open("s1", "/p");
        h.activate(Some("s1"));
        h.finish(&key("s1", "b"), ok());
        let plugins = vec![tab("b"), global("g")];
        let a = h.ev(Event::Config {
            plugins: shared(plugins),
            approved: HashSet::from(["b".to_string()]),
        });
        let mut killed = kills(&a);
        killed.sort_unstable();
        assert_eq!(
            killed,
            vec![(GLOBAL_KEY, "g"), ("s1", "a")],
            "removed / revoked approval"
        );
        assert_eq!(removed(&a), vec![(GLOBAL_KEY, "g"), ("s1", "a")]);
        assert!(h.s.run(&key("s1", "b")).is_some(), "b keeps its result");
        assert_eq!(starts(&a), vec![]);
    }

    #[test]
    fn a_late_result_of_a_killed_run_never_lands_in_a_new_run() {
        let mut h = H::new(vec![tab("a")]);
        h.open("s1", "/p");
        h.activate(Some("s1"));
        let old = h.seq(&key("s1", "a"));
        h.config(Vec::new());
        h.config(vec![tab("a")]);
        assert_eq!(h.phase(&key("s1", "a")), Phase::Running);
        h.ev(Event::Finished {
            key: key("s1", "a"),
            seq: old,
            outcome: err("killed"),
        });
        assert_eq!(h.phase(&key("s1", "a")), Phase::Running);
    }

    #[test]
    fn scope_change_drops_old_keys_and_reactivates() {
        let mut h = H::settled(vec![tab("p")]);
        let a = h.config(vec![global("p")]);
        assert_eq!(removed(&a), vec![("s1", "p")]);
        assert_eq!(starts(&a), vec![(GLOBAL_KEY, "p", "s1", Activate)]);
    }

    #[test]
    fn other_field_changes_keep_results_and_do_not_rerun() {
        let mut h = H::settled(vec![tab("a")]);
        let mut a2 = tab("a");
        a2.title = "renamed".into();
        a2.timeout_ms = 5_000;
        let a = h.config(vec![a2]);
        assert_eq!(a, vec![]);
        assert!(h.s.run(&key("s1", "a")).is_some());
        assert_eq!(
            h.s.cfg.get("a").map(|(_, p)| p.title.as_str()),
            Some("renamed")
        );
    }

    #[test]
    fn interval_change_rebuilds_the_timer_and_removal_drops_it() {
        let mut h = H::settled(vec![plugin("t", Scope::Tab, &[Activate], Some(MIN))]);
        let start = h.now;
        h.now += 10;
        h.config(vec![plugin("t", Scope::Tab, &[Activate], Some(MIN))]);
        assert_eq!(
            h.s.next_deadline(),
            Some(start + MIN),
            "unchanged timer kept"
        );
        h.config(vec![plugin("t", Scope::Tab, &[Activate], Some(2 * MIN))]);
        assert_eq!(h.s.next_deadline(), Some(h.now + 2 * MIN), "rebuilt");
        h.config(vec![plugin("t", Scope::Tab, &[Activate], None)]);
        assert_eq!(h.s.next_deadline(), None);
    }

    #[test]
    fn approval_change_rebuilds_the_timer() {
        let p = || plugin("t", Scope::Global, &[], Some(MIN));
        let mut h = H::settled(vec![p()]);
        h.ev(Event::Config {
            plugins: shared(vec![p()]),
            approved: HashSet::new(),
        });
        assert_eq!(h.s.next_deadline(), None, "unapproved: no timer");
        h.now += 5;
        h.config(vec![p()]);
        assert_eq!(h.s.next_deadline(), Some(h.now + MIN));
    }

    #[test]
    fn dirty_triggers_of_removed_plugins_are_forgotten() {
        let mut h = H::settled(vec![plugin("b", Scope::Tab, &[TurnEnd], None)]);
        h.open("s2", "/q");
        h.activate(Some("s2"));
        h.turn_end("s1");
        h.config(Vec::new());
        h.config(vec![plugin("b", Scope::Tab, &[TurnEnd], None)]);
        assert_eq!(starts(&h.activate(Some("s1"))), vec![]);
    }

    // ── active_applicable ─────────────────────────────────────────────────

    #[test]
    fn active_applicable_lists_matching_approved_enabled_plugins() {
        let mut w = tab("w");
        w.match_path = Some("/work".into());
        let mut gw = global("gw");
        gw.match_path = Some("/work".into());
        let mut off = tab("off");
        off.enabled = false;
        let plugins = vec![w, tab("any"), off, gw, tab("pending")];
        let mut h = H::new(Vec::new());
        let mut approved = approved_all(&plugins);
        approved.remove("pending");
        h.ev(Event::Config {
            plugins: shared(plugins),
            approved,
        });
        assert_eq!(h.s.active_applicable(), (None, vec![]));
        h.open("s1", "/work/x");
        h.open("s2", "/home/y");
        h.activate(Some("s1"));
        assert_eq!(
            h.s.active_applicable(),
            (Some("s1"), vec!["w", "any", "gw"])
        );
        h.activate(Some("s2"));
        assert_eq!(
            h.s.active_applicable(),
            (Some("s2"), vec!["any"]),
            "global in a non-matching tab excluded"
        );
        h.view.hidden.insert("any".into());
        h.view.rail_visible = false;
        h.view.window_visible = false;
        h.push_view();
        assert_eq!(
            h.s.active_applicable(),
            (Some("s2"), vec!["any"]),
            "visibility and hidden cards do not matter"
        );
    }

    #[test]
    fn bits_yields_set_indices_in_order() {
        assert_eq!(bits(0).collect::<Vec<_>>(), Vec::<usize>::new());
        assert_eq!(bits(0b1010_0001).collect::<Vec<_>>(), vec![0, 5, 7]);
        assert_eq!(bits(1 << 63).collect::<Vec<_>>(), vec![63]);
        assert_eq!(bit(64), 0);
    }
}
