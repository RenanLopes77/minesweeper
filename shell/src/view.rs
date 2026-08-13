//! Pinch-zoom and pan for the board.
//!
//! Expert is 30 columns; on a phone that is ~16px a cell however the CSS
//! slices it. The fix is not smaller cells, it is a magnifier: two fingers
//! zoom and drag the board inside a clipping viewport, one finger stays a
//! click. The zoom is a CSS transform on the canvas — `cell_at` in lib.rs
//! needs no changes, because `get_bounding_client_rect` reports the
//! transformed box and the arithmetic scales with it.
//!
//! Mouse users get nothing here on purpose: every board already fits a
//! desktop, and a wheel-zoom would fight the page scroll.

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{Document, HtmlElement, TouchEvent};

const MAX_ZOOM: f64 = 5.0;

/// The whole state of the magnifier: how much, and which corner of the board
/// sits where. `scale >= 1` always — zooming *out* past the fitted size only
/// makes cells smaller than the problem this exists to solve.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct View {
    scale: f64,
    tx: f64,
    ty: f64,
}

impl Default for View {
    fn default() -> Self {
        View {
            scale: 1.0,
            tx: 0.0,
            ty: 0.0,
        }
    }
}

/// Where a two-finger gesture started: the distance between the fingers, the
/// point midway between them, and the view at that moment. Every move is
/// computed from here rather than from the previous move, so error cannot
/// accumulate across a gesture.
#[derive(Clone, Copy)]
struct Grip {
    dist: f64,
    mid: (f64, f64),
    view: View,
}

/// The view after a pinch step, pure so the native tests can reach it.
///
/// The invariant is the one fingers expect: the piece of board that was
/// under the midpoint when the gesture began is under the midpoint now —
/// that single rule is both zoom-about-a-point and two-finger pan. The
/// translation is then clamped so the board always fills the viewport
/// (`bw`, `bh`: the board's unzoomed size, which is also the viewport's).
fn pinched(g: Grip, dist: f64, mid: (f64, f64), bw: f64, bh: f64) -> View {
    // Fingers on one spot would divide by zero; a pixel is close enough.
    let scale = (g.view.scale * dist / g.dist.max(1.0)).clamp(1.0, MAX_ZOOM);
    let wx = (g.mid.0 - g.view.tx) / g.view.scale;
    let wy = (g.mid.1 - g.view.ty) / g.view.scale;
    View {
        scale,
        tx: (mid.0 - wx * scale).clamp(bw * (1.0 - scale), 0.0),
        ty: (mid.1 - wy * scale).clamp(bh * (1.0 - scale), 0.0),
    }
}

/// The two touches of a gesture, as distance and midpoint relative to the
/// viewport's corner.
fn fingers(e: &TouchEvent, origin: (f64, f64)) -> Option<(f64, (f64, f64))> {
    let (a, b) = (e.touches().item(0)?, e.touches().item(1)?);
    let (ax, ay) = (
        a.client_x() as f64 - origin.0,
        a.client_y() as f64 - origin.1,
    );
    let (bx, by) = (
        b.client_x() as f64 - origin.0,
        b.client_y() as f64 - origin.1,
    );
    Some((
        ((ax - bx).powi(2) + (ay - by).powi(2)).sqrt(),
        ((ax + bx) / 2.0, (ay + by) / 2.0),
    ))
}

pub fn wire(doc: &Document) -> Result<(), JsValue> {
    let wrap: HtmlElement = doc
        .get_element_by_id("viewport")
        .ok_or("no #viewport")?
        .dyn_into()?;
    let board: HtmlElement = doc
        .get_element_by_id("board")
        .ok_or("no #board")?
        .dyn_into()?;

    let view = Rc::new(Cell::new(View::default()));
    let grip: Rc<Cell<Option<Grip>>> = Rc::new(Cell::new(None));

    let apply = {
        let board = board.clone();
        move |v: View| {
            let _ = board.style().set_property(
                "transform",
                &format!("translate({}px, {}px) scale({})", v.tx, v.ty, v.scale),
            );
        }
    };

    // Two fingers down: remember where the gesture starts. One finger is not
    // ours — and is not preventDefault'ed, or the browser would stop turning
    // taps into the mousedown that plays the game.
    {
        let (wrap2, view, grip) = (wrap.clone(), view.clone(), grip.clone());
        let cb = Closure::<dyn FnMut(TouchEvent)>::new(move |e: TouchEvent| {
            let r = wrap2.get_bounding_client_rect();
            let Some((dist, mid)) = fingers(&e, (r.left(), r.top())) else {
                return;
            };
            e.prevent_default();
            grip.set(Some(Grip {
                dist,
                mid,
                view: view.get(),
            }));
        });
        wrap.add_event_listener_with_callback("touchstart", cb.as_ref().unchecked_ref())?;
        cb.forget();
    }

    {
        let (wrap2, board2, view, grip) = (wrap.clone(), board.clone(), view.clone(), grip.clone());
        let apply = apply.clone();
        let cb = Closure::<dyn FnMut(TouchEvent)>::new(move |e: TouchEvent| {
            let Some(g) = grip.get() else {
                return;
            };
            let r = wrap2.get_bounding_client_rect();
            let Some((dist, mid)) = fingers(&e, (r.left(), r.top())) else {
                return;
            };
            e.prevent_default();
            // The board's *layout* size: offset_width ignores the transform,
            // which is exactly the frame the clamp needs.
            let v = pinched(
                g,
                dist,
                mid,
                board2.offset_width() as f64,
                board2.offset_height() as f64,
            );
            view.set(v);
            apply(v);
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

    fn grip(view: View, dist: f64, mid: (f64, f64)) -> Grip {
        Grip { dist, mid, view }
    }

    /// Fingers spreading to twice the distance doubles the zoom, and the
    /// board point under the fingers stays under the fingers.
    #[test]
    fn spreading_fingers_zooms_about_their_midpoint() {
        let g = grip(View::default(), 100.0, (150.0, 150.0));
        let v = pinched(g, 200.0, (150.0, 150.0), 300.0, 300.0);
        assert_eq!(v.scale, 2.0);
        // The world point at the midpoint was (150, 150); at scale 2 it sits
        // at 150*2 + tx = 150, so tx = -150.
        assert_eq!((v.tx, v.ty), (-150.0, -150.0));
    }

    #[test]
    fn moving_both_fingers_pans() {
        let zoomed = View {
            scale: 2.0,
            tx: -150.0,
            ty: -150.0,
        };
        let g = grip(zoomed, 100.0, (150.0, 150.0));
        let v = pinched(g, 100.0, (100.0, 150.0), 300.0, 300.0);
        assert_eq!(v.scale, 2.0, "distance unchanged, zoom unchanged");
        assert_eq!((v.tx, v.ty), (-200.0, -150.0), "board followed the drag");
    }

    /// The board can never be dragged off screen or zoomed out past fitting:
    /// scale stays in [1, MAX_ZOOM] and the viewport stays full of board.
    #[test]
    fn zoom_and_pan_are_clamped() {
        let g = grip(View::default(), 100.0, (0.0, 0.0));
        assert_eq!(pinched(g, 1.0, (0.0, 0.0), 300.0, 300.0), View::default());
        let v = pinched(g, 1e9, (0.0, 0.0), 300.0, 300.0);
        assert_eq!(v.scale, MAX_ZOOM);

        // Dragging far right-down pins the board's top-left corner.
        let zoomed = View {
            scale: 2.0,
            tx: -150.0,
            ty: -150.0,
        };
        let g = grip(zoomed, 100.0, (150.0, 150.0));
        let v = pinched(g, 100.0, (1e6, 1e6), 300.0, 300.0);
        assert_eq!((v.tx, v.ty), (0.0, 0.0));
        // And far left-up pins the bottom-right: bw(1 - s) = -300.
        let v = pinched(g, 100.0, (-1e6, -1e6), 300.0, 300.0);
        assert_eq!((v.tx, v.ty), (-300.0, -300.0));
    }

    /// Zero finger distance must not divide the view into NaN.
    #[test]
    fn fingers_on_one_spot_do_not_explode() {
        let g = grip(View::default(), 0.0, (10.0, 10.0));
        let v = pinched(g, 50.0, (10.0, 10.0), 300.0, 300.0);
        assert!(v.scale.is_finite() && v.tx.is_finite() && v.ty.is_finite());
    }
}
