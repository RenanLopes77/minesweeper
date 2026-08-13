//! Pinch-zoom and pan for the board.
//!
//! Expert is 30 columns; on a phone that is ~16px a cell however the CSS
//! slices it. Two fingers zoom, and the zoom is *layout*, not a transform:
//! the canvas's CSS width is multiplied, the page grows with it, and panning
//! is ordinary scrolling — vertically the page itself, horizontally the
//! `#viewport` frame around the canvas. Nothing is cropped away behind a
//! clipped box, and the controls scroll off-screen exactly like any other
//! content, which is what summons the floating flag button (lib.rs).
//!
//! One finger is left strictly alone: it stays a click.
//!
//! Mouse users get nothing here on purpose: every board already fits a
//! desktop, and a wheel-zoom would fight the page scroll.

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{Document, Element, HtmlElement, TouchEvent};

const MAX_ZOOM: f64 = 5.0;

/// Where a two-finger gesture started: the distance between the fingers, the
/// point of the *displayed* canvas under their midpoint, and the zoom at that
/// moment. Every move is computed from here rather than from the previous
/// move, so error cannot accumulate across a gesture.
#[derive(Clone, Copy)]
struct Grip {
    dist: f64,
    p0: (f64, f64),
    z0: f64,
}

/// The zoom after a pinch step, and where the canvas's top-left corner must
/// land (in client coordinates) to honour it. Pure so the native tests can
/// reach it.
///
/// The invariant is the one fingers expect: the piece of board that was
/// under the midpoint when the gesture began is under the midpoint now —
/// that single rule is both zoom-about-a-point and two-finger pan. `p0`
/// scales by the ratio the zoom changed, and the corner is wherever puts the
/// scaled point under the fingers. The caller turns the corner into scroll
/// positions, which the browser then clamps to the page for free.
fn zoomed(g: Grip, dist: f64, mid: (f64, f64)) -> (f64, (f64, f64)) {
    // Fingers on one spot would divide by zero; a pixel is close enough.
    let z = (g.z0 * dist / g.dist.max(1.0)).clamp(1.0, MAX_ZOOM);
    let r = z / g.z0;
    (z, (mid.0 - g.p0.0 * r, mid.1 - g.p0.1 * r))
}

/// The two touches of a gesture: their distance and client midpoint.
fn fingers(e: &TouchEvent) -> Option<(f64, (f64, f64))> {
    let (a, b) = (e.touches().item(0)?, e.touches().item(1)?);
    let (ax, ay) = (a.client_x() as f64, a.client_y() as f64);
    let (bx, by) = (b.client_x() as f64, b.client_y() as f64);
    Some((
        ((ax - bx).powi(2) + (ay - by).powi(2)).sqrt(),
        ((ax + bx) / 2.0, (ay + by) / 2.0),
    ))
}

pub fn wire(doc: &Document) -> Result<(), JsValue> {
    let wrap: Element = doc.get_element_by_id("viewport").ok_or("no #viewport")?;
    let board: HtmlElement = doc
        .get_element_by_id("board")
        .ok_or("no #board")?
        .dyn_into()?;

    let zoom = Rc::new(Cell::new(1.0f64));
    let grip: Rc<Cell<Option<Grip>>> = Rc::new(Cell::new(None));

    // Two fingers down: remember where the gesture starts. One finger is not
    // ours — and is not preventDefault'ed, or the browser would stop turning
    // taps into the mousedown that plays the game.
    {
        let (board2, zoom, grip) = (board.clone(), zoom.clone(), grip.clone());
        let cb = Closure::<dyn FnMut(TouchEvent)>::new(move |e: TouchEvent| {
            let Some((dist, mid)) = fingers(&e) else {
                return;
            };
            e.prevent_default();
            let r = board2.get_bounding_client_rect();
            grip.set(Some(Grip {
                dist,
                p0: (mid.0 - r.left(), mid.1 - r.top()),
                z0: zoom.get(),
            }));
        });
        wrap.add_event_listener_with_callback("touchstart", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }

    {
        let (wrap2, board2, zoom, grip) = (wrap.clone(), board.clone(), zoom.clone(), grip.clone());
        let cb = Closure::<dyn FnMut(TouchEvent)>::new(move |e: TouchEvent| {
            let Some(g) = grip.get() else {
                return;
            };
            let Some((dist, mid)) = fingers(&e) else {
                return;
            };
            e.prevent_default();
            let (z, corner) = zoomed(g, dist, mid);
            zoom.set(z);
            // Layout zoom: the CSS that sizes the board at 1x keeps doing so,
            // multiplied — a restart at another difficulty re-derives from
            // the same var and the zoom survives it.
            let _ = board2
                .style()
                .set_property("width", &format!("calc(var(--board) * {z})"));
            // The write reflowed the page; read where the corner landed and
            // scroll the difference. The browser clamps both to the page.
            let fresh = board2.get_bounding_client_rect();
            wrap2.set_scroll_left(wrap2.scroll_left() + (fresh.left() - corner.0) as i32);
            if let Some(win) = web_sys::window() {
                win.scroll_by_with_x_and_y(0.0, fresh.top() - corner.1);
            }
        });
        wrap.add_event_listener_with_callback("touchmove", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }

    // A finger lifting ends the gesture. The remaining finger must not pan on
    // its own — releasing a pinch never leaves both fingers at once, and the
    // half-second straggler would smear the board.
    for ev in ["touchend", "touchcancel"] {
        let grip = grip.clone();
        let cb = Closure::<dyn FnMut(TouchEvent)>::new(move |e: TouchEvent| {
            if e.touches().length() < 2 {
                grip.set(None);
            }
        });
        wrap.add_event_listener_with_callback(ev, cb.as_ref().unchecked_ref())?;
        cb.forget();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fingers spreading to twice the distance doubles the zoom, and the
    /// board point under the fingers stays under the fingers: it was 100px
    /// into the canvas, it is now 200px in, so the corner asks to sit 200px
    /// before the unmoved midpoint.
    #[test]
    fn spreading_fingers_zooms_about_their_midpoint() {
        let g = Grip {
            dist: 100.0,
            p0: (100.0, 100.0),
            z0: 1.0,
        };
        let (z, corner) = zoomed(g, 200.0, (150.0, 150.0));
        assert_eq!(z, 2.0);
        assert_eq!(corner, (-50.0, -50.0));
    }

    /// Distance unchanged, midpoint dragged: pure pan, corner follows.
    #[test]
    fn moving_both_fingers_pans() {
        let g = Grip {
            dist: 100.0,
            p0: (100.0, 100.0),
            z0: 2.0,
        };
        let (z, corner) = zoomed(g, 100.0, (120.0, 90.0));
        assert_eq!(z, 2.0);
        assert_eq!(corner, (20.0, -10.0), "corner moved exactly with the drag");
    }

    /// Zoom is clamped to [1, MAX_ZOOM] whatever the fingers claim, and a
    /// clamped zoom still anchors: at the floor, p0 shrinks by z0 -> 1.
    #[test]
    fn zoom_is_clamped_but_still_anchored() {
        let g = Grip {
            dist: 100.0,
            p0: (100.0, 100.0),
            z0: 2.0,
        };
        let (z, corner) = zoomed(g, 1.0, (150.0, 150.0));
        assert_eq!(z, 1.0);
        assert_eq!(corner, (100.0, 100.0), "p0 halved with the zoom");
        assert_eq!(zoomed(g, 1e9, (0.0, 0.0)).0, MAX_ZOOM);
    }

    /// Zero finger distance must not divide the zoom into NaN.
    #[test]
    fn fingers_on_one_spot_do_not_explode() {
        let g = Grip {
            dist: 0.0,
            p0: (10.0, 10.0),
            z0: 1.0,
        };
        let (z, corner) = zoomed(g, 50.0, (10.0, 10.0));
        assert!(z.is_finite() && corner.0.is_finite() && corner.1.is_finite());
    }
}
