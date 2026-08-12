mod app;
mod b64;
mod net;
mod qr;
mod sig;
mod zip;

use std::cell::RefCell;
use std::rc::Rc;

use app::{App, CELL, LEVELS};
use engine::{Event, Mode};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::CanvasRenderingContext2d as Ctx;

thread_local! {
    /// The live app, so the browser tests can ask what the board actually is.
    /// Nothing in the game reads this — see `debug_hash` below.
    static APP: RefCell<Option<app::Shared>> = const { RefCell::new(None) };
}

/// The board hash, as hex, for the end-to-end tests. Two peers agreeing on
/// this is the entire sync guarantee, and it is otherwise invisible from JS.
#[wasm_bindgen]
pub fn debug_hash() -> String {
    APP.with(|a| match a.borrow().as_ref() {
        Some(app) => format!("{:016x}", app.borrow().game.hash()),
        None => String::new(),
    })
}

/// How many events are in the log. Tells a test whether a merge landed.
#[wasm_bindgen]
pub fn debug_log_len() -> usize {
    APP.with(|a| a.borrow().as_ref().map_or(0, |app| app.borrow().log.len()))
}

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
    // The bitmap is 2x the logical board (see index.html); `draw` sizes the
    // canvas and reapplies that scale, because the board can change size.
    let shared: app::Shared = Rc::new(RefCell::new(App::new(
        ctx,
        canvas.clone(),
        (now() as u64) | 1,
    )));
    APP.with(|a| *a.borrow_mut() = Some(shared.clone()));
    shared.borrow().draw();

    {
        let (shared, c) = (shared.clone(), canvas.clone());
        let on_down =
            Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |e: web_sys::MouseEvent| {
                let player = shared.borrow().player;
                // A finished board is done: New game restarts it. Clicking on
                // it used to restart by accident. `over` rather than the board's
                // own status, or a race that someone else has already won keeps
                // taking moves and broadcasting them.
                if shared.borrow().over() {
                    return;
                }
                let (w, h) = {
                    let b = &shared.borrow().game.board;
                    (b.w, b.h)
                };
                let Some((x, y)) = cell_at(&c, &e, w, h) else {
                    return;
                };
                let ev = if e.button() == 2 || shared.borrow().flag_mode {
                    Event::Flag { player, x, y }
                } else {
                    Event::Reveal { player, x, y }
                };
                app::local(&shared, ev);
            });
        canvas.add_event_listener_with_callback("mousedown", on_down.as_ref().unchecked_ref())?;
        on_down.forget();
    }

    {
        let btn: web_sys::HtmlElement = doc
            .get_element_by_id("flag")
            .ok_or("no #flag button")?
            .dyn_into()?;
        let (shared, b) = (shared.clone(), btn.clone());
        let on_flag = Closure::<dyn FnMut()>::new(move || {
            let on = {
                let mut a = shared.borrow_mut();
                a.flag_mode = !a.flag_mode;
                a.flag_mode
            };
            b.set_text_content(Some(if on {
                "Flag mode: ON"
            } else {
                "Flag mode: off"
            }));
            b.style()
                .set_property("background", if on { "#c0392b" } else { "#2b3038" })
                .ok();
        });
        btn.add_event_listener_with_callback("click", on_flag.as_ref().unchecked_ref())?;
        on_flag.forget();
    }

    // New game, at whatever the picker says. The Start travels, so the peer
    // restarts on the same board — difficulty is agreed by whoever presses it.
    {
        let level: web_sys::HtmlSelectElement = doc
            .get_element_by_id("level")
            .ok_or("no #level")?
            .dyn_into()?;
        let mode_sel: web_sys::HtmlSelectElement = doc
            .get_element_by_id("mode")
            .ok_or("no #mode")?
            .dyn_into()?;
        // Built here rather than written out in the page: the picker's order
        // is the wire's order, and a mode added to the enum appears by itself.
        for mode in Mode::ALL {
            let opt = doc.create_element("option")?;
            opt.set_text_content(Some(mode.label()));
            mode_sel.append_child(&opt)?;
        }
        let shared = shared.clone();
        let restart = Closure::<dyn FnMut()>::new(move || {
            let i = (level.selected_index().max(0) as usize).min(LEVELS.len() - 1);
            let (w, h, mines) = LEVELS[i];
            // The picker's order is engine::Mode's order; anything else the
            // DOM could hand us falls back to co-op rather than guessing.
            let mode = Mode::from_u8(mode_sel.selected_index().max(0) as u8).unwrap_or_default();
            app::local(
                &shared,
                Event::Start {
                    seed: (now() as u64) | 1,
                    w,
                    h,
                    mines,
                    mode,
                },
            );
        });
        doc.get_element_by_id("restart")
            .ok_or("no #restart")?
            .add_event_listener_with_callback("click", restart.as_ref().unchecked_ref())?;
        restart.forget();
    }

    // The clock has to move between moves, and a move is the only other thing
    // that redraws. One interval for the life of the page.
    {
        let shared = shared.clone();
        let tick = Closure::<dyn FnMut()>::new(move || app::tick(&shared));
        win.set_interval_with_callback_and_timeout_and_arguments_0(
            tick.as_ref().unchecked_ref(),
            1000,
        )?;
        tick.forget();
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
fn cell_at(
    canvas: &web_sys::HtmlCanvasElement,
    e: &web_sys::MouseEvent,
    w: u8,
    h: u8,
) -> Option<(u8, u8)> {
    let r = canvas.get_bounding_client_rect();
    let sx = w as f64 * CELL / r.width();
    let sy = h as f64 * CELL / r.height();
    let x = ((e.client_x() as f64 - r.left()) * sx / CELL).floor();
    let y = ((e.client_y() as f64 - r.top()) * sy / CELL).floor();
    if x < 0.0 || y < 0.0 || x >= w as f64 || y >= h as f64 {
        return None;
    }
    Some((x as u8, y as u8))
}
