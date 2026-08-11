mod app;
mod b64;
mod net;
mod sig;

use std::cell::RefCell;
use std::rc::Rc;

use app::{App, CELL, H, MINES, W};
use engine::{Event, Status};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::CanvasRenderingContext2d as Ctx;

#[wasm_bindgen(start)]
pub fn main() -> Result<(), JsValue> {
    let win = web_sys::window().ok_or("no window")?;
    let doc = win.document().ok_or("no document")?;

    // ?selftest runs the WebRTC loopback instead of the game. Not a unit test
    // — running it needs a real browser, and a real browser is the thing under
    // test.
    if win
        .location()
        .search()
        .unwrap_or_default()
        .contains("selftest")
    {
        net::note("RUNNING");
        wasm_bindgen_futures::spawn_local(async move {
            match net::loopback_selftest().await {
                Ok(s) => net::note(&s),
                Err(e) => net::note(&format!("FAIL — {e:?}")),
            }
        });
        return Ok(());
    }

    let canvas: web_sys::HtmlCanvasElement = doc
        .get_element_by_id("board")
        .ok_or("no #board canvas")?
        .dyn_into()?;
    let ctx: Ctx = canvas
        .get_context("2d")?
        .ok_or("no 2d context")?
        .dyn_into()?;

    let shared: app::Shared = Rc::new(RefCell::new(App::new(ctx, (now() as u64) | 1)));
    shared.borrow().draw();

    {
        let (shared, c) = (shared.clone(), canvas.clone());
        let on_down =
            Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |e: web_sys::MouseEvent| {
                let player = shared.borrow().player;
                let ev = if shared.borrow().game.status() != Status::Playing {
                    // Any click on a finished board starts a new one — and the
                    // Start travels, so the peer restarts with the same seed.
                    Event::Start {
                        seed: (now() as u64) | 1,
                        w: W,
                        h: H,
                        mines: MINES,
                    }
                } else {
                    let Some((x, y)) = cell_at(&c, &e) else {
                        return;
                    };
                    match e.button() {
                        2 => Event::Flag { player, x, y },
                        _ => Event::Reveal { player, x, y },
                    }
                };
                app::local(&shared, ev);
            });
        canvas.add_event_listener_with_callback("mousedown", on_down.as_ref().unchecked_ref())?;
        on_down.forget();
    }

    // Otherwise right-click opens the browser menu instead of flagging.
    let block = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(|e: web_sys::MouseEvent| {
        e.prevent_default();
    });
    canvas.add_event_listener_with_callback("contextmenu", block.as_ref().unchecked_ref())?;
    block.forget();

    sig::wire(&doc, shared)?;

    Ok(())
}

#[wasm_bindgen]
extern "C" {
    /// Enough entropy for a local seed, and no extra web-sys feature flags.
    #[wasm_bindgen(js_namespace = Date)]
    fn now() -> f64;
}

/// Pixel -> cell. `get_bounding_client_rect` is what makes this survive CSS
/// scaling, page scroll, and high-DPI displays.
fn cell_at(canvas: &web_sys::HtmlCanvasElement, e: &web_sys::MouseEvent) -> Option<(u8, u8)> {
    let r = canvas.get_bounding_client_rect();
    let sx = canvas.width() as f64 / r.width();
    let sy = canvas.height() as f64 / r.height();
    let x = ((e.client_x() as f64 - r.left()) * sx / CELL).floor();
    let y = ((e.client_y() as f64 - r.top()) * sy / CELL).floor();
    if x < 0.0 || y < 0.0 || x >= W as f64 || y >= H as f64 {
        return None;
    }
    Some((x as u8, y as u8))
}
