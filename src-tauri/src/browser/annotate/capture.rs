//! Area capture of a pick, and scrolling back to one.

use serde::Serialize;
use serde_json::{json, Value};

use super::super::lock;
use super::super::view::{Core, Rect};
use super::pick::sane;

/// Longest side of a captured image in device pixels, like the frontend's
/// `OMP_IMAGES` `MAX_EDGE_PX`: larger areas are scaled down by Chromium.
const MAX_EDGE_PX: f64 = 2000.0;
/// Hide the overlay (counted: concurrent captures each hide and show it)
/// and tell the viewport, from the overlay's world: the page's scripts
/// cannot redefine `scrollX` there.
pub(super) const HIDE: &str = "window.__ompAnnotateHide(true)";
pub(super) const SHOW: &str = "window.__ompAnnotateHide(false)";

/// An area screenshot, base64 like the frontend's image attachments.
#[derive(Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CapturedImage {
    pub data: String,
    pub mime_type: String,
}

/// The visible part of the document, in document CSS pixels, and its
/// device pixel ratio.
#[derive(Debug, PartialEq)]
pub(super) struct Viewport {
    pub(super) area: Rect,
    pub(super) dpr: f64,
}

impl Viewport {
    /// From what the overlay's `__ompAnnotateHide` returns.
    pub(super) fn parse(value: &Value) -> Option<Self> {
        let number = |key: &str| value[key].as_f64().filter(|v| v.is_finite());
        let area = Rect {
            x: number("x")?,
            y: number("y")?,
            w: number("w")?,
            h: number("h")?,
        };
        let dpr = number("dpr").filter(|d| *d > 0.0)?;
        sane(&area).then_some(Self { area, dpr })
    }
}

/// `Page.captureScreenshot`'s clip: the document rect `doc` cut to the
/// viewport, scaled so that its longest side stays within [`MAX_EDGE_PX`].
pub(super) fn clip(doc: &Rect, viewport: &Viewport) -> Result<Value, String> {
    if !sane(doc) {
        return Err("invalid capture area".into());
    }
    let v = &viewport.area;
    let (left, top) = (doc.x.max(v.x), doc.y.max(v.y));
    let width = (doc.x + doc.w).min(v.x + v.w) - left;
    let height = (doc.y + doc.h).min(v.y + v.h) - top;
    if width < 1.0 || height < 1.0 {
        return Err("the picked area is out of view".into());
    }
    let scale = (MAX_EDGE_PX / (width.max(height) * viewport.dpr)).min(1.0);
    Ok(json!({ "x": left, "y": top, "width": width, "height": height, "scale": scale }))
}

impl Core {
    /// A PNG of `doc_rect` (a pick's document CSS pixels) as far as it is in
    /// view, taken of the document the last pick came from. The overlay is
    /// hidden for the shot, only visually: annotate stays on, so clicks keep
    /// being swallowed meanwhile.
    pub(in crate::browser) fn capture(&self, doc_rect: &Rect) -> Result<CapturedImage, String> {
        if !sane(doc_rect) {
            return Err("invalid capture area".into());
        }
        let shot = {
            // One capture at a time, and no page switch or viewport restart
            // under it: a clipped `Page.captureScreenshot` resizes the page's
            // view for the shot, so a second one (or a screencast restart)
            // running meanwhile captures the wrong pixels.
            let _switching = lock(&self.switch);
            let session = self.active_session()?;
            let context = lock(&self.annotate)
                .picked_context(&session)
                .ok_or("the page changed since the pick")?;
            let evaluate = |expression: &str| {
                self.call(
                    Some(&session),
                    "Runtime.evaluate",
                    json!({ "expression": expression, "contextId": context, "returnByValue": true }),
                )
            };
            // A gone context fails here. A hide that timed out may still
            // run later: the show below follows it on the session either way
            // (the overlay clamps its counter), so it never stays hidden.
            let shot = evaluate(HIDE)
                .map_err(|_| "the page changed since the pick".to_owned())
                .and_then(|hidden| {
                    Viewport::parse(&hidden["result"]["value"])
                        .ok_or_else(|| "could not read the page's viewport".to_owned())
                })
                .and_then(|viewport| clip(doc_rect, &viewport))
                .and_then(|clip| {
                    self.call(
                        Some(&session),
                        "Page.captureScreenshot",
                        json!({ "format": "png", "clip": clip, "captureBeyondViewport": false }),
                    )
                });
            // Whatever the outcome. A failed show means the document is gone
            // (its successor has its own overlay): the shot stands.
            let _ = evaluate(SHOW);
            shot
        };
        // After the switch lock is released: the restart takes it.
        self.refresh_frame();
        let mut shot = shot?;
        let Some(Value::String(data)) = shot.get_mut("data").map(Value::take) else {
            return Err("capture returned no image".into());
        };
        Ok(CapturedImage {
            data,
            mime_type: "image/png".to_owned(),
        })
    }

    /// Scroll the page shown now to a pick's absolute document CSS rect.
    pub(in crate::browser) fn scroll_to(&self, rect: &Rect) -> Result<(), String> {
        if !sane(rect) {
            return Err("invalid area".into());
        }
        // Only validated rect numbers travel in the expression; no selector
        // or other page-supplied text is interpolated.
        let expression = format!(
            r"window.scrollTo({{
                left: {x} + {w} / 2 - window.innerWidth / 2,
                top: {y} + {h} / 2 - window.innerHeight / 2,
                behavior: 'instant'
            }})",
            x = rect.x,
            y = rect.y,
            w = rect.w,
            h = rect.h,
        );
        self.call_active(
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true }),
        )
        .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    /// 400×300 CSS pixels scrolled to (0, 1000).
    fn viewport(dpr: f64) -> Viewport {
        Viewport {
            area: rect(0.0, 1000.0, 400.0, 300.0),
            dpr,
        }
    }

    #[test]
    fn a_capture_is_cut_to_the_viewport() {
        assert_eq!(
            clip(&rect(10.0, 1100.0, 50.0, 20.0), &viewport(1.0)).unwrap(),
            json!({ "x": 10.0, "y": 1100.0, "width": 50.0, "height": 20.0, "scale": 1.0 })
        );
        assert_eq!(
            clip(&rect(-20.0, 900.0, 1000.0, 9000.0), &viewport(1.0)).unwrap(),
            json!({ "x": 0.0, "y": 1000.0, "width": 400.0, "height": 300.0, "scale": 1.0 }),
            "a whole long page is only its visible part"
        );
        for out in [rect(0.0, 0.0, 50.0, 20.0), rect(0.0, 1299.5, 50.0, 20.0)] {
            assert_eq!(
                clip(&out, &viewport(1.0)),
                Err("the picked area is out of view".to_owned())
            );
        }
    }

    #[test]
    fn a_capture_stays_within_the_pixel_budget() {
        let big = Viewport {
            area: rect(0.0, 0.0, 1600.0, 900.0),
            dpr: 4.0,
        };
        let clip = clip(&rect(0.0, 0.0, 1600.0, 900.0), &big).unwrap();
        let scale = clip["scale"].as_f64().unwrap();
        assert!(
            1600.0f64.mul_add(4.0 * scale, -MAX_EDGE_PX).abs() < 1e-6,
            "{clip}"
        );
    }

    #[test]
    fn invalid_areas_and_viewports_are_refused() {
        for bad in [
            rect(f64::NAN, 1000.0, 5.0, 5.0),
            rect(0.0, f64::INFINITY, 5.0, 5.0),
            rect(0.0, 1000.0, f64::NAN, 5.0),
            rect(0.0, 1000.0, -5.0, 5.0),
        ] {
            assert_eq!(
                clip(&bad, &viewport(1.0)),
                Err("invalid capture area".to_owned())
            );
        }
        let good = json!({ "x": 0, "y": 10, "w": 400, "h": 300, "dpr": 2 });
        assert_eq!(
            Viewport::parse(&good),
            Some(Viewport {
                area: rect(0.0, 10.0, 400.0, 300.0),
                dpr: 2.0
            })
        );
        for bad in [
            json!(null),
            json!({ "x": 0, "y": 10, "w": 400, "h": 300, "dpr": 0 }),
            json!({ "x": 0, "y": "10", "w": 400, "h": 300, "dpr": 1 }),
            json!({ "x": 0, "y": 10, "w": -4, "h": 300, "dpr": 1 }),
        ] {
            assert_eq!(Viewport::parse(&bad), None, "{bad}");
        }
    }

    #[test]
    fn invalid_scroll_rects_are_refused_before_cdp() {
        let core = Core::new("s1", std::sync::Arc::new(super::super::tests::NoEmit));
        for n in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for bad in [
                rect(n, 0.0, 1.0, 1.0),
                rect(0.0, n, 1.0, 1.0),
                rect(0.0, 0.0, n, 1.0),
                rect(0.0, 0.0, 1.0, n),
            ] {
                assert_eq!(core.scroll_to(&bad), Err("invalid area".to_owned()));
                assert_eq!(core.capture(&bad), Err("invalid capture area".to_owned()));
            }
        }
    }
}
