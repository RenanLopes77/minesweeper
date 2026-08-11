//! Shared game state, and the two ways it changes: a local click, or bytes
//! arriving from the peer.
//!
//! Both paths do the same three things in the same order — append to the log,
//! fold the event into the board, redraw. The only asymmetry is that a local
//! move is also sent, and a remote move is not sent back.

use std::cell::RefCell;
use std::rc::Rc;

use engine::{Event, Game, Msg, Reveal, Status, decode_msg, encode_msg};
use web_sys::{CanvasRenderingContext2d as Ctx, RtcDataChannel};

use crate::net;

pub const CELL: f64 = 32.0;
/// Canvas bitmap pixels per logical pixel. The canvas element is 2x the board
/// so CSS can shrink it to the viewport without softening the digits.
pub const SCALE: f64 = 2.0;
pub const W: u8 = 9;
pub const H: u8 = 9;
pub const MINES: u16 = 10;

/// Classic Minesweeper digit colours. Index 0 is unused.
const DIGITS: [&str; 9] = [
    "", "#2b52c8", "#2e7d32", "#c0392b", "#1f3070", "#8c3b2e", "#17727a", "#2b3038", "#6d7684",
];

pub struct App {
    pub game: Game,
    pub log: Vec<Event>,
    /// 0 for the host, 1 for the joiner. Unused by the rules; it rides along
    /// in every event so a later chapter can attribute moves.
    pub player: u8,
    /// Touch has no right button, so flagging needs a mode instead of a
    /// modifier. It also works with a mouse.
    pub flag_mode: bool,
    pub chan: Option<RtcDataChannel>,
    ctx: Ctx,
}

pub type Shared = Rc<RefCell<App>>;

impl App {
    pub fn new(ctx: Ctx, seed: u64) -> Self {
        App {
            game: Game::new(seed, W, H, MINES),
            log: vec![Event::Start {
                seed,
                w: W,
                h: H,
                mines: MINES,
            }],
            player: 0,
            flag_mode: false,
            chan: None,
            ctx,
        }
    }

    fn send(&self, msg: &Msg) {
        if let Some(ch) = self
            .chan
            .as_ref()
            .filter(|c| c.ready_state() == web_sys::RtcDataChannelState::Open)
        {
            let _ = ch.send_with_u8_array(&encode_msg(msg));
        }
    }

    /// Announces what our board looks like now. The peer compares it against
    /// its own and shouts if they differ — eight bytes instead of a board.
    fn send_state(&self) {
        self.send(&Msg::State {
            count: self.log.len() as u32,
            hash: self.game.hash(),
        });
    }
}

/// A move made on this device.
pub fn local(app: &Shared, ev: Event) {
    let mut a = app.borrow_mut();
    a.log.push(ev);
    a.game.apply(&ev);
    a.send(&Msg::Events(vec![ev]));
    a.send_state();
    a.draw();
}

/// Bytes from the peer. This is a trust boundary: `decode_log` returns None
/// for anything malformed and we drop the message rather than acting on half
/// of it.
pub fn remote(app: &Shared, bytes: &[u8]) {
    let Some(msg) = decode_msg(bytes) else {
        net::log(&format!("dropped {} malformed bytes", bytes.len()));
        return;
    };
    let mut a = app.borrow_mut();

    match msg {
        Msg::Events(events) => {
            // A message that opens with Start is a whole log, not a move: the
            // host sends one when the channel opens so the joiner adopts its
            // seed. Start can only ever mean "here is the game".
            if matches!(events.first(), Some(Event::Start { .. })) {
                if let Some(g) = Game::replay(&events) {
                    net::log(&format!("adopted host log, {} events", events.len()));
                    a.game = g;
                    a.log = events;
                }
            } else {
                for ev in &events {
                    a.game.apply(ev);
                    a.log.push(*ev);
                }
            }
            a.draw();
            // Answer with where that left us, so they can check too.
            a.send_state();
        }
        Msg::State { count, hash } => {
            // Only meaningful at the same point in the log. Different counts
            // mean one side is simply behind, which happens constantly.
            if count as usize != a.log.len() {
                return;
            }
            let ours = a.game.hash();
            if ours == hash {
                return;
            }
            // The two boards disagree. Recovery is ch16; for now, say so
            // loudly rather than letting the players discover it by losing.
            net::note("DESYNC — boards no longer match");
            net::log(&format!(
                "desync at event {count}: ours {ours:016x}, theirs {hash:016x}"
            ));
        }
    }
}

/// Called when the channel opens. The host hands over the log it already has,
/// which is what makes both sides agree on the seed.
pub fn on_connect(app: &Shared, is_host: bool) {
    let mut a = app.borrow_mut();
    a.player = if is_host { 0 } else { 1 };
    if is_host {
        let log = a.log.clone();
        net::log(&format!("sent log, {} events", log.len()));
        a.send(&Msg::Events(log));
        a.send_state();
    }
}

impl App {
    pub fn draw(&self) {
        let ctx = &self.ctx;
        let b = &self.game.board;
        let over = self.game.status() != Status::Playing;

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
            let (msg, colour) = match self.game.status() {
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
}
