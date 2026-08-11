/// A move. The event log is the game; board state is what you get by
/// folding these over a fresh board.
///
/// `player` is unused in single-player. It is here now because adding a
/// field to a wire format later means versioning the wire format later.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    Start { seed: u64, w: u8, h: u8, mines: u16 },
    Reveal { player: u8, x: u8, y: u8 },
    Flag { player: u8, x: u8, y: u8 },
}
