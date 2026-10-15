//! The panel against a real Chromium (skipped when none is installed), and
//! the URL policy.

use std::net::TcpListener;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::super::cdp::{browser_ws_url, CdpClient};
use super::super::tests::{eventually, manager, manager_with_chromium, mouse, CHROMIUM};
use super::super::{lock, BrowserManager};
use super::{checked_url, BrowserEmit, Frame, InputEvent, Pick, Status, TabInfo, View, ViewState};

#[derive(Clone, Debug)]
enum Seen {
    Frame(Frame),
    Tabs(Vec<TabInfo>),
    State(ViewState),
}

/// Records everything the view emits, in order, and acks every frame as
/// the webview does once it painted it.
#[derive(Default)]
struct Collect {
    log: Mutex<Vec<Seen>>,
    view: OnceLock<Weak<View>>,
}

impl Collect {
    fn log(&self) -> Vec<Seen> {
        lock(&self.log).clone() // a snapshot for the test to inspect
    }

    fn frames(&self) -> Vec<Frame> {
        self.log()
            .into_iter()
            .filter_map(|s| match s {
                Seen::Frame(f) => Some(f),
                _ => None,
            })
            .collect()
    }

    fn frame_count(&self) -> usize {
        lock(&self.log)
            .iter()
            .filter(|s| matches!(s, Seen::Frame(_)))
            .count()
    }

    fn last_tabs(&self) -> Vec<TabInfo> {
        self.log()
            .into_iter()
            .rev()
            .find_map(|s| match s {
                Seen::Tabs(t) => Some(t),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn last_state(&self) -> Option<ViewState> {
        self.log().into_iter().rev().find_map(|s| match s {
            Seen::State(s) => Some(s),
            _ => None,
        })
    }
}

impl BrowserEmit for Collect {
    fn frame(&self, _: &str, f: &Frame) {
        lock(&self.log).push(Seen::Frame(f.clone())); // the log owns what it records
        if let Some(view) = self.view.get().and_then(Weak::upgrade) {
            view.frame_ack(f.seq);
        }
    }
    fn tabs(&self, _: &str, tabs: &[TabInfo]) {
        lock(&self.log).push(Seen::Tabs(tabs.to_vec()));
    }
    fn state(&self, _: &str, s: &ViewState) {
        lock(&self.log).push(Seen::State(s.clone())); // the log owns what it records
    }
    fn pick(&self, _: &str, _: &Pick) {}
}

/// A manager with a panel open on session `s1` (400×300 at 2x) whose frames
/// are acked, or `None` without Chromium.
fn panel(name: &str) -> Option<(BrowserManager, Arc<View>, Arc<Collect>)> {
    let seen = Arc::new(Collect::default());
    let emit: Arc<dyn BrowserEmit> = Arc::clone(&seen) as Arc<dyn BrowserEmit>;
    let (m, view) = super::super::tests::panel(name, 2.0, emit)?;
    seen.view.set(Arc::downgrade(&view)).unwrap();
    Some((m, view, seen))
}

const SECS_3: Duration = Duration::from_secs(3);

/// Wait until the shown page has loaded a document where `condition` (JS)
/// holds: `navigate` returns before the old document is gone.
fn loaded(view: &View, condition: &str) {
    let expression = format!("document.readyState === 'complete' && !!({condition})");
    eventually(SECS_3, || {
        (view.evaluate_for_test(&expression).ok()? == json!(true)).then_some(())
    })
    .unwrap_or_else(|| panic!("page with {condition} loaded"));
}

fn active_tab(seen: &Collect) -> Option<String> {
    seen.last_tabs()
        .into_iter()
        .find(|t| t.active)
        .map(|t| t.target_id)
}

/// Wait until no frame has come for a while (a still page), and give back
/// how many there are.
fn settled(seen: &Collect) -> usize {
    eventually(Duration::from_secs(10), || {
        let before = seen.frame_count();
        std::thread::sleep(Duration::from_millis(400));
        (seen.frame_count() == before).then_some(before)
    })
    .expect("the screencast goes quiet on a still page")
}

#[test]
fn only_web_inline_and_blank_urls_are_loaded() {
    for url in [
        "https://example.com/",
        "HTTP://localhost:3000/x",
        " data:text/html,<h1>hi</h1> ",
        "about:blank",
        "About:Blank",
    ] {
        assert_eq!(checked_url(url), Ok(url.trim()), "{url}");
    }
    for url in [
        "javascript:alert(1)",
        "file:///etc/passwd",
        "chrome://kill",
        "about:config",
        "view-source:https://example.com/",
        "example.com",
    ] {
        assert_eq!(
            checked_url(url),
            Err("unsupported url scheme".to_owned()),
            "{url}"
        );
    }
    let long = format!("https://{}", "a".repeat(8200));
    for url in ["", "   ", "https://x/\u{7}y", &long] {
        assert_eq!(checked_url(url), Err("invalid url".to_owned()), "{url:?}");
    }
}

#[test]
fn screencast_emits_numbered_frames_of_the_panel_size() {
    let _chromium = lock(&CHROMIUM);
    let Some((m, view, seen)) = panel("view-frames") else {
        return;
    };
    view.navigate("data:text/html,<h1>hi</h1>").unwrap();
    let frame = eventually(SECS_3, || {
        seen.frames().into_iter().find(|f| f.css_w == 400)
    });
    let frame = frame.expect("a frame of the 400 px wide page");
    assert_eq!(frame.css_h, 300);
    assert_eq!(
        frame.w * frame.css_h,
        frame.h * frame.css_w,
        "the image has the page's proportions"
    );
    assert!(frame.b64.starts_with("/9j/"), "a base64 JPEG");
    // Chromium may capture its whole window surface for a moment after a
    // navigation; the view restarts the screencast, so the latest frame is
    // the page's size again.
    let size = eventually(Duration::from_secs(5), || {
        let last = seen.frames().pop()?;
        ((last.css_w, last.css_h) == (400, 300)).then_some(())
    });
    assert!(size.is_some(), "settled on the panel size");
    let seqs: Vec<u64> = seen.frames().iter().map(|f| f.seq).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    assert_eq!(seen.last_state().unwrap().status, Status::Running);
    m.release("s1");
}

#[test]
fn frames_keep_coming_while_the_page_changes() {
    let _chromium = lock(&CHROMIUM);
    let Some((m, view, seen)) = panel("view-ack") else {
        return;
    };
    // Chromium stops sending while frames are not acknowledged; the acks
    // follow the webview's.
    view.navigate(
        "data:text/html,<body><script>setInterval(()=>{document.body.style.background=\
         'rgb('+(Date.now()&255)+',0,0)'},20)</script>",
    )
    .unwrap();
    let flowing = eventually(Duration::from_secs(4), || {
        (seen.frame_count() >= 15).then_some(())
    });
    assert!(flowing.is_some(), "only {} frames", seen.frame_count());
    m.release("s1");
}

#[test]
fn refresh_frame_brings_a_newer_frame_of_a_still_page() {
    let _chromium = lock(&CHROMIUM);
    let Some((m, view, seen)) = panel("view-refresh-frame") else {
        return;
    };
    view.navigate("data:text/html,<h1>still</h1>").unwrap();
    loaded(&view, "document.querySelector('h1')");
    let before = settled(&seen);
    let newest = seen.frames().last().map_or(0, |f| f.seq);
    view.core.refresh_frame();
    let fresh = eventually(SECS_3, || {
        seen.frames()
            .into_iter()
            .find(|f| f.seq > newest && f.css_w == 400)
    });
    assert!(
        fresh.is_some(),
        "a new frame after {before}, though nothing changed on the page"
    );
    m.release("s1");
}

#[test]
fn the_tab_title_follows_the_page() {
    let _chromium = lock(&CHROMIUM);
    let Some((m, view, seen)) = panel("view-title") else {
        return;
    };
    let titled = |title: &str| {
        eventually(SECS_3, || {
            seen.last_tabs()
                .iter()
                .any(|t| t.active && t.title == title)
                .then_some(())
        })
    };
    view.navigate("data:text/html,<title>Demo Shop</title><p>shop")
        .unwrap();
    assert!(titled("Demo Shop").is_some(), "{:?}", seen.last_tabs());
    // A single-page app: a new title with a same-document navigation.
    view.evaluate_for_test("document.title = 'Cart'; history.pushState({}, '', '#cart')")
        .unwrap();
    assert!(titled("Cart").is_some(), "{:?}", seen.last_tabs());
    m.release("s1");
}

#[test]
fn input_reaches_the_page() {
    let _chromium = lock(&CHROMIUM);
    let Some((m, view, _seen)) = panel("view-input") else {
        return;
    };
    let page = "data:text/html,<input id=i><button onclick=\"document.title='clicked'\" \
                style=\"position:absolute;left:100px;top:100px;width:100px;height:50px\">go</button>";
    view.navigate(page).unwrap();
    loaded(&view, "document.getElementById('i')");
    focus_input(&view);
    view.input(&InputEvent::Text { text: "abc".into() })
        .unwrap();
    let typed = eventually(SECS_3, || {
        let value = view
            .evaluate_for_test("document.getElementById('i').value")
            .ok()?;
        (value == json!("abc")).then_some(value)
    });
    assert!(typed.is_some(), "typed text reached the input");
    for event in [
        mouse("mouseMoved", 150.0, 125.0),
        mouse("mousePressed", 150.0, 125.0),
        mouse("mouseReleased", 150.0, 125.0),
    ] {
        view.input(&event).unwrap();
    }
    let clicked = eventually(SECS_3, || {
        let title = view.evaluate_for_test("document.title").ok()?;
        (title == json!("clicked")).then_some(title)
    });
    assert!(clicked.is_some(), "click reached the button");
    assert!(
        view.core.panel_released_within(Duration::from_secs(10)),
        "the release was stamped"
    );
    m.release("s1");
}

#[test]
fn acting_on_the_page_holds_the_view_but_hovering_does_not() {
    let _chromium = lock(&CHROMIUM);
    let Some((m, view, _seen)) = panel("view-input-hold") else {
        return;
    };
    let may_follow = || lock(&view.core.inner).may_follow();
    assert!(may_follow(), "nothing held yet");
    view.input(&mouse("mouseMoved", 10.0, 10.0)).unwrap();
    assert!(may_follow(), "hovering is not acting");
    view.input(&InputEvent::Text { text: "x".into() }).unwrap();
    assert!(!may_follow(), "typing holds the view");
    lock(&view.core.inner).user_activated = None;
    view.navigate("data:text/html,typed").unwrap();
    assert!(!may_follow(), "the address field holds the view");
    m.release("s1");
}

#[test]
fn tabs_and_history() {
    let _chromium = lock(&CHROMIUM);
    let Some((m, view, seen)) = panel("view-tabs") else {
        return;
    };
    assert_eq!(seen.last_tabs().len(), 1, "the initial blank tab");
    view.tab_new().unwrap();
    let two = eventually(SECS_3, || (seen.last_tabs().len() == 2).then_some(()));
    assert!(two.is_some(), "{:?}", seen.last_tabs());
    assert!(seen.last_tabs()[1].active, "the new tab is shown");
    view.navigate("data:text/html,a").unwrap();
    loaded(&view, "location.href.endsWith(',a')");
    view.navigate("data:text/html,b").unwrap();
    let on_b = eventually(SECS_3, || {
        let state = seen.last_state()?;
        (state.url.ends_with(",b") && state.can_back).then_some(())
    });
    assert!(on_b.is_some(), "{:?}", seen.last_state());
    assert!(!seen.last_state().unwrap().can_forward);
    view.history(-1).unwrap();
    let back = eventually(SECS_3, || {
        let state = seen.last_state()?;
        (state.can_forward && state.url.ends_with(",a")).then_some(())
    });
    assert!(back.is_some(), "{:?}", seen.last_state());
    let second = seen.last_tabs()[1].target_id.clone(); // the id outlives the log snapshot
    view.tab_close(&second).unwrap();
    let one = eventually(SECS_3, || {
        let tabs = seen.last_tabs();
        (tabs.len() == 1 && tabs[0].active).then_some(())
    });
    assert!(one.is_some(), "{:?}", seen.last_tabs());
    m.release("s1");
}

#[test]
fn a_slow_server_is_not_a_failed_navigation_and_a_failed_load_is_told() {
    let _chromium = lock(&CHROMIUM);
    let Some((m, view, seen)) = panel("view-navigate") else {
        return;
    };
    // Accepts and never answers, like a dev server compiling its first route.
    let slow = TcpListener::bind("127.0.0.1:0").unwrap();
    let slow_port = slow.local_addr().unwrap().port();
    let started = Instant::now();
    assert_eq!(
        view.navigate(&format!("http://127.0.0.1:{slow_port}/")),
        Ok(())
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "returns once sent"
    );
    let loading = eventually(SECS_3, || seen.last_state()?.loading.then_some(()));
    assert!(loading.is_some(), "{:?}", seen.last_state());
    drop(slow);
    // Nothing listens here: the load fails.
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let closed_port = closed.local_addr().unwrap().port();
    drop(closed);
    view.navigate(&format!("http://127.0.0.1:{closed_port}/"))
        .unwrap();
    let failed = eventually(Duration::from_secs(5), || {
        seen.last_state()?
            .error
            .filter(|e| e.contains(&format!("127.0.0.1:{closed_port}")))
    });
    assert!(failed.is_some(), "{:?}", seen.last_state());
    view.navigate("data:text/html,fine").unwrap();
    let cleared = eventually(SECS_3, || {
        let state = seen.last_state()?;
        (state.error.is_none() && state.url.ends_with(",fine")).then_some(())
    });
    assert!(cleared.is_some(), "{:?}", seen.last_state());
    assert_eq!(
        view.navigate("javascript:alert(1)"),
        Err("unsupported url scheme".to_owned())
    );
    m.release("s1");
}

/// The agent's side: a raw client through the placeholder port, on the
/// browser connection or (`page`) on one page's own.
fn agent(m: &BrowserManager, page: Option<&str>) -> CdpClient {
    let port = m.port("s1").unwrap();
    let ws = page.map_or_else(
        || browser_ws_url(port).unwrap(),
        |id| format!("ws://127.0.0.1:{port}/devtools/page/{id}"),
    );
    CdpClient::connect(&ws, Box::new(|_, _, _: Value| {})).unwrap()
}

/// Attach as puppeteer does and give back the page session.
fn attach(agent: &CdpClient, target: &str) -> String {
    let attached = agent
        .call(
            None,
            "Target.attachToTarget",
            json!({ "targetId": target, "flatten": true }),
        )
        .unwrap();
    attached["sessionId"].as_str().unwrap().to_owned()
}

fn navigate(agent: &CdpClient, session: &str, url: &str) {
    agent
        .call(Some(session), "Page.navigate", json!({ "url": url }))
        .unwrap();
}

fn create_page(agent: &CdpClient, url: &str) -> String {
    let created = agent
        .call(None, "Target.createTarget", json!({ "url": url }))
        .unwrap();
    created["targetId"].as_str().unwrap().to_owned()
}

/// Manager, view, what it emitted, the pages A and B, and when the user's
/// choice of A began to hold the view (measured before the choice).
struct TwoTabs {
    m: BrowserManager,
    view: Arc<View>,
    seen: Arc<Collect>,
    a: String,
    b: String,
    held_from: Instant,
}

/// A panel on page A with a second page B listed, the user's last choice
/// being A; `hold` is how long that choice keeps the view from following.
fn panel_on_a_next_to_b(name: &str, hold: Duration) -> Option<TwoTabs> {
    let (m, view, seen) = panel(name)?;
    lock(&view.core.inner).follow_hold = hold;
    let a = active_tab(&seen).unwrap();
    view.tab_new().unwrap();
    view.navigate("data:text/html,b").unwrap();
    let b = active_tab(&seen).unwrap();
    assert_ne!(a, b);
    let held_from = Instant::now();
    view.tab_activate(&a).unwrap();
    Some(TwoTabs {
        m,
        view,
        seen,
        a,
        b,
        held_from,
    })
}

impl TwoTabs {
    /// Wait out the user's hold.
    fn hold_over(&self) {
        let over = eventually(Duration::from_secs(5), || {
            lock(&self.view.core.inner).may_follow().then_some(())
        });
        assert!(over.is_some(), "the hold ends");
    }
}

/// Whether the view shows `target` and has framed it since `frames_before`.
fn shows(seen: &Collect, target: &str, frames_before: usize) -> bool {
    active_tab(seen).as_deref() == Some(target) && seen.frame_count() > frames_before
}

#[test]
fn follows_an_agent_that_navigates_an_existing_page() {
    let _chromium = lock(&CHROMIUM);
    let Some(t) = panel_on_a_next_to_b("view-follow-nav", Duration::from_millis(200)) else {
        return;
    };
    t.hold_over();
    let agent = agent(&t.m, None);
    let session = attach(&agent, &t.b);
    std::thread::sleep(Duration::from_millis(600)); // what must not happen has time to
    assert_eq!(
        active_tab(&t.seen).as_deref(),
        Some(t.a.as_str()),
        "attached is not worked in: puppeteer stays attached to every page"
    );
    let frames_before = t.seen.frame_count();
    navigate(&agent, &session, "data:text/html,agent-b");
    let followed = eventually(SECS_3, || shows(&t.seen, &t.b, frames_before).then_some(()));
    assert!(
        followed.is_some(),
        "the view follows the agent to B and shows it"
    );
    agent.close();
    t.m.release("s1");
}

#[test]
fn the_agents_work_during_a_hold_is_shown_when_the_hold_ends() {
    let _chromium = lock(&CHROMIUM);
    let hold = Duration::from_secs(3);
    let Some(t) = panel_on_a_next_to_b("view-follow-hold", hold) else {
        return;
    };
    let agent = agent(&t.m, None);
    let session = attach(&agent, &t.b);
    navigate(&agent, &session, "data:text/html,agent-b");
    let frames_before = t.seen.frame_count();
    let followed = eventually(hold + SECS_3, || {
        shows(&t.seen, &t.b, frames_before).then(Instant::now)
    });
    let followed = followed.expect("the agent's last step is shown once the hold is over");
    assert!(
        followed.duration_since(t.held_from) >= hold,
        "not within the hold: after {:?}",
        followed.duration_since(t.held_from)
    );
    agent.close();
    t.m.release("s1");
}

#[test]
fn follows_a_page_the_agent_creates() {
    let _chromium = lock(&CHROMIUM);
    let Some(t) = panel_on_a_next_to_b("view-follow-create", Duration::from_millis(200)) else {
        return;
    };
    t.hold_over();
    let frames_before = t.seen.frame_count();
    let agent = agent(&t.m, None);
    let created = create_page(&agent, "data:text/html,agent-new");
    let followed = eventually(SECS_3, || {
        shows(&t.seen, &created, frames_before).then_some(())
    });
    assert!(
        followed.is_some(),
        "the view follows the agent to its new page"
    );
    agent.close();
    t.m.release("s1");
}

#[test]
fn follows_an_agent_on_a_page_level_connection() {
    let _chromium = lock(&CHROMIUM);
    let Some(t) = panel_on_a_next_to_b("view-follow-page-ws", Duration::from_millis(200)) else {
        return;
    };
    t.hold_over();
    let frames_before = t.seen.frame_count();
    let agent = agent(&t.m, Some(&t.b));
    agent.call(None, "Page.bringToFront", json!({})).unwrap();
    let followed = eventually(SECS_3, || shows(&t.seen, &t.b, frames_before).then_some(()));
    assert!(
        followed.is_some(),
        "bringToFront on B's own connection is work in B"
    );
    agent.close();
    t.m.release("s1");
}

#[test]
fn a_flood_of_follow_reports_costs_one_switch_to_the_last_page() {
    let _chromium = lock(&CHROMIUM);
    let Some(t) = panel_on_a_next_to_b("view-follow-flood", Duration::from_millis(200)) else {
        return;
    };
    t.hold_over();
    let shown_before = lock(&t.view.core.inner).activations();
    let frames_before = t.seen.frame_count();
    {
        let _paused = lock(&t.view.core.follow_gate);
        for i in 0..10_000 {
            t.view.follow_agent(if i % 2 == 0 { &t.a } else { &t.b }); // ends on B, the view shows A
        }
    }
    let followed = eventually(SECS_3, || shows(&t.seen, &t.b, frames_before).then_some(()));
    assert!(
        followed.is_some(),
        "the view ends on the last page reported"
    );
    std::thread::sleep(Duration::from_millis(500)); // what must not happen has time to
    assert_eq!(
        lock(&t.view.core.inner).activations(),
        shown_before + 1,
        "one switch, not one per report"
    );
    t.m.release("s1");
}

#[test]
fn the_agent_acting_ends_annotate_mode() {
    let _chromium = lock(&CHROMIUM);
    let Some((m, view, seen)) = panel("view-annotate-off") else {
        return;
    };
    let annotate =
        |on: bool| eventually(SECS_3, || (seen.last_state()?.annotate == on).then_some(()));
    view.annotate(true).unwrap();
    assert!(annotate(true).is_some(), "{:?}", seen.last_state());
    view.follow_agent(&active_tab(&seen).unwrap());
    assert!(annotate(false).is_some(), "{:?}", seen.last_state());
    assert!(!lock(&view.core.annotate).on);
    m.release("s1");
}

#[test]
fn a_panel_opened_after_the_agent_browsed_starts_on_its_page() {
    let _chromium = lock(&CHROMIUM);
    let Some(m) = manager_with_chromium("view-open-on-agent") else {
        return;
    };
    m.allocate("s1").unwrap();
    let agent = agent(&m, None);
    let b = create_page(&agent, "data:text/html,b");
    let c = create_page(&agent, "data:text/html,c"); // the newest page, not the agent's choice
    let session = attach(&agent, &b);
    navigate(&agent, &session, "data:text/html,agent-b");
    let told = eventually(SECS_3, || {
        (m.last_agent_target_for_test("s1").as_deref() == Some(b.as_str())).then_some(())
    });
    assert!(told.is_some(), "the manager remembers the agent's page");
    let seen = Arc::new(Collect::default());
    let emit: Arc<dyn BrowserEmit> = Arc::clone(&seen) as Arc<dyn BrowserEmit>;
    m.open_panel("s1", 400, 300, 2.0, emit).unwrap();
    assert!(
        seen.last_tabs().iter().any(|t| t.target_id == c),
        "C is listed too"
    );
    // The first page the panel ever showed, not just where it ended up.
    let first_shown = seen.log().into_iter().find_map(|s| match s {
        Seen::Tabs(tabs) => tabs.into_iter().find(|t| t.active).map(|t| t.target_id),
        _ => None,
    });
    assert_eq!(first_shown, Some(b), "the panel starts on the agent's page");
    agent.close();
    m.release("s1");
}

#[test]
fn release_closes_the_view_before_chromium_goes() {
    let _chromium = lock(&CHROMIUM);
    let Some((m, view, seen)) = panel("view-release") else {
        return;
    };
    m.release("s1");
    assert!(view.navigate("data:text/html,x").is_err());
    assert_eq!(seen.last_state().unwrap().status, Status::Idle);
    assert!(m.panel("s1").is_err());
}

#[test]
fn panel_commands_need_an_allocated_browser() {
    let m = manager("view-none");
    let emit: Arc<dyn BrowserEmit> = Arc::new(Collect::default());
    assert_eq!(
        m.open_panel("nope", 400, 300, 1.0, emit).unwrap_err(),
        "no browser for session"
    );
    assert_eq!(
        m.panel("nope").err().as_deref(),
        Some("no browser for session")
    );
    assert_eq!(
        m.close_panel("nope"),
        Err("no browser for session".to_owned())
    );
}

#[test]
fn a_bad_panel_size_is_refused_before_chromium_is_launched() {
    let m = manager("view-bad-size");
    m.allocate("s1").unwrap();
    for (w, h, dpr) in [(0, 300, 1.0), (4096, 300, 4.0)] {
        let emit: Arc<dyn BrowserEmit> = Arc::new(Collect::default());
        assert_eq!(
            m.open_panel("s1", w, h, dpr, emit),
            Err(format!("invalid panel size {w}x{h} at {dpr}x"))
        );
    }
    assert_eq!(m.profile_dir_for_test("s1"), None, "nothing was launched");
    m.release("s1");
}

#[test]
fn reopening_a_panel_replaces_the_view() {
    let _chromium = lock(&CHROMIUM);
    let Some((m, first, seen)) = panel("view-reopen") else {
        return;
    };
    let emit: Arc<dyn BrowserEmit> = Arc::clone(&seen) as Arc<dyn BrowserEmit>;
    m.open_panel("s1", 300, 200, 1.0, emit).unwrap();
    assert!(
        first.navigate("data:text/html,x").is_err(),
        "the old view is closed"
    );
    let second = m.panel("s1").unwrap();
    second.navigate("data:text/html,y").unwrap();
    assert!(
        eventually(SECS_3, || seen
            .frames()
            .iter()
            .any(|f| f.css_w == 300)
            .then_some(()))
        .is_some(),
        "frames of the new size"
    );
    m.close_panel("s1").unwrap();
    assert!(m.panel("s1").is_err());
    m.release("s1");
}

#[test]
fn a_close_during_a_pending_open_wins() {
    let _chromium = lock(&CHROMIUM);
    let Some(m) = manager_with_chromium("view-open-close") else {
        return;
    };
    m.allocate("s1").unwrap();
    let seen = Arc::new(Collect::default());
    let emit: Arc<dyn BrowserEmit> = Arc::clone(&seen) as Arc<dyn BrowserEmit>;
    let (reached, wait_reached) = std::sync::mpsc::channel();
    let (release, wait_release) = std::sync::mpsc::channel::<()>();
    let (view_out, view_in) = std::sync::mpsc::channel();
    let opened = std::thread::scope(|scope| {
        let opening = scope.spawn(|| {
            m.open_panel_with("s1", 400, 300, 2.0, emit, move |view| {
                view_out.send(Arc::clone(view)).unwrap();
                reached.send(()).unwrap();
                let _ = wait_release.recv_timeout(Duration::from_secs(30));
            })
        });
        wait_reached
            .recv_timeout(Duration::from_secs(30))
            .expect("the open got as far as installing");
        m.close_panel("s1").unwrap();
        release.send(()).unwrap();
        opening.join().unwrap()
    });
    assert_eq!(opened, Err("panel closed".to_owned()));
    assert!(
        m.panel("s1").is_err(),
        "no view was installed after the close"
    );
    let view = view_in.recv().unwrap();
    assert!(
        view.navigate("data:text/html,x").is_err(),
        "the late view's connection is closed"
    );
    assert_eq!(seen.last_state().unwrap().status, Status::Idle);
    m.release("s1");
}

const INPUT_PAGE: &str = "data:text/html,<input id=i autofocus>";

/// Document readiness does not imply that Chromium has applied autofocus.
fn focus_input(view: &View) {
    view.evaluate_for_test("document.getElementById('i').focus()")
        .unwrap();
    eventually(SECS_3, || {
        (view
            .evaluate_for_test("document.activeElement === document.getElementById('i')")
            .ok()?
            == json!(true))
        .then_some(())
    })
    .expect("input focused before sending text");
}

#[test]
fn queued_input_is_not_sent_to_the_page_shown_later() {
    let _chromium = lock(&CHROMIUM);
    let Some((m, view, seen)) = panel("view-stale-input") else {
        return;
    };
    let a = active_tab(&seen).unwrap();
    view.navigate(INPUT_PAGE).unwrap();
    loaded(&view, "document.getElementById('i')");
    view.tab_new().unwrap();
    let b = active_tab(&seen).unwrap();
    assert_ne!(a, b);
    view.navigate(INPUT_PAGE).unwrap();
    loaded(&view, "document.getElementById('i')");
    view.tab_activate(&a).unwrap();

    let paused = lock(&view.core.input_gate);
    view.input(&InputEvent::Text { text: "abc".into() })
        .unwrap(); // made for A
    view.tab_activate(&b).unwrap();
    drop(paused);

    focus_input(&view);
    // Input is sent in order: once "xyz" has arrived, "abc" has been dealt with.
    view.input(&InputEvent::Text { text: "xyz".into() })
        .unwrap();
    let typed = eventually(SECS_3, || {
        let value = view
            .evaluate_for_test("document.getElementById('i').value")
            .ok()?;
        (value != json!("")).then_some(value)
    });
    assert_eq!(typed, Some(json!("xyz")), "nothing made for A reached B");
    m.release("s1");
}

#[test]
fn a_tab_that_is_not_listed_is_refused_and_the_shown_one_stays() {
    let _chromium = lock(&CHROMIUM);
    let Some((m, view, seen)) = panel("view-unknown-tab") else {
        return;
    };
    let a = active_tab(&seen).unwrap();
    let state_before = seen.last_state();
    assert_eq!(
        view.tab_activate("no-such-target"),
        Err("no such tab".to_owned())
    );
    assert_eq!(
        view.tab_close("no-such-target"),
        Err("no such tab".to_owned())
    );
    assert_eq!(seen.last_state(), state_before, "nothing changed");
    assert_eq!(active_tab(&seen), Some(a), "still showing the same page");
    let frames_before = seen.frame_count();
    view.navigate("data:text/html,still-here").unwrap();
    let framed = eventually(SECS_3, || {
        (seen.frame_count() > frames_before).then_some(())
    });
    assert!(framed.is_some(), "its screencast keeps running");
    m.release("s1");
}
