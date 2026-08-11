use crate::{Board, Event, Status};

pub struct Game {
    pub board: Board,
    seed: u64,
    placed: bool,
}

impl Game {
    pub fn new(seed: u64, w: u8, h: u8, mines: u16) -> Self {
        Game {
            board: Board::new(w, h, mines),
            seed,
            placed: false,
        }
    }

    pub fn apply(&mut self, ev: &Event) {
        // Start is checked before the status guard below: restarting is
        // exactly what you do from a finished game.
        if let Event::Start { seed, w, h, mines } = *ev {
            *self = Game::new(seed, w, h, mines);
            return;
        }
        // A finished game ignores moves. Peers may still have some in flight
        // when someone hits a mine; this makes that harmless.
        if self.board.status() != Status::Playing {
            return;
        }
        match *ev {
            Event::Start { .. } => unreachable!("handled above"),
            Event::Reveal { x, y, .. } => {
                if !self.board.in_bounds(x as i32, y as i32) {
                    return; // a peer sent nonsense; drop it, don't panic
                }
                // The board does not exist until the first click, so that
                // click can be guaranteed safe. Both peers derive the same
                // layout because both have this event in their log.
                if !self.placed {
                    self.board.place_mines(self.seed, (x, y));
                    self.placed = true;
                }
                self.board.reveal(x, y);
            }
            Event::Flag { x, y, .. } => {
                if self.board.in_bounds(x as i32, y as i32) {
                    self.board.toggle_flag(x, y);
                }
            }
        }
    }

    pub fn status(&self) -> Status {
        self.board.status()
    }

    pub fn replay(events: &[Event]) -> Option<Game> {
        let Event::Start { seed, w, h, mines } = *events.first()? else {
            return None;
        };
        let mut g = Game::new(seed, w, h, mines);
        for ev in &events[1..] {
            g.apply(ev);
        }
        Some(g)
    }

    /// FNV-1a over every cell. Two peers compare this after each event and
    /// detect divergence in 8 bytes instead of shipping the whole board.
    ///
    /// Bit budget per cell: mine 1, adj 4 (0..=8), state 2 (0..=2) = 7.
    pub fn hash(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for c in self.board.cells() {
            let byte = (c.mine as u8) | (c.adj << 1) | ((c.state as u8) << 5);
            h ^= byte as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn script(seed: u64) -> Vec<Event> {
        vec![
            Event::Start {
                seed,
                w: 9,
                h: 9,
                mines: 10,
            },
            Event::Reveal {
                player: 0,
                x: 4,
                y: 4,
            },
            Event::Flag {
                player: 1,
                x: 0,
                y: 0,
            },
            Event::Reveal {
                player: 1,
                x: 8,
                y: 8,
            },
            Event::Flag {
                player: 0,
                x: 0,
                y: 0,
            }, // un-flag
        ]
    }

    #[test]
    fn replay_is_deterministic() {
        let a = Game::replay(&script(7)).unwrap();
        let b = Game::replay(&script(7)).unwrap();
        assert_eq!(a.hash(), b.hash());
    }

    #[test]
    fn replay_needs_a_start_event() {
        assert!(Game::replay(&[]).is_none());
        assert!(Game::replay(&[Event::Reveal {
            player: 0,
            x: 0,
            y: 0
        }])
        .is_none());
    }

    #[test]
    fn hash_reacts_to_every_move() {
        let mut g = Game::new(7, 9, 9, 10);
        let mut seen = vec![g.hash()];
        for ev in &script(7)[1..] {
            g.apply(ev);
            seen.push(g.hash());
        }
        assert_ne!(seen[0], seen[1], "revealing changed nothing in the hash");
        assert_ne!(seen[1], seen[2], "flagging changed nothing in the hash");
    }

    #[test]
    fn flag_then_unflag_restores_the_hash() {
        let mut g = Game::new(7, 9, 9, 10);
        let before = g.hash();
        g.apply(&Event::Flag {
            player: 0,
            x: 0,
            y: 0,
        });
        assert_ne!(before, g.hash(), "flagging is invisible to the hash");
        g.apply(&Event::Flag {
            player: 0,
            x: 0,
            y: 0,
        });
        assert_eq!(before, g.hash(), "un-flagging did not restore the state");
    }

    #[test]
    fn out_of_bounds_events_are_ignored_not_fatal() {
        let mut g = Game::new(7, 9, 9, 10);
        let before = g.hash();
        g.apply(&Event::Reveal {
            player: 9,
            x: 200,
            y: 200,
        });
        assert_eq!(g.hash(), before);
    }

    #[test]
    fn moves_after_the_game_ends_are_ignored() {
        let mut g = Game::new(7, 9, 9, 10);
        g.apply(&Event::Reveal {
            player: 0,
            x: 4,
            y: 4,
        });
        // Open every mine's neighbour-free path by brute force until we lose.
        let mine = (0..9u8)
            .flat_map(|y| (0..9u8).map(move |x| (x, y)))
            .find(|&(x, y)| g.board.get(x, y).mine)
            .unwrap();
        g.apply(&Event::Reveal {
            player: 0,
            x: mine.0,
            y: mine.1,
        });
        assert_eq!(g.status(), Status::Lost);

        let after_loss = g.hash();
        g.apply(&Event::Flag {
            player: 1,
            x: 0,
            y: 0,
        });
        assert_eq!(
            g.hash(),
            after_loss,
            "a finished game still accepted a move"
        );

        // ...but Start must still get through, or you can never play again.
        g.apply(&Event::Start {
            seed: 8,
            w: 9,
            h: 9,
            mines: 10,
        });
        assert_eq!(
            g.status(),
            Status::Playing,
            "could not restart after losing"
        );
    }

    proptest! {
        /// The property that makes peer-to-peer safe: same log in, same hash out.
        #[test]
        fn identical_logs_agree(
            seed: u64,
            moves in prop::collection::vec((0u8..9, 0u8..9), 0..40),
        ) {
            let mut log = vec![Event::Start { seed, w: 9, h: 9, mines: 10 }];
            for (i, &(x, y)) in moves.iter().enumerate() {
                log.push(if i % 3 == 0 {
                    Event::Flag { player: 0, x, y }
                } else {
                    Event::Reveal { player: 0, x, y }
                });
            }
            let a = Game::replay(&log).unwrap();
            let b = Game::replay(&log).unwrap();
            prop_assert_eq!(a.hash(), b.hash());
        }
    }
}
