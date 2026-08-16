//! Shared game state, and the two ways it changes: a local click, or bytes
//! arriving from the peer.
//!
//! Both paths do the same three things in the same order — append to the log,
//! fold the event into the board, redraw. The only asymmetry is that a local
//! move is also sent, and a remote move is not sent back.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use engine::{
    Board, Event, Game, Mode, Msg, Reveal, Stamped, Status, decode_msg, encode_msg, mode_of,
    race_fold,
};
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d as Ctx, RtcDataChannel};

use p2p_link::net;

pub const CELL: f64 = 32.0;
/// Canvas bitmap pixels per logical pixel. The canvas element is 2x the board
/// so CSS can shrink it to the viewport without softening the digits.
pub const SCALE: f64 = 2.0;

/// `(w, h, mines, name)`. The picker is built from this, so the label and the
/// numbers cannot drift apart — the page used to spell them out separately.
/// The classic three; anything else is a number nobody has an intuition for.
pub const LEVELS: [(u8, u8, u16, &str); 3] = [
    (9, 9, 10, "Beginner"),
    (16, 16, 40, "Intermediate"),
    (30, 16, 99, "Expert"),
];

/// One colour each. Player 0 hosts, player 1 joins.
pub const PLAYERS: [(&str, &str); 2] = [("#c0392b", "red"), ("#2b6fc8", "blue")];

/// Who touched which cell, and where each player last played. Derived from the
/// log on every draw instead of being stored, so it cannot drift from the
/// board — the log is already the source of truth.
#[derive(Default)]
pub struct Presence {
    pub owner: HashMap<(u8, u8), u8>,
    pub last: [Option<(u8, u8)>; 2],
}

pub fn presence(log: &[Stamped]) -> Presence {
    let mut p = Presence::default();
    for s in log {
        match s.ev {
            // A new board wipes what came before it.
            Event::Start { .. } => p = Presence::default(),
            Event::Reveal { player, x, y } | Event::Flag { player, x, y } => {
                p.owner.insert((x, y), player);
                if let Some(slot) = p.last.get_mut(player as usize) {
                    *slot = Some((x, y));
                }
            }
        }
    }
    p
}

/// Classic Minesweeper digit colours. Index 0 is unused.
const DIGITS: [&str; 9] = [
    "", "#2b52c8", "#2e7d32", "#c0392b", "#1f3070", "#8c3b2e", "#17727a", "#2b3038", "#6d7684",
];

/// The most events we will fold. A peer can send a log as large as it likes,
/// and every message re-folds the whole thing — a few thousand moves is far
/// past any real game and still refolds in microseconds.
pub const MAX_LOG: usize = 20_000;

/// Is this event the opening of a game that is not ours? A handover always
/// starts one: the first event of a log, a `Start` at seq 0. A restart is a
/// move and carries seq >= 1, which is what keeps the two apart.
pub fn is_foreign_start(s: &Stamped, ours: Option<&Stamped>) -> bool {
    s.seq == 0 && matches!(s.ev, Event::Start { .. }) && Some(s) != ours
}

/// A `Start` is what a log may open with — what `eventlog::sanitised` demands
/// at seq 0 before a peer's log is adopted wholesale.
fn opens(ev: &Event) -> bool {
    matches!(ev, Event::Start { .. })
}

pub struct App {
    pub game: Game,
    /// The log and its Lamport clock, so a move we make now always sorts
    /// after everything we knew about when we made it.
    pub log: eventlog::Log<Event>,
    /// 0 for the host, 1 for the joiner. Unused by the rules; it rides along
    /// in every event so a later chapter can attribute moves.
    pub player: u8,
    /// Touch has no right button, so flagging needs a mode instead of a
    /// modifier. It also works with a mouse.
    pub flag_mode: bool,
    pub chan: Option<RtcDataChannel>,
    /// Race only: the opponent's board, folded from their half of the log, and
    /// whoever got home first. Both are `None` in the shared-board modes.
    foe: Option<Game>,
    winner: Option<u8>,
    /// When the move that ended this game was made, so the clock freezes
    /// there rather than at whatever arrived afterwards.
    ended_at: Option<u64>,
    /// Who made that move. In a lost flag duel this is the player who set
    /// the mine off — both lose, but the banner names the one who did it.
    ender: Option<u8>,
    ctx: Ctx,
    canvas: web_sys::HtmlCanvasElement,
}

/// When the clock started and, if the game is over, when it stopped — both
/// read out of the log, so every peer shows the same numbers no matter when
/// they joined.
///
/// It starts at the first reveal of the current board, which is when the game
/// really begins: mines are not placed until then. Only events after the last
/// `Start` count, because a restart leaves the old moves in the log.
///
/// `ended_at` is the timestamp of the move that actually finished the game.
/// It cannot be read off the end of the log: peers keep sending moves until
/// they hear the bad news, and those merge in behind the losing click.
pub fn clock_window(log: &[Stamped], ended_at: Option<u64>) -> (Option<u64>, Option<u64>) {
    let from = log
        .iter()
        .rposition(|s| matches!(s.ev, Event::Start { .. }))
        .map_or(0, |i| i + 1);
    let started = log[from..]
        .iter()
        .find(|s| matches!(s.ev, Event::Reveal { .. }))
        .map(|s| s.at_ms);
    (started, started.and(ended_at))
}

/// Whether a peer's report means the two of us have actually parted company.
///
/// Only comparable at the same point in the log: a different count means one
/// side is simply behind, which happens constantly and is not a fault. That
/// gate is also the failure worth guarding — anything that makes the counts
/// differ forever turns detection off for the session without a word.
pub fn is_desync(theirs_count: u32, theirs_hash: u64, ours_len: usize, ours_hash: u64) -> bool {
    theirs_count as usize == ours_len && theirs_hash != ours_hash
}

/// Which of two games survives when peers meet holding different ones.
///
/// Both sides run this over the same pair of logs, so they agree without
/// negotiating: a game with moves in it beats an untouched deal, and a tie is
/// broken by the deals themselves so the answer cannot depend on who asked.
pub fn theirs_survives(ours: &[Stamped], theirs: &[Stamped]) -> bool {
    let moves = |l: &[Stamped]| {
        l.iter()
            .filter(|s| !matches!(s.ev, Event::Start { .. }))
            .count()
    };
    match moves(theirs).cmp(&moves(ours)) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => theirs.first() < ours.first(),
    }
}

/// The seat left over once we take on somebody else's game: the one their
/// moves are not already using.
pub fn seat_in(log: &[Stamped], is_host: bool) -> u8 {
    let played = |p: u8| {
        log.iter().any(|s| match s.ev {
            Event::Reveal { player, .. } | Event::Flag { player, .. } => player == p,
            Event::Start { .. } => false,
        })
    };
    match (played(0), played(1)) {
        (true, false) => 1,
        (false, true) => 0,
        _ => u8::from(!is_host),
    }
}

/// Which seat we take on a connection.
///
/// Identity cannot be "whoever pressed Host is player 0": after a drop either
/// side may host, and a peer that changes seat mid-game takes its own past
/// moves with it — in a race its board and the opponent's swap wholesale, in a
/// flag race the two scores trade places.
pub fn seat(current: u8, is_host: bool, log: &[Stamped]) -> u8 {
    let ours = log.iter().any(|s| match s.ev {
        Event::Reveal { player, .. } | Event::Flag { player, .. } => player == current,
        Event::Start { .. } => false,
    });
    match (ours, is_host) {
        (true, _) => current,
        (false, true) => 0,
        (false, false) => 1,
    }
}

/// What to say once a game is decided, and which colour to say it in.
///
/// Pure, because the ways a game can end were otherwise reachable only from
/// a browser — and the solo ones not even there, since every end-to-end test
/// connects first.
///
/// `scores` is the duel's net flag score (right minus wrong, per player);
/// `ender` is who made the move that finished the game. Only the flag duel
/// reads them.
pub fn verdict(
    mode: Mode,
    status: Status,
    winner: Option<u8>,
    me: u8,
    scores: [i32; 2],
    solo: bool,
    ender: Option<u8>,
) -> (String, &'static str) {
    let done = |head: String, class| (format!("{head} — press New game"), class);
    match mode {
        // A race is decided by who got home first, not by the board in front
        // of you: you can finish and still have lost.
        Mode::Race => match winner {
            Some(w) if w == me => done("YOU WIN".into(), "win"),
            Some(_) if status == Status::Lost => {
                let head = if solo {
                    "BOOM"
                } else {
                    "BOOM — the race is theirs"
                };
                done(head.into(), "lose")
            }
            Some(_) => done("THEY GOT THERE FIRST".into(), "lose"),
            None => (String::new(), ""),
        },
        Mode::FlagRace => match status {
            // A mine ends it for both — nobody wins a duel that blew up.
            // The banner still says whose click it was.
            Status::Lost => {
                let head = match (solo, ender == Some(me)) {
                    (true, _) => "BOOM",
                    (_, true) => "BOOM — you set it off, nobody wins",
                    (_, false) => "BOOM — they set it off, nobody wins",
                };
                done(head.into(), "lose")
            }
            Status::Won => {
                let (mine, theirs) = (scores[me as usize], scores[1 - me as usize]);
                match (solo, mine.cmp(&theirs)) {
                    (true, _) => done(format!("cleared — flag score {mine}"), "win"),
                    (_, std::cmp::Ordering::Greater) => {
                        done(format!("YOU WIN {mine}–{theirs}"), "win")
                    }
                    (_, std::cmp::Ordering::Less) => {
                        done(format!("YOU LOSE {mine}–{theirs}"), "lose")
                    }
                    _ => done(format!("A DRAW {mine}–{theirs}"), "win"),
                }
            }
            Status::Playing => (String::new(), ""),
        },
        Mode::Coop => match status {
            Status::Won => done("YOU WIN".into(), "win"),
            Status::Lost => done("BOOM".into(), "lose"),
            Status::Playing => (String::new(), ""),
        },
    }
}

/// Whether the game has finished under the rules actually in play.
///
/// A race ends for *both* players the moment one of them is home, and the
/// loser's own board is still `Playing` at that point — so asking the board
/// alone keeps a decided race accepting moves.
pub fn is_over(status: Status, winner: Option<u8>) -> bool {
    winner.is_some() || status != Status::Playing
}

/// How much of a board is uncovered, as cells shown out of cells to show.
pub fn progress(b: &Board) -> (u32, u32) {
    let shown = b
        .cells()
        .iter()
        .filter(|c| c.state == Reveal::Shown)
        .count() as u32;
    // Saturating: a peer's board can claim more mines than it has cells, and
    // a HUD is not worth aborting the module over.
    let total = (b.cells().len() as u32).saturating_sub(b.mines as u32);
    (shown, total)
}

/// Flags outstanding: the classic counter, which counts flags rather than
/// mines and so goes negative if you over-flag.
pub fn mines_left(b: &Board) -> i32 {
    let flagged = (0..b.h)
        .flat_map(|y| (0..b.w).map(move |x| (x, y)))
        .filter(|&(x, y)| b.get(x, y).state == Reveal::Flagged)
        .count();
    b.mines as i32 - flagged as i32
}

pub type Shared = Rc<RefCell<App>>;

impl App {
    pub fn new(ctx: Ctx, canvas: web_sys::HtmlCanvasElement, seed: u64) -> Self {
        let (w, h, mines, _) = LEVELS[0];
        App {
            game: Game::new(seed, w, h, mines, Mode::Coop),
            log: eventlog::Log::open(
                Event::Start {
                    seed,
                    w,
                    h,
                    mines,
                    mode: Mode::Coop,
                },
                js_sys::Date::now() as u64,
            ),
            player: 0,
            flag_mode: false,
            chan: None,
            foe: None,
            winner: None,
            ended_at: None,
            ender: None,
            ctx,
            canvas,
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

    /// Folds the whole log again. A remote event can land anywhere in the
    /// order, including before moves we have already applied, so the board is
    /// rebuilt rather than patched.
    ///
    /// ponytail: O(log) per remote message. A few hundred events on a 30x16
    /// board is microseconds; if logs ever get long, keep a snapshot and
    /// replay only the tail.
    fn rebuild(&mut self) {
        let events: Vec<Event> = self.log.iter().map(|s| s.ev).collect();
        if mode_of(&self.log) == Mode::Race {
            let (mine, theirs, winner, ended) = race_fold(&events, self.player);
            self.game = mine;
            self.foe = Some(theirs);
            self.winner = winner;
            self.ended_at = ended.and_then(|i| self.log.get(i)).map(|s| s.at_ms);
            self.ender = None; // a race names its winner instead
            return;
        }
        self.foe = None;
        self.winner = None;
        if let Some((g, ended)) = Game::replay_marking_end(&events) {
            self.game = g;
            let ending = ended.and_then(|i| self.log.get(i));
            self.ended_at = ending.map(|s| s.at_ms);
            self.ender = ending.and_then(|s| match s.ev {
                Event::Reveal { player, .. } | Event::Flag { player, .. } => Some(player),
                Event::Start { .. } => None,
            });
        }
    }

    /// What the peer compares against. In the shared-board modes that is the
    /// board itself; in a race the two boards are *supposed* to differ, so the
    /// agreement worth checking is the log both sides hold.
    fn sync_hash(&self) -> u64 {
        if mode_of(&self.log) != Mode::Race {
            return self.game.hash();
        }
        engine::log_hash(&self.log)
    }

    /// Everything that has to happen after the board changes: repaint, and
    /// update the text above the board.
    fn refresh(&mut self) {
        self.draw();
        self.hud();
        self.banner();
    }

    /// Finished, under this mode's rules. Public because the click handler in
    /// `lib.rs` has to ask the same question the renderer and clock ask.
    pub fn over(&self) -> bool {
        is_over(self.game.status(), self.winner)
    }

    /// The result, in the page rather than painted over the board — it used to
    /// cover the middle row of cells, which are exactly the ones you want to
    /// look at when you have just lost.
    fn banner(&self) {
        let Some(el) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("banner"))
        else {
            return;
        };
        let (text, class) = verdict(
            mode_of(&self.log),
            self.game.status(),
            self.winner,
            self.player,
            self.game.flag_scores(),
            self.chan.is_none(),
            self.ender,
        );
        el.set_text_content(Some(&text));
        el.set_class_name(class);
    }

    /// Seconds on the clock: live while playing, frozen once it is over.
    ///
    /// While the game runs this is our own clock minus the author's, so two
    /// devices whose clocks disagree will disagree by that much. The final
    /// time is subtracted entirely out of the log, so it is the same number
    /// on both screens — and that is the one worth being right.
    fn seconds(&self) -> u32 {
        let (Some(start), stop) = clock_window(&self.log, self.ended_at) else {
            return 0;
        };
        let now = stop.unwrap_or_else(|| js_sys::Date::now() as u64);
        (now.saturating_sub(start) / 1000) as u32
    }

    fn hud(&self) {
        if let Some(el) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id("hud"))
        {
            // The colour only means anything once someone else is here.
            let me = match self.chan {
                Some(_) => format!(" · you are {}", PLAYERS[self.player as usize].1),
                None => String::new(),
            };
            let head = match mode_of(&self.log) {
                Mode::Coop => format!("{} flags left", mines_left(&self.game.board)),
                Mode::FlagRace => {
                    // Flags placed, not flags *right* — the true score is a
                    // mine detector until the board is cleared, so it waits
                    // for the banner.
                    let [red, blue] = self.game.flags_placed();
                    let left = self.game.board.mines as i32 - (red + blue) as i32;
                    format!("red {red} – {blue} blue · {left} mines unflagged")
                }
                Mode::Race => {
                    let (mine, total) = progress(&self.game.board);
                    let theirs = self.foe.as_ref().map_or(0, |g| progress(&g.board).0);
                    format!("you {mine}/{total} · them {theirs}/{total}")
                }
            };
            el.set_text_content(Some(&format!("{head} · {}s{me}", self.seconds())));
        }
    }

    /// Announces what our board looks like now. The peer compares it against
    /// its own and shouts if they differ — eight bytes instead of a board.
    fn send_state(&self) {
        self.send(&Msg::State {
            count: self.log.len() as u32,
            hash: self.sync_hash(),
        });
    }
}

/// A move made on this device.
pub fn local(app: &Shared, ev: Event) {
    let mut a = app.borrow_mut();
    // The log stamps and orders the move; the clock hazards — saturation, a
    // peer pinning us at u32::MAX — live in `eventlog::Log`.
    let s = a.log.append(ev, js_sys::Date::now() as u64);
    // Refold rather than patching the board with this one event: the fold is
    // the only thing that knows about a race's two boards and its verdict.
    a.rebuild();
    a.send(&Msg::Events(vec![s]));
    a.send_state();
    a.refresh();
}

/// Called once a second, so the clock moves between moves.
pub fn tick(app: &Shared) {
    app.borrow().hud();
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
            // A whole log opens with the *first* event of a game: a Start at
            // seq 0. That is someone handing over their game on connect — the
            // joiner takes it, the host never abandons its own for a peer's.
            //
            // A restart is also a Start, but it is a move, so its seq is at
            // least 1. Telling the two apart is what keeps the logs the same
            // length after a New game — and `Msg::State` only compares hashes
            // when the counts agree, so getting this wrong turns off desync
            // detection for the rest of the session.
            let ours = a.log.first().copied();
            let handover = events
                .first()
                .is_some_and(|s| is_foreign_start(s, ours.as_ref()));
            if handover {
                let Some(log) = eventlog::sanitised(events, MAX_LOG, opens) else {
                    return net::note("ignored a log that could not be a game");
                };
                if theirs_survives(&a.log, &log) {
                    net::log(&format!("adopted their game, {} events", log.len()));
                    // Their game, so their seats: we take the one their moves
                    // are not using. The seat our discarded log gave us no
                    // longer means anything.
                    a.player = seat_in(&log, a.player == 0);
                    a.log.adopt(log);
                } else {
                    net::log("kept our game — theirs had less in it");
                }
            } else if events.iter().any(|s| is_foreign_start(s, ours.as_ref())) {
                // A deal hidden behind a move: not a handover by position, so
                // it would have merged straight into our log and re-dealt the
                // board underneath us.
                return net::note("ignored a message trying to re-deal the board");
            } else {
                // Same game: this is either a move or a whole log arriving
                // after a reconnect. Merging handles both, and handles them
                // being partly or entirely things we already know.
                if eventlog::overflows(a.log.len(), events.len(), MAX_LOG) {
                    return net::note("ignored a message past the log cap");
                }
                let new = a.log.merge(&events);
                if events.len() > 1 {
                    net::log(&format!(
                        "merged {} events, {}",
                        events.len(),
                        if new { "some were new" } else { "all known" }
                    ));
                }
            }
            a.rebuild();
            a.refresh();
            // Answer with where that left us, so they can check too.
            a.send_state();
        }
        Msg::State { count, hash } => {
            let ours = a.sync_hash();
            if !is_desync(count, hash, a.log.len(), ours) {
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

/// Called when the channel opens — the first time, or again after a drop.
///
/// Both sides hand over the whole log. On a fresh join that is what settles
/// the seed; on a reconnect it is the catch-up, because merging two logs of
/// the same game is exactly "ship the missing tail" in both directions at
/// once. The log is small enough that asking which events are missing would
/// cost more than sending them.
pub fn on_connect(app: &Shared, is_host: bool) {
    let mut a = app.borrow_mut();
    a.player = seat(a.player, is_host, &a.log);
    let log = a.log.to_vec();
    net::log(&format!("sent log, {} events", log.len()));
    a.send(&Msg::Events(log));
    a.send_state();
    // Our colour is only known now, and the HUD announces it.
    a.refresh();
}

impl App {
    pub fn draw(&self) {
        let ctx = &self.ctx;
        let b = &self.game.board;
        let over = self.over();

        // The board can change size under us — a restart at another
        // difficulty, or the host's log arriving. Resizing the canvas also
        // wipes the transform, so the scale is reapplied every draw.
        let (bw, bh) = (
            (b.w as f64 * CELL * SCALE) as u32,
            (b.h as f64 * CELL * SCALE) as u32,
        );
        if self.canvas.width() != bw || self.canvas.height() != bh {
            self.canvas.set_width(bw);
            self.canvas.set_height(bh);
            // CSS sizes the board from these: how many cells across and down,
            // not how many pixels. Expert is 30 wide and would otherwise be
            // squeezed into the same width as a 9-wide Beginner board.
            if let Some(root) = web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.document_element())
                .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
            {
                let _ = root.style().set_property("--cols", &b.w.to_string());
                let _ = root.style().set_property("--rows", &b.h.to_string());
            }
        }
        let _ = ctx.set_transform(SCALE, 0.0, 0.0, SCALE, 0.0, 0.0);

        let who = presence(&self.log);

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
                    // The flag is whoever planted it, so a disagreement about
                    // a cell is visible rather than argued about.
                    // In a race this board is only ever yours, so the flags on
                    // it are too — colouring them by the log would paint your
                    // opponent's moves onto your own board.
                    let p = match mode_of(&self.log) {
                        Mode::Race => self.player,
                        _ => *who.owner.get(&(x, y)).unwrap_or(&0),
                    };
                    ctx.set_fill_style_str(PLAYERS[p as usize % PLAYERS.len()].0);
                    ctx.fill_rect(px + 11.0, py + 8.0, 11.0, 13.0);

                    // At the end, a flag on a safe cell is crossed out — the
                    // flags left standing on pink are the ones you got right.
                    if over && !c.mine {
                        ctx.set_stroke_style_str("#2b3038");
                        ctx.set_line_width(3.0);
                        ctx.begin_path();
                        ctx.move_to(px + 6.0, py + 6.0);
                        ctx.line_to(px + CELL - 8.0, py + CELL - 8.0);
                        ctx.move_to(px + CELL - 8.0, py + 6.0);
                        ctx.line_to(px + 6.0, py + CELL - 8.0);
                        ctx.stroke();
                    }
                } else if c.state == Reveal::Shown && !c.mine && c.adj > 0 {
                    ctx.set_fill_style_str(DIGITS[c.adj as usize]);
                    ctx.set_font("bold 20px monospace");
                    let _ = ctx.fill_text(&c.adj.to_string(), px + 10.0, py + 24.0);
                }
            }
        }

        // Where the other player last played — the closest thing to seeing
        // them without streaming a cursor over the channel. Not in a race:
        // their last move happened on their own board, so drawing it on
        // yours would be pointing at the wrong square.
        let show_peer = mode_of(&self.log) != Mode::Race;
        for (i, spot) in who.last.iter().enumerate().filter(|_| show_peer) {
            let Some(&(x, y)) = spot.as_ref() else {
                continue;
            };
            if i as u8 == self.player {
                continue;
            }
            ctx.set_stroke_style_str(PLAYERS[i % PLAYERS.len()].0);
            ctx.set_line_width(3.0);
            ctx.stroke_rect(
                x as f64 * CELL + 1.5,
                y as f64 * CELL + 1.5,
                CELL - 4.0,
                CELL - 4.0,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eventlog::merge;

    #[test]
    fn flags_left_counts_down_and_can_go_negative() {
        let mut g = Game::new(1, 9, 9, 10, Mode::Coop);
        assert_eq!(mines_left(&g.board), 10);

        // Eleven flags: a full row of nine, then two on the next row.
        for (x, y) in (0..9).map(|x| (x, 0)).chain((0..2).map(|x| (x, 1))) {
            g.apply(&Event::Flag { player: 0, x, y });
        }
        // Eleven flags on a ten-mine board: the counter is flags, not mines.
        assert_eq!(mines_left(&g.board), -1);
    }

    /// `at_ms` follows `seq` unless a test cares about the clock, in which
    /// case it uses `at`.
    fn stamp(seq: u32, ev: Event) -> Stamped {
        at(seq, ev, seq as u64)
    }

    fn at(seq: u32, ev: Event, at_ms: u64) -> Stamped {
        Stamped { seq, ev, at_ms }
    }

    fn start(seed: u64) -> Stamped {
        stamp(
            0,
            Event::Start {
                seed,
                w: 9,
                h: 9,
                mines: 10,
                mode: Mode::Coop,
            },
        )
    }

    #[test]
    fn presence_attributes_cells_and_remembers_each_last_move() {
        let log = [
            start(1),
            stamp(
                1,
                Event::Flag {
                    player: 0,
                    x: 1,
                    y: 1,
                },
            ),
            stamp(
                2,
                Event::Flag {
                    player: 1,
                    x: 2,
                    y: 2,
                },
            ),
            // Player 1 takes over a cell player 0 had.
            stamp(
                3,
                Event::Reveal {
                    player: 1,
                    x: 1,
                    y: 1,
                },
            ),
        ];
        let p = presence(&log);
        assert_eq!(p.owner.get(&(1, 1)), Some(&1));
        assert_eq!(p.owner.get(&(2, 2)), Some(&1));
        assert_eq!(p.last, [Some((1, 1)), Some((1, 1))]);

        // A restart mid-log wipes everything before it.
        let restarted = presence(&[log[1], start(2)]);
        assert!(restarted.owner.is_empty());
        assert_eq!(restarted.last, [None, None]);
    }

    /// The point of the whole stamping exercise: two peers who each make a
    /// move before hearing the other's end up with the same board. This is
    /// hazard 1 from the README — the opening move, which used to give the
    /// two of them different mine layouts.
    #[test]
    fn peers_converge_whatever_order_the_moves_arrive_in() {
        let seed = 0x5EED;
        let mine = stamp(
            1,
            Event::Reveal {
                player: 0,
                x: 0,
                y: 0,
            },
        );
        let theirs = stamp(
            1,
            Event::Reveal {
                player: 1,
                x: 8,
                y: 8,
            },
        );

        // Us: our move, then theirs arrives. Them: the mirror image.
        let mut ours = vec![start(seed), mine];
        merge(&mut ours, &[theirs]);
        let mut peer = vec![start(seed), theirs];
        merge(&mut peer, &[mine]);

        assert_eq!(ours, peer, "the logs must be the same sequence");
        let fold = |log: &[Stamped]| {
            Game::replay(&log.iter().map(|s| s.ev).collect::<Vec<_>>())
                .unwrap()
                .hash()
        };
        assert_eq!(fold(&ours), fold(&peer));
    }

    /// Reconnect: each side played on while the channel was down, then they
    /// swap whole logs. Nobody has to work out which events are missing.
    #[test]
    fn swapping_whole_logs_catches_both_sides_up() {
        let flag = |seq, player, x| stamp(seq, Event::Flag { player, x, y: 0 });
        let mut ours = vec![start(7), flag(1, 0, 1), flag(3, 0, 2)];
        let mut theirs = vec![start(7), flag(2, 1, 5)];

        let (ours_before, theirs_before) = (ours.clone(), theirs.clone());
        assert!(merge(&mut ours, &theirs_before));
        assert!(merge(&mut theirs, &ours_before));

        assert_eq!(ours, theirs);
        assert_eq!(ours.len(), 4, "three moves and the Start");
        // And the seq order, not arrival order, is what survives.
        assert!(ours.windows(2).all(|w| w[0] < w[1]));
    }

    /// The clock runs for the board in front of you, not for the log — and it
    /// stops at the move that ended the game, not at whatever arrived after.
    #[test]
    fn the_clock_is_read_from_the_log() {
        let reveal = at(
            1,
            Event::Reveal {
                player: 0,
                x: 1,
                y: 1,
            },
            10_000,
        );
        let ending = at(
            2,
            Event::Flag {
                player: 1,
                x: 5,
                y: 5,
            },
            42_000,
        );
        // A move made by a peer who had not yet heard the bad news.
        let straggler = at(
            3,
            Event::Flag {
                player: 1,
                x: 6,
                y: 6,
            },
            99_000,
        );

        // Nothing opened yet: stopped, and no start to count from.
        assert_eq!(clock_window(&[start(1)], None), (None, None));
        // Running: it began at the first reveal and has no end.
        assert_eq!(
            clock_window(&[start(1), reveal, ending], None),
            (Some(10_000), None)
        );
        // Over: the clock stops at the move that ended it — 32 seconds of
        // game — however many stragglers land behind it.
        assert_eq!(
            clock_window(&[start(1), reveal, ending, straggler], Some(42_000)),
            (Some(10_000), Some(42_000))
        );
        // New game: the old reveal is still in the log and must not count.
        assert_eq!(
            clock_window(&[start(1), reveal, at(3, START2.ev, 99_000)], None),
            (None, None)
        );
    }

    const START2: Stamped = Stamped {
        seq: 3,
        ev: Event::Start {
            seed: 2,
            w: 9,
            h: 9,
            mines: 10,
            mode: Mode::Coop,
        },
        at_ms: 0,
    };

    /// A restart is a move, not a new game being handed over. Merging it is
    /// what keeps both logs the same length — and the length is what decides
    /// whether the two hashes are compared at all.
    #[test]
    fn a_restart_is_a_move_not_a_whole_log() {
        let handshake = start(1); // seq 0: the opening of somebody's game
        let restart = at(
            4,
            Event::Start {
                seed: 99,
                w: 16,
                h: 16,
                mines: 40,
                mode: Mode::Coop,
            },
            5_000,
        );
        // Against the real predicate. This used to re-type the condition as a
        // local closure, so it passed no matter what `remote` actually did.
        let ours = Some(&handshake);
        assert!(
            !is_foreign_start(&handshake, ours),
            "our own game is not foreign"
        );
        assert!(!is_foreign_start(&restart, ours), "a restart is a move");
        assert!(
            is_foreign_start(&start(2), ours),
            "somebody else's deal is foreign"
        );

        // And merging it leaves both sides holding the same number of events.
        let mut ours = vec![
            handshake,
            stamp(
                1,
                Event::Reveal {
                    player: 0,
                    x: 1,
                    y: 1,
                },
            ),
        ];
        let mut theirs = ours.clone();
        merge(&mut ours, &[restart]);
        merge(&mut theirs, &[restart]);
        assert_eq!(ours.len(), 3);
        assert_eq!(ours, theirs);
    }

    /// Two different moves that share a seq are still two moves.
    #[test]
    fn the_same_seq_from_two_players_keeps_both_moves() {
        let mine = stamp(
            1,
            Event::Reveal {
                player: 0,
                x: 1,
                y: 1,
            },
        );
        let theirs = stamp(
            1,
            Event::Reveal {
                player: 1,
                x: 2,
                y: 2,
            },
        );
        let mut log = vec![start(1), mine];
        assert!(merge(&mut log, &[theirs]));
        assert_eq!(log.len(), 3);
        assert!(
            log.windows(2).all(|w| w[0] < w[1]),
            "the log came back unsorted"
        );
    }

    /// A board that claims more mines than it has cells is a peer's invention,
    /// not a crash: the HUD used to underflow and abort the module.
    #[test]
    fn progress_survives_a_board_with_more_mines_than_cells() {
        let b = Board::new(4, 4, 500);
        assert_eq!(progress(&b), (0, 0));
    }

    /// The opening predicate handed to `eventlog::sanitised`: only a deal
    /// opens a game, so a log that leads with a move is refused wholesale.
    #[test]
    fn only_a_deal_can_open_an_adopted_log() {
        let mv = at(
            1,
            Event::Reveal {
                player: 1,
                x: 2,
                y: 2,
            },
            10,
        );
        assert!(eventlog::sanitised(vec![start(1), mv], MAX_LOG, opens).is_some());
        assert!(
            eventlog::sanitised(vec![mv], MAX_LOG, opens).is_none(),
            "a log with no deal was accepted"
        );
    }

    /// Every difficulty has to survive the wire. Expert sits exactly on both
    /// dimension limits, so shrinking a bound — or adding a bigger level —
    /// would have the peer silently drop every message containing that deal,
    /// with no error and no DESYNC, because the counts would never match.
    #[test]
    fn every_difficulty_is_an_event_a_peer_will_accept() {
        for (w, h, mines, _) in LEVELS {
            let deal = Event::Start {
                seed: 1,
                w,
                h,
                mines,
                mode: Mode::Coop,
            };
            assert!(
                deal.is_playable(),
                "{w}x{h} with {mines} mines is unplayable"
            );
        }
    }

    /// Seats must survive a reconnect. After a drop either peer may host, and
    /// a player who changed seat would take their own past moves with them:
    /// in a race their board and the opponent's swap, in a flag race the two
    /// scores do. Nothing else in the protocol would notice — the logs stay
    /// identical, so the hashes agree while the two screens disagree.
    #[test]
    fn a_player_keeps_their_seat_across_a_role_reversal() {
        let played_by = |p: u8| {
            vec![
                start(1),
                stamp(
                    1,
                    Event::Reveal {
                        player: p,
                        x: 1,
                        y: 1,
                    },
                ),
            ]
        };

        // The host drops and comes back as the joiner: still player 0.
        assert_eq!(seat(0, false, &played_by(0)), 0);
        // The joiner hosts the reconnect: still player 1.
        assert_eq!(seat(1, true, &played_by(1)), 1);
        // Somebody with nothing at stake takes the seat the connection implies.
        assert_eq!(seat(0, false, &[start(1)]), 1);
        assert_eq!(seat(1, true, &[start(1)]), 0);
        // A deal alone is nobody's move, whoever dealt it.
        assert_eq!(seat(1, false, &played_by(0)), 1);
    }

    /// The consequence the seat rule exists to prevent, spelled out: flipping
    /// `me` swaps the two boards wholesale.
    #[test]
    fn a_race_folded_from_the_other_seat_swaps_the_boards() {
        let deal = at(
            0,
            Event::Start {
                seed: 5,
                w: 9,
                h: 9,
                mines: 10,
                mode: Mode::Race,
            },
            0,
        );
        let log = [
            deal,
            stamp(
                1,
                Event::Reveal {
                    player: 0,
                    x: 4,
                    y: 4,
                },
            ),
            stamp(
                2,
                Event::Reveal {
                    player: 1,
                    x: 0,
                    y: 0,
                },
            ),
        ];
        let events: Vec<Event> = log.iter().map(|s| s.ev).collect();

        let (mine0, theirs0, w0, _) = race_fold(&events, 0);
        let (mine1, theirs1, w1, _) = race_fold(&events, 1);
        assert_eq!(
            mine0.hash(),
            theirs1.hash(),
            "my board is not their 'theirs'"
        );
        assert_eq!(theirs0.hash(), mine1.hash());
        assert_eq!(w0, w1, "the two peers named different winners");
    }

    /// Whose game survives cannot depend on who pressed Host, or a newcomer
    /// hosting a reconnect wipes the game that was already in progress —
    /// which is exactly what the README promises never happens.
    #[test]
    fn the_game_with_moves_in_it_survives_a_meeting() {
        let played = vec![
            start(1),
            stamp(
                1,
                Event::Reveal {
                    player: 0,
                    x: 1,
                    y: 1,
                },
            ),
        ];
        let fresh = vec![start(2)];

        assert!(
            theirs_survives(&fresh, &played),
            "an untouched deal outranked a game"
        );
        assert!(
            !theirs_survives(&played, &fresh),
            "a game yielded to an untouched deal"
        );
        // Both sides must reach the same verdict about the same pair.
        assert_ne!(
            theirs_survives(&played, &fresh),
            theirs_survives(&fresh, &played)
        );
        // Two untouched deals still resolve, the same way for both peers.
        assert_ne!(
            theirs_survives(&[start(1)], &[start(2)]),
            theirs_survives(&[start(2)], &[start(1)])
        );

        // Taking on their game means taking the seat their moves left free.
        assert_eq!(seat_in(&played, true), 1);
        assert_eq!(seat_in(&fresh, true), 0);
        assert_eq!(seat_in(&fresh, false), 1);
    }

    /// The loudest failure path in the product, and the one nothing exercised:
    /// deleting the whole comparison used to leave every test green.
    #[test]
    fn a_desync_is_only_called_at_the_same_point_in_the_log() {
        // Same length, different boards: this is the real thing.
        assert!(is_desync(3, 0xAAAA, 3, 0xBBBB));
        // Same length, same board: silence.
        assert!(!is_desync(3, 0xAAAA, 3, 0xAAAA));
        // Behind is not broken — the counts differ constantly in normal play.
        assert!(!is_desync(2, 0xAAAA, 3, 0xBBBB));
        assert!(!is_desync(9, 0xAAAA, 3, 0xBBBB));
        // A count no log could reach must not panic on the cast.
        assert!(!is_desync(u32::MAX, 0xAAAA, 3, 0xBBBB));
    }

    /// Every way a game can end, none of them reachable from a native test
    /// before this was pulled out of the DOM write — and the solo ones not
    /// reachable from the browser tests either, since those always connect.
    #[test]
    fn every_ending_says_the_right_thing() {
        use Status::{Lost, Playing, Won};
        let (red, blue) = (0, 1);

        // Co-op: the board decides, and an unfinished game says nothing.
        let coop = |st| verdict(Mode::Coop, st, None, red, [0, 0], true, None).0;
        assert_eq!(coop(Playing), "");
        assert!(coop(Won).starts_with("YOU WIN"));
        assert!(coop(Lost).starts_with("BOOM"));

        // Flag duel, survived: the net flag score decides, from the reader's
        // own side — and it can be negative, wrong flags being -1.
        let score = |me, s: [i32; 2]| verdict(Mode::FlagRace, Won, None, me, s, false, None).0;
        assert!(score(red, [7, 3]).starts_with("YOU WIN 7–3"));
        assert!(score(blue, [7, 3]).starts_with("YOU LOSE 3–7"));
        assert!(score(red, [5, 5]).starts_with("A DRAW 5–5"));
        assert!(score(red, [-2, 1]).starts_with("YOU LOSE -2–1"));
        // Alone there is nobody to beat, but the score is still yours.
        assert!(
            verdict(Mode::FlagRace, Won, None, blue, [3, 7], true, None)
                .0
                .starts_with("cleared — flag score 7")
        );

        // Flag duel, blown up: both lose, and the clicker is named.
        let boom = |me, ender| verdict(Mode::FlagRace, Lost, None, me, [9, 0], false, ender);
        assert!(boom(red, Some(red)).0.starts_with("BOOM — you set it off"));
        assert!(
            boom(red, Some(blue))
                .0
                .starts_with("BOOM — they set it off")
        );
        // Nobody wins, however far ahead the scoreboard was.
        assert_eq!(boom(red, Some(blue)).1, "lose");
        assert_eq!(boom(blue, Some(red)).1, "lose");
        assert!(
            verdict(Mode::FlagRace, Lost, None, red, [0, 0], true, Some(red))
                .0
                .starts_with("BOOM — press"),
            "solo has nobody to blame"
        );

        // Race: the verdict decides, and losing has two flavours.
        let race = |w, st, solo| verdict(Mode::Race, st, w, red, [0, 0], solo, None).0;
        assert!(race(Some(red), Playing, false).starts_with("YOU WIN"));
        assert!(race(Some(blue), Playing, false).starts_with("THEY GOT THERE FIRST"));
        assert!(race(Some(blue), Lost, false).starts_with("BOOM — the race is theirs"));
        assert!(race(Some(blue), Lost, true).starts_with("BOOM — press"));
        assert_eq!(race(None, Playing, false), "");

        // Every ending tells you how to start another.
        for m in [Mode::Coop, Mode::FlagRace, Mode::Race] {
            let (text, class) = verdict(m, Won, Some(red), red, [1, 0], false, None);
            assert!(text.ends_with("press New game"), "{m:?}: {text}");
            assert!(!class.is_empty());
        }
    }
}
