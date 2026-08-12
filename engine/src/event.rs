/// Which game the two of them are playing. It rides on `Start` because it has
/// to be agreed before the first move, and `Start` is the one event both peers
/// are guaranteed to share.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
#[repr(u8)]
pub enum Mode {
    /// One board, cleared together. Hitting a mine ends it for both.
    #[default]
    Coop = 0,
    /// One board, but the mines are the prize: revealing one claims it for you
    /// instead of killing you, and whoever claims more wins.
    FlagRace = 1,
    /// Same seed, a board each, first to clear theirs wins.
    Race = 2,
}

impl Mode {
    pub fn from_u8(v: u8) -> Option<Mode> {
        match v {
            0 => Some(Mode::Coop),
            1 => Some(Mode::FlagRace),
            2 => Some(Mode::Race),
            // A mode we do not know is a peer from the future. Refusing the
            // event is better than silently playing a different game.
            _ => None,
        }
    }
}

/// A move. The event log is the game; board state is what you get by
/// folding these over a fresh board.
///
/// `player` is who made the move. The rules ignore it in co-op; the renderer
/// uses it to say who did what, the other modes score by it, and it is part of
/// the ordering key below.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Event {
    Start {
        seed: u64,
        w: u8,
        h: u8,
        mines: u16,
        mode: Mode,
    },
    Reveal {
        player: u8,
        x: u8,
        y: u8,
    },
    Flag {
        player: u8,
        x: u8,
        y: u8,
    },
}

impl Event {
    /// The widest board this game deals. A peer can put any `u8` in `w`/`h`,
    /// and 255x255 is a 1GB canvas and a fold slow enough to hang the tab, so
    /// the limit is part of what "a legal event" means rather than a UI rule.
    pub const MAX_W: u8 = 30;
    pub const MAX_H: u8 = 16;
    /// The first click and its neighbours are never mined, so a board has to
    /// hold that many cells over and above its mines.
    pub const SAFE_ZONE: usize = 9;
    /// Seats at the table. Anything else splits a race: an event that is "not
    /// me" on both peers lands on the opponent's board on both peers.
    pub const SEATS: u8 = 2;

    /// Whether this event describes a game that can actually be played.
    ///
    /// Every field arrives from a peer that may be buggy or hostile, and the
    /// engine is folded over these without further checks — a board with more
    /// mines than cells, or no cells at all, is refused here rather than
    /// trapped over later.
    pub fn is_playable(&self) -> bool {
        match *self {
            Event::Start { w, h, mines, .. } => {
                w >= 1
                    && h >= 1
                    && w <= Self::MAX_W
                    && h <= Self::MAX_H
                    // Room for the mines *and* the safe zone around the first
                    // click. Beyond that `place_mines` quietly lays fewer than
                    // asked, and a flag race then waits for prizes that were
                    // never dealt.
                    && mines as usize + Self::SAFE_ZONE <= w as usize * h as usize
            }
            Event::Reveal { player, .. } | Event::Flag { player, .. } => player < Self::SEATS,
        }
    }
}

/// An event with its place in the total order, and when it happened.
///
/// `seq` is a Lamport clock: one more than the highest any peer has been seen
/// to use. Two peers moving at the same time pick the same `seq`, so the
/// derived `Ord` breaks the tie on the event itself — variant, then player,
/// then coordinates. Both sides sort by the same key and therefore fold the
/// same log, which is what makes the ordering hazards impossible rather than
/// merely rare.
///
/// `at_ms` is the author's wall clock, in Unix milliseconds. It is deliberately
/// last in the field order — and therefore last in the derived `Ord` — because
/// it must never decide the order of two events. It is there so the game clock
/// can be read out of the log instead of measured locally, which is what makes
/// a peer who joins in the middle show the same elapsed time as everyone else.
///
/// Field order is the sort order. Do not reorder it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Stamped {
    pub seq: u32,
    pub ev: Event,
    pub at_ms: u64,
}
