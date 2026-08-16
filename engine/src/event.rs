/// Which game the two of them are playing. It rides on `Start` because it has
/// to be agreed before the first move, and `Start` is the one event both peers
/// are guaranteed to share.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
#[repr(u8)]
pub enum Mode {
    /// One board, cleared together. Hitting a mine ends it for both.
    #[default]
    Coop = 0,
    /// One board, survived together, but the flags are the score: a standing
    /// flag on a mine is +1, on a safe cell -1, and whoever nets more when
    /// the board is cleared wins. A revealed mine still ends it for both.
    FlagRace = 1,
    /// Same seed, a board each, first to clear theirs wins.
    Race = 2,
}

impl Mode {
    /// Every mode, in wire order. The picker is built from this, so the list
    /// the player sees and the byte on the wire cannot drift apart — they used
    /// to be two hand-kept orders with a comment between them.
    pub const ALL: [Mode; 3] = [Mode::Coop, Mode::FlagRace, Mode::Race];

    /// A mode we do not know is a peer from the future. Refusing the event is
    /// better than silently playing a different game.
    pub fn from_u8(v: u8) -> Option<Mode> {
        Self::ALL.get(v as usize).copied()
    }

    /// What the picker calls it. Exhaustive on purpose: a new mode fails to
    /// compile here rather than quietly appearing as an unnamed option.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Coop => "Co-op — clear it together",
            Mode::FlagRace => "Flag duel — survive together, best flags win",
            Mode::Race => "Race — same deal, a board each",
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

/// A move with its place in the total order — see `eventlog::Stamped` for
/// why `seq` outranks the event and the event outranks `at_ms`. Two peers
/// moving at the same time pick the same `seq`, so the derived `Ord` on
/// `Event` breaks the tie — variant, then player, then coordinates.
pub type Stamped = eventlog::Stamped<Event>;

#[cfg(test)]
mod mode_table {
    use super::*;

    /// The picker is built from `ALL`, and the wire reads the same index back.
    /// The two used to be hand-kept orders with a comment between them, so
    /// reordering an option silently changed the game everyone played.
    #[test]
    fn every_mode_round_trips_its_own_index() {
        for (i, mode) in Mode::ALL.into_iter().enumerate() {
            assert_eq!(mode as u8 as usize, i, "{mode:?} is not at its own index");
            assert_eq!(Mode::from_u8(i as u8), Some(mode));
            assert!(!mode.label().is_empty());
        }
        assert_eq!(Mode::from_u8(Mode::ALL.len() as u8), None);
        assert_eq!(Mode::from_u8(255), None);
    }

    #[test]
    fn the_labels_are_distinct() {
        let mut seen: Vec<&str> = Mode::ALL.iter().map(|m| m.label()).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), before, "two modes share a label");
    }
}
