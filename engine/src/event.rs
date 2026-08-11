/// A move. The event log is the game; board state is what you get by
/// folding these over a fresh board.
///
/// `player` is who made the move. The rules ignore it; the renderer uses it to
/// say who did what, and it is part of the ordering key below.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Event {
    Start { seed: u64, w: u8, h: u8, mines: u16 },
    Reveal { player: u8, x: u8, y: u8 },
    Flag { player: u8, x: u8, y: u8 },
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
