//! What the overlay may report, on the reader thread: its world's
//! contexts, and picks, parsed and bounded.

use std::time::Duration;

use serde_json::Value;

use super::super::lock;
use super::super::view::{Core, Pick, Rect};
use super::{set_params, BINDING, WORLD};

/// How long after the panel released the mouse a pick counts as the user's:
/// the overlay's `mouseup` reports it within milliseconds of the release.
const PICK_AFTER_RELEASE: Duration = Duration::from_secs(2);

/// What a binding call may carry (the overlay sends at most 20 000 UTF-16
/// units of HTML, 500 of text and 8192 of url and selector, JSON-escaped).
const MAX_PAYLOAD: usize = 256 * 1024;
const MAX_HTML: usize = 20_000;
const MAX_TEXT: usize = 500;
const MAX_FIELD: usize = 8192;

/// Finite, non-negative extent, coordinates within a sane range.
pub(super) fn sane(r: &Rect) -> bool {
    [r.x, r.y, r.w, r.h]
        .iter()
        .all(|v| v.is_finite() && v.abs() <= 1e7)
        && r.w >= 0.0
        && r.h >= 0.0
}

/// A pick from the overlay's report, bounded; `None` for anything the
/// overlay would not send.
pub(super) fn parse(payload: &str) -> Option<Pick> {
    if payload.len() > MAX_PAYLOAD {
        return None;
    }
    let mut pick: Pick = serde_json::from_str(payload).ok()?;
    // The capture is made of `docRect`: a pick without one is no pick.
    if !sane(&pick.rect) || !pick.doc_rect.as_ref().is_some_and(sane) {
        return None;
    }
    cap_chars(&mut pick.url, MAX_FIELD);
    match pick.kind.as_str() {
        "element" => {
            let (selector, tag) = (pick.selector.as_mut()?, pick.tag.as_mut()?);
            cap_chars(selector, MAX_FIELD);
            cap_chars(tag, MAX_FIELD);
            if let Some(html) = pick.html.as_mut() {
                cap_chars(html, MAX_HTML);
            }
            if let Some(text) = pick.text.as_mut() {
                cap_chars(text, MAX_TEXT);
            }
        }
        "area" => {
            // The overlay reports a thinner drag as an element pick.
            if pick.rect.w < 1.0 || pick.rect.h < 1.0 {
                return None;
            }
            (pick.selector, pick.tag, pick.html, pick.text) = (None, None, None, None);
        }
        _ => return None,
    }
    Some(pick)
}

/// Keep at most `max` chars of `s`.
fn cap_chars(s: &mut String, max: usize) {
    if let Some((end, _)) = s.char_indices().nth(max) {
        s.truncate(end);
    }
}

impl Core {
    /// The reader thread's share: bookkeeping and one notification at most.
    pub(in crate::browser) fn on_overlay_event(
        &self,
        session: Option<&str>,
        method: &str,
        params: &Value,
    ) {
        if self.is_closed() {
            return;
        }
        let Some(session) = session else {
            return;
        };
        match method {
            "Runtime.executionContextCreated" => {
                let context = &params["context"];
                let Some(id) = context["id"].as_i64().filter(|_| context["name"] == WORLD) else {
                    return;
                };
                let mut annotate = lock(&self.annotate);
                if self.is_closed() {
                    return;
                }
                let on = annotate.add_context(session, id);
                if let (true, Some(client)) = (on, self.client.get()) {
                    client.notify(Some(session), "Runtime.evaluate", set_params(true, id));
                }
                drop(annotate); // No stale enable may follow a release or toggle.
            }
            "Runtime.executionContextDestroyed" => {
                if let Some(id) = params["executionContextId"].as_i64() {
                    lock(&self.annotate).drop_context(session, Some(id));
                }
            }
            "Runtime.executionContextsCleared" => {
                lock(&self.annotate).drop_context(session, None);
            }
            "Runtime.bindingCalled" => self.on_binding(session, params),
            _ => {}
        }
    }

    /// A pick from the overlay of the shown page, made by a panel click: the
    /// agent's clicks are swallowed alike, but they are not the user's picks.
    pub(super) fn on_binding(&self, session: &str, params: &Value) {
        let Some(context) = params["executionContextId"]
            .as_i64()
            .filter(|id| params["name"] == BINDING && lock(&self.annotate).owns(session, *id))
        else {
            return;
        };
        let Some(pick) = params["payload"].as_str().and_then(parse) else {
            return;
        };
        if !self.panel_released_within(PICK_AFTER_RELEASE) {
            return;
        }
        // Activation emits its tabs event under `inner`, like frames.
        // Recheck after parsing and hold that lock through the pick; and
        // `annotate` too, so a pick still queued when annotate went off is
        // dropped rather than emitted after the off.
        let inner = lock(&self.inner);
        if inner.active.as_ref().is_some_and(|a| a.session == session) {
            let mut annotate = lock(&self.annotate);
            if annotate.on {
                annotate.picked = Some((session.to_owned(), context));
                self.emit_pick(&pick);
            }
            drop(annotate);
        }
        drop(inner);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;

    fn pick(kind: &str, w: f64, extra: &Value) -> String {
        let rect = json!({ "x": 1.0, "y": 2.0, "w": w, "h": 4.0 });
        let mut p = json!({
            "kind": kind, "selector": null, "tag": null, "html": null, "text": null,
            "rect": rect, "docRect": rect, "url": "http://a/",
        });
        for (key, value) in extra.as_object().unwrap() {
            p[key] = value.clone(); // building a fixture
        }
        p.to_string()
    }

    #[test]
    fn an_element_pick_is_parsed_and_bounded() {
        let long = "é".repeat(MAX_HTML + 5);
        let payload = pick(
            "element",
            3.0,
            &json!({ "selector": "button.save", "tag": "button", "html": long, "text": long }),
        );
        let p = parse(&payload).unwrap();
        assert_eq!(p.selector.as_deref(), Some("button.save"));
        assert_eq!(p.html.unwrap().chars().count(), MAX_HTML);
        assert_eq!(p.text.unwrap().chars().count(), MAX_TEXT);
        let expected = Rect {
            x: 1.0,
            y: 2.0,
            w: 3.0,
            h: 4.0,
        };
        assert_eq!(p.rect, expected);
    }

    #[test]
    fn document_rect_is_required_validated_and_serialized_as_camel_case() {
        assert!(
            parse(&pick("area", 3.0, &json!({ "docRect": null }))).is_none(),
            "no document rect, nothing to capture"
        );
        let document = json!({ "x": 100.0, "y": 1200.0, "w": 3.0, "h": 4.0 });
        let parsed = parse(&pick("area", 3.0, &json!({ "docRect": document }))).unwrap();
        let encoded = serde_json::to_value(parsed).unwrap();
        assert_eq!(encoded["docRect"], document);
        assert!(encoded.get("doc_rect").is_none());
        for invalid in [
            json!({ "x": 0.0, "y": 0.0, "w": -1.0, "h": 4.0 }),
            json!({ "x": 0.0, "y": 0.0, "w": 3.0, "h": -1.0 }),
            json!({ "x": "NaN", "y": 0.0, "w": 3.0, "h": 4.0 }),
            json!({ "x": 1e9, "y": 0.0, "w": 3.0, "h": 4.0 }),
        ] {
            assert!(parse(&pick("area", 3.0, &json!({ "docRect": invalid }))).is_none());
        }
    }

    #[test]
    fn an_area_pick_carries_no_element_fields() {
        let payload = pick("area", 3.0, &json!({ "selector": "x", "html": "<b>" }));
        let p = parse(&payload).unwrap();
        assert_eq!(
            (p.selector, p.tag, p.html, p.text),
            (None, None, None, None)
        );
    }

    #[test]
    fn malformed_picks_are_refused() {
        assert!(parse("not json").is_none());
        assert!(parse(&pick("other", 3.0, &json!({}))).is_none());
        assert!(
            parse(&pick("element", 3.0, &json!({}))).is_none(),
            "an element without selector and tag"
        );
        assert!(
            parse(&pick("area", 0.0, &json!({}))).is_none(),
            "empty area"
        );
        assert!(
            parse(&pick("area", 0.5, &json!({}))).is_none(),
            "thinner than a pixel"
        );
        assert!(parse(&pick("area", -1.0, &json!({}))).is_none());
        let huge = pick("area", 3.0, &json!({ "url": "x".repeat(MAX_PAYLOAD) }));
        assert!(parse(&huge).is_none(), "over the payload bound");
    }

    #[test]
    fn a_lone_surrogate_escape_is_refused_so_the_overlay_sends_well_formed_text() {
        // `JSON.stringify` escapes a lone surrogate like this; serde refuses
        // it, which is why the overlay runs `toWellFormed()` on its strings.
        let payload = pick("element", 3.0, &json!({ "selector": "p", "tag": "p" }))
            .replace("http://a/", "http://a/\\ud800");
        assert!(parse(&payload).is_none());
    }
}
