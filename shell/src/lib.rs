use std::cell::RefCell;
use std::rc::Rc;

use engine::{Event, Game, Reveal, Status};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::CanvasRenderingContext2d as Ctx;

const CELL: f64 = 32.0;
const W: u8 = 9;
const H: u8 = 9;
const MINES: u16 = 10;

/// Classic Minesweeper digit colours. Index 0 is unused.
const DIGITS: [&str; 9] = [
    "", "#2b52c8", "#2e7d32", "#c0392b", "#1f3070", "#8c3b2e", "#17727a", "#2b3038", "#6d7684",
];

#[wasm_bindgen(start)]
pub fn main() -> Result<(), JsValue> {
    let doc = web_sys::window()
        .ok_or("no window")?
        .document()
        .ok_or("no document")?;
    let canvas: web_sys::HtmlCanvasElement = doc
        .get_element_by_id("board")
        .ok_or("no #board canvas")?
        .dyn_into()?;
    let ctx: Ctx = canvas
        .get_context("2d")?
        .ok_or("no 2d context")?
        .dyn_into()?;

    // The event log is the game. `game` is the fold, kept alongside it so we
    // don't replay from scratch on every click.
    let seed = (now() as u64) | 1;
    let log = Rc::new(RefCell::new(vec![Event::Start {
        seed,
        w: W,
        h: H,
        mines: MINES,
    }]));
    let game = Rc::new(RefCell::new(Game::new(seed, W, H, MINES)));

    draw(&ctx, &game.borrow());

    {
        let (log, game, ctx, c) = (log.clone(), game.clone(), ctx.clone(), canvas.clone());
        let on_down = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |e: web_sys::MouseEvent| {
            let ev = if game.borrow().status() != Status::Playing {
                // Any click on a finished board starts a new one.
                let seed = log.borrow().len() as u64 ^ 0x5DEE_CE66_D125;
                Event::Start { seed, w: W, h: H, mines: MINES }
            } else {
                let Some((x, y)) = cell_at(&c, &e) else { return };
                match e.button() {
                    2 => Event::Flag { player: 0, x, y },
                    _ => Event::Reveal { player: 0, x, y },
                }
            };
            // Append first, then fold. In phase 2 the append also sends.
            log.borrow_mut().push(ev);
            game.borrow_mut().apply(&ev);
            draw(&ctx, &game.borrow());
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

fn draw(ctx: &Ctx, game: &Game) {
    let b = &game.board;
    let over = game.status() != Status::Playing;

    for y in 0..b.h {
        for x in 0..b.w {
            let c = b.get(x, y);
            let (px, py) = (x as f64 * CELL, y as f64 * CELL);

            // Once the game is over, mines come into view.
            let fill = match (over && c.mine, c.state) {
                (true, Reveal::Shown) => "#c0392b", // the one you hit
                (true, _) => "#e8a5a0",             // the ones you missed
                (false, Reveal::Shown) => "#dfe2e7",
                (false, _) => "#b6bcc6",
            };
            ctx.set_fill_style_str(fill);
            ctx.fill_rect(px, py, CELL - 1.0, CELL - 1.0);

            if c.state == Reveal::Flagged {
                ctx.set_fill_style_str("#c0392b");
                ctx.fill_rect(px + 11.0, py + 8.0, 11.0, 13.0);
            } else if c.state == Reveal::Shown && !c.mine && c.adj > 0 {
                ctx.set_fill_style_str(DIGITS[c.adj as usize]);
                ctx.set_font("bold 20px monospace");
                let _ = ctx.fill_text(&c.adj.to_string(), px + 10.0, py + 24.0);
            }
        }
    }

    if over {
        let (msg, colour) = match game.status() {
            Status::Won => ("YOU WIN — click to restart", "#2e7d32"),
            _ => ("BOOM — click to restart", "#c0392b"),
        };
        let w = b.w as f64 * CELL;
        ctx.set_fill_style_str("rgba(20, 24, 32, 0.82)");
        ctx.fill_rect(0.0, w / 2.0 - 22.0, w, 44.0);
        ctx.set_fill_style_str(colour);
        ctx.set_font("bold 15px monospace");
        let _ = ctx.fill_text(msg, 14.0, w / 2.0 + 6.0);
    }
}
